//! `#[derive(WaveWire)]` — emit a [`WaveWire`] impl, the companion proc-macro of the
//! [`wavedb-wire`] crate (re-exported as `wavedb_wire::WaveWire`).
//!
//! It generates everything through absolute `::wavedb_wire::` paths, so it works
//! from any crate that depends on `wavedb-wire` (including `wavedb-wire` itself via
//! `extern crate self as wavedb_wire`).
//!
//! Supported shapes:
//!
//! - **structs** (named, tuple, unit): field stack slots are emitted inline in
//!   declaration order; heap payloads append depth-first to the shared heap.
//! - **enums**: the canonical tag form. If every variant is field-less the value
//!   is a single `u8` tag; if any variant carries fields, the stack slot is
//!   `tag (u8) + payload-length (u32)` and the active variant's fields are written
//!   as a self-contained unit in the heap (all field stacks, then all field
//!   heaps), tagged by declaration order. Mirrors `docs/wire_format.md`.
//!
//! Unions are rejected.
//!
//! [`WaveWire`]: https://docs.rs/wavedb-wire
//! [`wavedb-wire`]: https://docs.rs/wavedb-wire

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Data, DataStruct, DeriveInput, Fields, Index, parse_macro_input,
    spanned::Spanned,
};

mod enums;

use enums::expand_enum;

/// Derive [`WaveWire`](wavedb_wire::WaveWire) for a struct or enum.
#[proc_macro_derive(WaveWire)]
pub fn wave_wire(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    match &input.data {
        Data::Struct(data) => Ok(expand_struct(input, data)),
        Data::Enum(data) => expand_enum(input, data),
        Data::Union(_) => Err(syn::Error::new(
            input.span(),
            "WaveWire cannot be derived for unions",
        )),
    }
}

/// Wrap the four `WaveWire` method bodies in the trait impl for `input`.
pub(crate) fn wire_impl(
    input: &DeriveInput,
    stack_size: &TokenStream2,
    heap_size: &TokenStream2,
    encode_stack: &TokenStream2,
    encode_heap: &TokenStream2,
    decode: &TokenStream2,
) -> TokenStream2 {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) =
        input.generics.split_for_impl();
    quote! {
        impl #impl_generics ::wavedb_wire::WaveWire for #name #ty_generics #where_clause {
            const STACK_SIZE: usize = #stack_size;

            fn heap_size(&self) -> usize { #heap_size }

            fn encode_stack(&self, stack: &mut ::std::vec::Vec<u8>) { #encode_stack }

            fn encode_heap(&self, heap: &mut ::std::vec::Vec<u8>) { #encode_heap }

            fn decode(
                stack: &mut ::wavedb_wire::Cursor,
                heap: &mut ::wavedb_wire::Cursor,
            ) -> ::wavedb_wire::Result<Self> {
                #decode
            }
        }
    }
}

// ---- structs ----------------------------------------------------------------

fn expand_struct(input: &DeriveInput, data: &DataStruct) -> TokenStream2 {
    // Per field: its type (for STACK_SIZE / decode) and its accessor (`self.ident`
    // or `self.0`). Decode also needs the binding target to rebuild `Self`.
    let mut field_types = Vec::new();
    let mut accessors = Vec::new();

    let decode_body = match &data.fields {
        Fields::Named(named) => {
            let decoders: Vec<_> = named
                .named
                .iter()
                .map(|f| {
                    let ident = f.ident.as_ref().expect("named field has ident");
                    let ty = &f.ty;
                    field_types.push(ty.clone());
                    accessors.push(quote!(self.#ident));
                    quote!(#ident: <#ty as ::wavedb_wire::WaveWire>::decode(stack, heap)?)
                })
                .collect();
            quote!(::core::result::Result::Ok(Self { #(#decoders,)* }))
        }
        Fields::Unnamed(unnamed) => {
            let decoders: Vec<_> = unnamed
                .unnamed
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let idx = Index::from(i);
                    let ty = &f.ty;
                    field_types.push(ty.clone());
                    accessors.push(quote!(self.#idx));
                    quote!(<#ty as ::wavedb_wire::WaveWire>::decode(stack, heap)?)
                })
                .collect();
            quote!(::core::result::Result::Ok(Self( #(#decoders,)* )))
        }
        Fields::Unit => quote!(::core::result::Result::Ok(Self)),
    };

    let stack_size =
        quote!(0 #( + <#field_types as ::wavedb_wire::WaveWire>::STACK_SIZE )*);
    let heap_size =
        quote!(0 #( + ::wavedb_wire::WaveWire::heap_size(&#accessors) )*);
    let encode_stack = quote!(#( ::wavedb_wire::WaveWire::encode_stack(&#accessors, stack); )*);
    let encode_heap =
        quote!(#( ::wavedb_wire::WaveWire::encode_heap(&#accessors, heap); )*);

    wire_impl(
        input,
        &stack_size,
        &heap_size,
        &encode_stack,
        &encode_heap,
        &decode_body,
    )
}
