//! The `#[wavedb]` attribute expansion.
//!
//! Turns a plain struct into a WaveDB object. For every struct it emits:
//!
//! - the original struct (with `#[wavedb::pivot(...)]` helper attributes stripped),
//! - a [`WaveWire`] impl over the declared fields,
//! - inherent `STRUCT_HASH` / `SHAPE` / `HAS_VALIDATE` / `HAS_PREPROCESS` consts,
//! - a [`WaveDbStruct`] impl tying identity, shape, and the `PivotId` type together.
//!
//! For a `NonUnique` struct it additionally emits the generated `{Name}PivotId` and
//! `{Name}Pivot` types (see [`crate::generated`]).
//!
//! [`WaveWire`]: wavedb_wire::WaveWire
//! [`WaveDbStruct`]: wavedb_core::traits::WaveDbStruct

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::{Attribute, Data, DeriveInput, Fields, Ident};

use crate::args::{PivotSpec, Shape, WavedbArgs};
use crate::secondaries::ResolvedPivot;
use crate::{exec_ops, generated, storage_statics, struct_hash, wire_derive};

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

    // Take `#[wavedb::pivot(...)]` helper attributes; each is one secondary
    // index, its fields resolved (and validated) against the struct's own.
    let pivot_specs = take_pivot_specs(&mut input.attrs)?;
    let secondaries = resolve_pivot_fields(&pivot_specs, named)?;
    if !secondaries.is_empty() && args.shape != Shape::NonUnique {
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
    // Unique types get the anchor ops (`get`/`save`); NonUnique types get their
    // collection machinery from `generated::nonunique_types`.
    let (pivot_id_ty, generated_types) = match args.shape {
        Shape::Unique => (quote!(()), unique_ops(&name)),
        Shape::NonUnique => {
            let pivot_id = format_ident!("{}PivotId", name);
            let types = generated::nonunique_types(&name, &secondaries)?;
            (quote!(#pivot_id), types)
        }
    };

    // Native-only compile-time storage: the type's own StructStorage static.
    // The NonUnique variant's Pivot slot and `storage_entries()` are emitted
    // with the pivot types in `generated::nonunique_types`.
    let struct_hash_expr =
        quote!(<#name as ::wavedb_core::WaveDbStruct>::STRUCT_HASH);
    let storage_slot =
        storage_statics::statics_for(&name, &struct_hash_expr, args.compress);
    let storage_entries = match args.shape {
        Shape::Unique => storage_statics::entries_for(&name, None),
        Shape::NonUnique => TokenStream::new(),
    };

    // The per-command execution steps (`__wavedb_<op>`): defined on every
    // item, wire-reachable only once listed in an exposure declaration.
    // NonUnique steps need the generated PivotId type, so they emit with it
    // in `generated::nonunique_types`.
    let exec_steps = match args.shape {
        Shape::Unique => exec_ops::unique_ops(&name),
        Shape::NonUnique => TokenStream::new(),
    };

    // The shape marker trait — `UniqueStruct` for the default shape (the
    // NonUnique marker is emitted with the collection types). Client typed
    // surfaces gate on these so `Unique` and `NonUnique` ops never overlap.
    let shape_marker = match args.shape {
        Shape::Unique => quote!(impl ::wavedb_core::UniqueStruct for #name {}),
        Shape::NonUnique => TokenStream::new(),
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

        // As a `#[server]` signature type this struct tags as its own
        // schema identity — evolving it transitively renames every function
        // whose signature carries it.
        impl ::wavedb_core::FnArgTag for #name {
            const TAG: u64 = #hash;
        }

        #shape_marker
        #storage_slot
        #storage_entries
        #exec_steps

        #generated_types
    })
}

/// The `Unique` anchor ops: `get` / `save` / `history` inherent fns over any
/// [`DbHandle`](wavedb_core::DbHandle) — the same spelling resolves against a
/// `LocalHandle`, the client `Db`, and a node-side `ServerDb`. `save` **is**
/// the upsert — a `Unique` type has no separate create.
fn unique_ops(name: &Ident) -> TokenStream {
    quote! {
        impl #name {
            /// Fetch this tenant's record from its `STRUCT_HASH` anchor.
            /// `None` = never saved.
            ///
            /// # Errors
            /// The context's failure (backend/transport) or a decode fault.
            #[allow(clippy::future_not_send)]
            pub async fn get<D: ::wavedb_core::DbHandle>(
                db: &D,
            ) -> ::core::result::Result<::core::option::Option<Self>, D::Error>
            {
                db.get_unique::<Self>().await
            }

            /// Save (insert-or-overwrite) this tenant's record at its anchor.
            /// A save over an existing record archives the superseded version
            /// — the timeline stays walkable via [`history`](Self::history).
            ///
            /// # Errors
            /// The context's failure (backend/transport).
            #[allow(clippy::future_not_send)]
            pub async fn save<D: ::wavedb_core::DbHandle>(
                &self,
                db: &D,
            ) -> ::core::result::Result<(), D::Error> {
                db.save_unique(self).await
            }

            /// Stream this tenant's record versions **newest-first** (the
            /// live record, then each archived version along the
            /// modification chain). Empty when never saved.
            pub fn history<D: ::wavedb_core::DbHandle>(
                db: &D,
            ) -> impl ::wavedb_core::Stream<
                Item = ::core::result::Result<
                    (::wavedb_core::Metadata, Self),
                    D::Error,
                >,
            > {
                db.unique_history::<Self>()
            }
        }
    }
}

/// Remove every `#[wavedb::pivot(...)]` attribute from `attrs`, parsing each
/// into the fields it declares (declaration order preserved).
fn take_pivot_specs(attrs: &mut Vec<Attribute>) -> syn::Result<Vec<PivotSpec>> {
    let mut specs = Vec::new();
    let mut kept = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        if is_pivot_attr(&attr) {
            specs.push(attr.parse_args::<PivotSpec>()?);
        } else {
            kept.push(attr);
        }
    }
    *attrs = kept;
    Ok(specs)
}

/// Resolve each declared pivot field against the struct's named fields,
/// pairing it with its type — an unknown field is a compile error at the
/// declaration site.
fn resolve_pivot_fields(
    specs: &[PivotSpec],
    named: &syn::FieldsNamed,
) -> syn::Result<Vec<ResolvedPivot>> {
    specs
        .iter()
        .map(|spec| {
            let fields = spec
                .fields
                .iter()
                .map(|ident| {
                    named
                        .named
                        .iter()
                        .find(|f| f.ident.as_ref() == Some(ident))
                        .map(|f| (ident.clone(), f.ty.clone()))
                        .ok_or_else(|| {
                            syn::Error::new_spanned(
                                ident,
                                "#[wavedb::pivot(...)] names a field this \
                                 struct does not declare",
                            )
                        })
                })
                .collect::<syn::Result<Vec<_>>>()?;
            Ok(ResolvedPivot { fields })
        })
        .collect()
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
