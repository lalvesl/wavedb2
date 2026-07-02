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
use quote::{format_ident, quote};
use syn::{
    Data, DataEnum, DataStruct, DeriveInput, Fields, Index, parse_macro_input,
    spanned::Spanned,
};

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
fn wire_impl(
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

// ---- enums ------------------------------------------------------------------

/// The four per-variant `match`-arm sets that drive an enum's `WaveWire` methods.
#[derive(Default)]
struct EnumArms {
    heap_size: Vec<TokenStream2>, // self -> active variant's payload size
    tag: Vec<TokenStream2>,       // self -> tag literal (fields ignored)
    encode_heap: Vec<TokenStream2>, // self -> write the unit into the heap
    decode: Vec<TokenStream2>,    // tag  -> reconstruct the variant
}

fn expand_enum(
    input: &DeriveInput,
    data: &DataEnum,
) -> syn::Result<TokenStream2> {
    if data.variants.len() > 256 {
        return Err(syn::Error::new(
            input.span(),
            "WaveWire enums are limited to 256 variants (u8 tag)",
        ));
    }
    let has_fields = data
        .variants
        .iter()
        .any(|v| !matches!(v.fields, Fields::Unit));
    let arms = enum_variant_arms(data);
    let EnumArms {
        heap_size,
        tag,
        encode_heap,
        decode,
    } = &arms;

    let (stack_size, heap_size, encode_stack, encode_heap, decode) =
        if has_fields {
            (
                quote!(1 + 4),
                quote!(match self { #(#heap_size,)* }),
                quote! {
                    let __tag: u8 = match self { #(#tag,)* };
                    stack.push(__tag);
                    stack.extend_from_slice(
                        &(::wavedb_wire::WaveWire::heap_size(self) as u32).to_le_bytes(),
                    );
                },
                quote!(match self { #(#encode_heap,)* }),
                quote! {
                    let __tag = stack.u8()?;
                    let __payload_len = stack.u32()? as usize;
                    let __payload = heap.take(__payload_len)?;
                    match __tag {
                        #(#decode,)*
                        other => ::core::result::Result::Err(
                            ::wavedb_wire::Error::InvalidTag(other),
                        ),
                    }
                },
            )
        } else {
            (
                quote!(1),
                quote!(0),
                quote! {
                    let __tag: u8 = match self { #(#tag,)* };
                    stack.push(__tag);
                },
                quote!(),
                quote! {
                    let __tag = stack.u8()?;
                    match __tag {
                        #(#decode,)*
                        other => ::core::result::Result::Err(
                            ::wavedb_wire::Error::InvalidTag(other),
                        ),
                    }
                },
            )
        };

    Ok(wire_impl(
        input,
        &stack_size,
        &heap_size,
        &encode_stack,
        &encode_heap,
        &decode,
    ))
}

/// Build the per-variant arms. Tags are the declaration order; the variant count
/// is bounded to 256 by [`expand_enum`], so the index always fits a `u8`.
fn enum_variant_arms(data: &DataEnum) -> EnumArms {
    let mut arms = EnumArms::default();

    for (i, v) in data.variants.iter().enumerate() {
        let tag = u8::try_from(i).expect("variant count is bounded to 256");
        let vident = &v.ident;

        match &v.fields {
            Fields::Unit => {
                arms.heap_size.push(quote!(Self::#vident => 0));
                arms.tag.push(quote!(Self::#vident => #tag));
                arms.encode_heap.push(quote!(Self::#vident => {}));
                arms.decode.push(
                    quote!(#tag => ::core::result::Result::Ok(Self::#vident)),
                );
            }
            Fields::Unnamed(fields) => {
                let binds: Vec<_> = (0..fields.unnamed.len())
                    .map(|j| format_ident!("__f{j}"))
                    .collect();
                let tys: Vec<_> =
                    fields.unnamed.iter().map(|f| &f.ty).collect();
                push_field_variant_arms(
                    &mut arms,
                    tag,
                    &quote!(Self::#vident( #(#binds),* )),
                    &quote!(Self::#vident( .. )),
                    &binds,
                    &tys,
                    |decoders| quote!(Self::#vident( #(#decoders,)* )),
                );
            }
            Fields::Named(fields) => {
                let binds: Vec<_> = fields
                    .named
                    .iter()
                    .map(|f| f.ident.clone().expect("named field has ident"))
                    .collect();
                let tys: Vec<_> = fields.named.iter().map(|f| &f.ty).collect();
                let names = binds.clone();
                push_field_variant_arms(
                    &mut arms,
                    tag,
                    &quote!(Self::#vident { #(#binds),* }),
                    &quote!(Self::#vident { .. }),
                    &binds,
                    &tys,
                    move |decoders| {
                        let fields = names
                            .iter()
                            .zip(decoders)
                            .map(|(n, d)| quote!(#n: #d));
                        quote!(Self::#vident { #(#fields,)* })
                    },
                );
            }
        }
    }

    arms
}

/// Append the four arms for a variant that carries fields. `bind_pat` binds the
/// fields by the idents in `binds`; `build(decoders)` assembles the variant from
/// the per-field decode expressions.
fn push_field_variant_arms<I, F>(
    arms: &mut EnumArms,
    tag: u8,
    bind_pat: &TokenStream2,
    tag_pat: &TokenStream2,
    binds: &[I],
    tys: &[&syn::Type],
    build: F,
) where
    I: quote::ToTokens,
    F: FnOnce(Vec<TokenStream2>) -> TokenStream2,
{
    let hs = binds.iter().zip(tys).map(|(b, ty)| {
        quote!(<#ty as ::wavedb_wire::WaveWire>::STACK_SIZE
            + ::wavedb_wire::WaveWire::heap_size(#b))
    });
    arms.heap_size.push(quote!(#bind_pat => 0 #( + #hs )*));
    arms.tag.push(quote!(#tag_pat => #tag));

    let enc_stacks = binds
        .iter()
        .map(|b| quote!(::wavedb_wire::WaveWire::encode_stack(#b, heap);));
    let enc_heaps = binds
        .iter()
        .map(|b| quote!(::wavedb_wire::WaveWire::encode_heap(#b, heap);));
    arms.encode_heap
        .push(quote!(#bind_pat => { #(#enc_stacks)* #(#enc_heaps)* }));

    let decoders: Vec<_> = tys
        .iter()
        .map(|ty| quote!(<#ty as ::wavedb_wire::WaveWire>::decode(&mut __sc, &mut __hc)?))
        .collect();
    let variant_expr = build(decoders);
    arms.decode.push(unit_decode_arm(tag, tys, &variant_expr));
}

/// A `has_fields` enum decode arm: split the already-taken `__payload` into its
/// stack part (the sum of the variant's field stack sizes) and heap part, then
/// build the variant over two fresh cursors named `__sc` / `__hc`.
fn unit_decode_arm(
    tag: u8,
    field_types: &[&syn::Type],
    build: &TokenStream2,
) -> TokenStream2 {
    quote! {
        #tag => {
            let __stack_size = 0usize
                #( + <#field_types as ::wavedb_wire::WaveWire>::STACK_SIZE )*;
            if __payload.len() < __stack_size {
                return ::core::result::Result::Err(
                    ::wavedb_wire::Error::UnexpectedEof,
                );
            }
            let (__vs, __vh) = __payload.split_at(__stack_size);
            let mut __sc = ::wavedb_wire::Cursor::new(__vs);
            let mut __hc = ::wavedb_wire::Cursor::new(__vh);
            ::core::result::Result::Ok(#build)
        }
    }
}
