//! The `#[wavedb]` attribute expansion.
//!
//! Turns a plain struct into a WaveDB object. For every struct it emits:
//!
//! - the original struct (with `#[wavedb::pivot(...)]` helper attributes stripped),
//! - a [`Wire`] impl over the declared fields,
//! - inherent `STRUCT_HASH` / `SHAPE` / `HAS_VALIDATE` / `HAS_PREPROCESS` consts,
//! - a [`WaveDbStruct`] impl tying identity, shape, and the `PivotId` type together.
//!
//! For a `NonUnique` struct it additionally emits the generated `{Name}PivotId` and
//! `{Name}Pivot` types (see [`crate::generated`]).
//!
//! [`Wire`]: wavedb_core::wire::Wire
//! [`WaveDbStruct`]: wavedb_core::traits::WaveDbStruct

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::{Attribute, Data, DeriveInput, Fields, Ident};

use crate::args::{Shape, WavedbArgs};
use crate::{generated, struct_hash, wire_derive};

/// Expand `#[wavedb(<attr>)] <item>`.
pub fn expand(
    attr: TokenStream,
    item: TokenStream,
) -> syn::Result<TokenStream> {
    let args = Parser::parse2(WavedbArgs::parse, attr)?;
    let mut input: DeriveInput = syn::parse2(item)?;

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input,
            "#[wavedb] can only be applied to structs",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &input,
            "#[wavedb] structs must have named fields",
        ));
    };

    // Field (name, normalised-type) pairs feed the STRUCT_HASH.
    let hash_fields: Vec<(String, String)> = named
        .named
        .iter()
        .map(|f| {
            let name = f.ident.as_ref().expect("named field").to_string();
            (name, normalise_type(&f.ty))
        })
        .collect();

    let name = input.ident.clone();
    let hash = struct_hash::compute(
        &name.to_string(),
        args.shape.as_str(),
        &hash_fields,
    );

    // Strip `#[wavedb::pivot(...)]` helper attributes; each is one secondary index.
    let num_secondaries = strip_pivot_attrs(&mut input.attrs);
    if num_secondaries > 0 && args.shape != Shape::NonUnique {
        return Err(syn::Error::new(
            Span::call_site(),
            "#[wavedb::pivot(...)] is only valid on a #[wavedb(NonUnique)] struct",
        ));
    }

    let wire_impl = wire_derive::derive(&input)?;
    let shape_variant =
        Ident::new(args.shape.core_variant(), Span::call_site());
    let has_validate = args.validate.is_some();
    let has_preprocess = args.preprocess.is_some();

    // The PivotId associated type: () for Unique, the generated newtype otherwise.
    let (pivot_id_ty, generated_types) = match args.shape {
        Shape::Unique => (quote!(()), TokenStream::new()),
        Shape::NonUnique => {
            let pivot_id = format_ident!("{}PivotId", name);
            let types = generated::nonunique_types(&name, num_secondaries)?;
            (quote!(#pivot_id), types)
        }
    };

    Ok(quote! {
        #input

        impl #name {
            /// Compile-time identity of this type and its schema generation.
            pub const STRUCT_HASH: u64 = #hash;
            /// This type's cardinality discipline.
            pub const SHAPE: ::wavedb_core::Shape = ::wavedb_core::Shape::#shape_variant;
            /// Whether a `validate` hook is declared.
            pub const HAS_VALIDATE: bool = #has_validate;
            /// Whether a `preprocess` hook is declared.
            pub const HAS_PREPROCESS: bool = #has_preprocess;
        }

        #wire_impl

        impl ::wavedb_core::WaveDbStruct for #name {
            const STRUCT_HASH: u64 = #hash;
            const SHAPE: ::wavedb_core::Shape = ::wavedb_core::Shape::#shape_variant;
            type PivotId = #pivot_id_ty;
        }

        #generated_types
    })
}

/// Remove every `#[wavedb::pivot(...)]` attribute from `attrs`, returning how many
/// were found (the secondary-index count).
fn strip_pivot_attrs(attrs: &mut Vec<Attribute>) -> usize {
    let before = attrs.len();
    attrs.retain(|a| !is_pivot_attr(a));
    before - attrs.len()
}

/// `true` for a `#[wavedb::pivot(...)]` helper attribute.
fn is_pivot_attr(attr: &Attribute) -> bool {
    let segs = &attr.path().segments;
    segs.len() == 2 && segs[0].ident == "wavedb" && segs[1].ident == "pivot"
}

/// A whitespace-free rendering of a field type, so the same declared type always
/// hashes identically (`Vec < String >` → `Vec<String>`).
fn normalise_type(ty: &syn::Type) -> String {
    quote!(#ty).to_string().split_whitespace().collect()
}
