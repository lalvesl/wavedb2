//! The stream-returning `#[server]` expansion
//! (`-> impl Stream<Item = Result<T>>`) — split from [`crate::server`] for
//! the file budget: the node runs the body's stream and ships one item frame
//! per element; the client stub re-exposes the same async iterator, decoding
//! frames as they arrive.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{ItemFn, Type};

use crate::server::{Arg, decode_and_forward, encode_payload};

/// Emit the three items for a stream-returning fn (body, fn-type + dispatch,
/// client stub). `hash` is the composed-identity `const` expression.
pub fn expand(
    func: &ItemFn,
    name: &syn::Ident,
    vis: &syn::Visibility,
    db_pat: &syn::Pat,
    args: &[Arg],
    item_ty: &Type,
    hash: &TokenStream,
) -> TokenStream {
    let output = &func.sig.output;
    let body = &func.block;
    let body_fn = format_ident!("__{}_body", name);
    let (decode, forward) = decode_and_forward(args);
    let payload = encode_payload(args);
    let arg_sig: Vec<TokenStream> = args
        .iter()
        .map(|a| {
            let (p, t) = (&a.pat, &a.ty);
            quote!(#p: #t)
        })
        .collect();

    quote! {
        // The server body: not async — it *returns* the stream, borrowing
        // the node context like any collection walk.
        #[allow(clippy::future_not_send, non_snake_case)]
        fn #body_fn<S: ::wavedb_core::Store>(
            #db_pat: &::wavedb::ServerDb<'_, S>,
            #(#arg_sig),*
        ) #output {
            #[allow(unused_imports)]
            use ::wavedb_core::DbHandle as _;
            #body
        }

        // The fn-type: identity + the node dispatch step.
        #[allow(non_camel_case_types)]
        #vis struct #name {}

        impl #name {
            /// This function's composed identity, in the struct hash space.
            pub const STRUCT_HASH: u64 = #hash;

            /// Decode args, run the body's stream, ship the items. Buffered
            /// node-side for now (a later internal change); the wire and the
            /// client already stream frame-by-frame.
            #[doc(hidden)]
            #[allow(clippy::future_not_send)]
            pub async fn __wavedb_dispatch<S: ::wavedb_core::Store>(
                store: &S,
                tenant: ::wavedb_core::U48,
                _command: ::wavedb_core::expose::Command,
                payload: &[u8],
            ) -> ::wavedb_core::Result<::wavedb_core::expose::Reply> {
                use ::wavedb_core::TryStreamExt as _;
                #decode
                let db = ::wavedb::ServerDb::new(store, tenant);
                let items = #body_fn(&db, #(#forward),*);
                let collected: ::std::vec::Vec<#item_ty> =
                    match items.try_collect().await {
                        ::core::result::Result::Ok(v) => v,
                        ::core::result::Result::Err(error) => {
                            return ::core::result::Result::Err(
                                ::wavedb_core::Error::Backend(
                                    ::std::string::ToString::to_string(&error),
                                ),
                            );
                        }
                    };
                let entries = collected
                    .iter()
                    .map(::wavedb_core::wire::to_wire)
                    .collect();
                ::core::result::Result::Ok(
                    ::wavedb_core::expose::Reply::Values(entries),
                )
            }
        }

        // The client stub: the same async-iterator signature, decoding item
        // frames as the node writes them.
        #vis fn #name(
            #db_pat: &::wavedb::Db,
            #(#arg_sig),*
        ) #output {
            #db_pat.call_fn_stream::<#item_ty>(#name::STRUCT_HASH, #payload)
        }
    }
}
