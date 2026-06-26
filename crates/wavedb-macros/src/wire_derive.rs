//! `#[derive(WaveWire)]` — emit a [`Wire`] impl for a struct.
//!
//! Field stack slots are encoded inline in declaration order; heap payloads append
//! depth-first to the shared heap section. Decode reads the same field order. The
//! generated impl names everything through absolute `::wavedb_core::` paths so it
//! works from any crate (including `wavedb-core` itself via `extern crate self`).
//!
//! Supports named-field, tuple, and unit structs. Enums and unions are rejected
//! with a diagnostic (enum wire layout is emitted by `#[wavedb]`, not the derive).
//!
//! [`Wire`]: wavedb_core::wire::Wire

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Index, spanned::Spanned};

/// Expand `#[derive(WaveWire)]` for `input`.
pub fn derive(input: &DeriveInput) -> syn::Result<TokenStream> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "WaveWire can only be derived for structs",
        ));
    };

    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) =
        input.generics.split_for_impl();

    // Per field: its type (for STACK_SIZE / decode) and its accessor (`self.ident`
    // or `self.0`). Decode also needs the binding target to rebuild `Self`.
    let mut field_types = Vec::new();
    let mut accessors = Vec::new();

    let decode_body = match &data.fields {
        Fields::Named(named) => {
            let mut decoders = Vec::new();
            for f in &named.named {
                let ident = f.ident.as_ref().expect("named field has ident");
                let ty = &f.ty;
                field_types.push(ty.clone());
                accessors.push(quote!(self.#ident));
                decoders.push(quote! {
                    #ident: <#ty as ::wavedb_core::Wire>::decode(stack, heap)?
                });
            }
            quote!(Ok(Self { #(#decoders,)* }))
        }
        Fields::Unnamed(unnamed) => {
            let mut decoders = Vec::new();
            for (i, f) in unnamed.unnamed.iter().enumerate() {
                let idx = Index::from(i);
                let ty = &f.ty;
                field_types.push(ty.clone());
                accessors.push(quote!(self.#idx));
                decoders.push(quote! {
                    <#ty as ::wavedb_core::Wire>::decode(stack, heap)?
                });
            }
            quote!(Ok(Self( #(#decoders,)* )))
        }
        Fields::Unit => quote!(Ok(Self)),
    };

    Ok(quote! {
        impl #impl_generics ::wavedb_core::Wire for #name #ty_generics #where_clause {
            const STACK_SIZE: usize =
                0 #( + <#field_types as ::wavedb_core::Wire>::STACK_SIZE )*;

            fn heap_size(&self) -> usize {
                0 #( + ::wavedb_core::Wire::heap_size(&#accessors) )*
            }

            fn encode_stack(&self, stack: &mut ::std::vec::Vec<u8>) {
                #( ::wavedb_core::Wire::encode_stack(&#accessors, stack); )*
            }

            fn encode_heap(&self, heap: &mut ::std::vec::Vec<u8>) {
                #( ::wavedb_core::Wire::encode_heap(&#accessors, heap); )*
            }

            fn decode(
                stack: &mut ::wavedb_core::Cursor,
                heap: &mut ::wavedb_core::Cursor,
            ) -> ::wavedb_core::Result<Self> {
                #decode_body
            }
        }
    })
}
