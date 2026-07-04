//! `#[server]` — a server-only async function with a client call binding.
//!
//! From one `async fn name(db: &Db, args…) -> Result<Ret>` the macro emits
//! three coexisting items:
//!
//! - a **fn-type** `struct name {}` (type namespace) carrying the function's
//!   `STRUCT_HASH` and a `__wavedb_dispatch` step — how the node registry
//!   routes an incoming call;
//! - the **server body** as a generic `__name_body<S: Store>(db: &ServerDb…)`
//!   — the user's body with `db` retyped to the node-side context;
//! - the **client stub** `fn name(db: &Db, args…)` (value namespace) that
//!   ships the args over the wire and decodes the return.
//!
//! Type and value namespaces let `struct name` and `fn name` share the name.
//! The function's identity is a `STRUCT_HASH` composed from its signature, in
//! the same hash space as structs — the registry `match` disambiguates.
//!
//! **Auth (M8).** `#[server(public)]` parses today but the login guard is not
//! injected yet; every function is reachable. The marker is preserved so the
//! guard slots into the body later without a signature change.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, GenericArgument, ItemFn, PathArguments, Type, parse2};

use crate::struct_hash;

/// One argument: its binding pattern and its type.
struct Arg {
    pat: Box<syn::Pat>,
    ty: Box<Type>,
}

/// Normalise a type to a stable string for the identity hash (whitespace out).
fn type_string(ty: &Type) -> String {
    quote!(#ty).to_string().split_whitespace().collect()
}

/// The `Ok` type of a `Result<Ok, …>` return; the whole type otherwise.
fn ok_type(output: &syn::ReturnType) -> Type {
    let syn::ReturnType::Type(_, ty) = output else {
        return parse2(quote!(())).expect("unit parses");
    };
    result_ok_arg(ty).unwrap_or_else(|| (**ty).clone())
}

/// The first generic argument of a `Result<…>` type, if `ty` is one.
fn result_ok_arg(ty: &Type) -> Option<Type> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "Result" {
        return None;
    }
    let PathArguments::AngleBracketed(a) = &seg.arguments else {
        return None;
    };
    match a.args.first()? {
        GenericArgument::Type(ok) => Some(ok.clone()),
        _ => None,
    }
}

/// Split a `#[server]` fn's inputs into the `db` receiver and the args.
fn split_inputs(f: &ItemFn) -> syn::Result<(Box<syn::Pat>, Vec<Arg>)> {
    let mut inputs = f.sig.inputs.iter();
    let first = inputs.next().ok_or_else(|| {
        syn::Error::new_spanned(
            &f.sig,
            "a #[server] fn takes `db: &Db` as its first parameter",
        )
    })?;
    let FnArg::Typed(db) = first else {
        return Err(syn::Error::new_spanned(
            first,
            "a #[server] fn is a free function, not a method",
        ));
    };
    let db_pat = db.pat.clone();
    let mut args = Vec::new();
    for input in inputs {
        let FnArg::Typed(a) = input else {
            return Err(syn::Error::new_spanned(input, "unexpected receiver"));
        };
        args.push(Arg {
            pat: a.pat.clone(),
            ty: a.ty.clone(),
        });
    }
    Ok((db_pat, args))
}

/// `(let-bindings decoding `payload` into the args, the arg idents to pass on)`.
fn decode_and_forward(args: &[Arg]) -> (TokenStream, Vec<&syn::Pat>) {
    let pats: Vec<&syn::Pat> = args.iter().map(|a| a.pat.as_ref()).collect();
    let decode = match args.len() {
        0 => quote!(let _ = payload;),
        1 => {
            let (p, t) = (&args[0].pat, &args[0].ty);
            quote! {
                let #p: #t = ::wavedb_core::wire::from_wire(payload)
                    .map_err(::wavedb_core::Error::from)?;
            }
        }
        _ => {
            let types = args.iter().map(|a| &a.ty);
            quote! {
                let ( #(#pats),* ): ( #(#types),* ) =
                    ::wavedb_core::wire::from_wire(payload)
                        .map_err(::wavedb_core::Error::from)?;
            }
        }
    };
    (decode, pats)
}

/// The client stub's payload expression (args → wire bytes).
fn encode_payload(args: &[Arg]) -> TokenStream {
    let pats = args.iter().map(|a| a.pat.as_ref());
    match args.len() {
        0 => quote!(::std::vec::Vec::new()),
        1 => {
            let p = args[0].pat.as_ref();
            quote!(::wavedb_core::wire::to_wire(&#p))
        }
        _ => quote!(::wavedb_core::wire::to_wire(&( #(#pats),* ))),
    }
}

/// Expand `#[server]` (the attribute is parsed for `public` but not yet used).
pub fn expand(
    _attr: TokenStream,
    item: TokenStream,
) -> syn::Result<TokenStream> {
    let func: ItemFn = parse2(item)?;
    let name = func.sig.ident.clone();
    let vis = func.vis.clone();
    let (db_pat, args) = split_inputs(&func)?;
    let ret = ok_type(&func.sig.output);
    let output = func.sig.output.clone();
    let body = func.block;

    // Identity: hash the name, the arg (name, type) pairs, and the return
    // type, under the reserved `fn` discriminator so it can't collide with a
    // struct of the same name and fields.
    let mut fields: Vec<(String, String)> = args
        .iter()
        .map(|a| {
            let pat = &a.pat;
            (quote!(#pat).to_string(), type_string(&a.ty))
        })
        .collect();
    fields.push(("->".into(), type_string(&ret)));
    let hash = struct_hash::compute(&name.to_string(), "fn", &fields);

    let body_fn = format_ident!("__{}_body", name);
    let (decode, forward) = decode_and_forward(&args);
    let payload = encode_payload(&args);
    let arg_sig: Vec<TokenStream> = args
        .iter()
        .map(|a| {
            let (p, t) = (&a.pat, &a.ty);
            quote!(#p: #t)
        })
        .collect();

    Ok(quote! {
        // The server body: the user's block, `db` retyped to the node context.
        #[allow(clippy::future_not_send, non_snake_case)]
        async fn #body_fn<S: ::wavedb_core::Store>(
            #db_pat: &::wavedb::ServerDb<'_, S>,
            #(#arg_sig),*
        ) #output #body

        // The fn-type: identity + the node dispatch step.
        #[allow(non_camel_case_types)]
        #vis struct #name {}

        impl #name {
            /// This function's composed identity, in the struct hash space.
            pub const STRUCT_HASH: u64 = #hash;

            /// Decode args, run the body against a node context, wire the
            /// return. A function ignores the frame command.
            #[doc(hidden)]
            #[allow(clippy::future_not_send)]
            pub async fn __wavedb_dispatch<S: ::wavedb_core::Store>(
                store: &S,
                tenant: ::wavedb_core::U48,
                _command: ::wavedb_core::expose::Command,
                payload: &[u8],
            ) -> ::wavedb_core::Result<::wavedb_core::expose::Reply> {
                #decode
                let db = ::wavedb::ServerDb::new(store, tenant);
                match #body_fn(&db, #(#forward),*).await {
                    ::core::result::Result::Ok(value) => {
                        ::core::result::Result::Ok(
                            ::wavedb_core::expose::Reply::Returned(
                                ::wavedb_core::wire::to_wire(&value),
                            ),
                        )
                    }
                    ::core::result::Result::Err(error) => {
                        ::core::result::Result::Err(
                            ::wavedb_core::Error::Backend(
                                ::std::string::ToString::to_string(&error),
                            ),
                        )
                    }
                }
            }
        }

        // The client stub: ship the args, decode the return.
        #[allow(clippy::future_not_send)]
        #vis async fn #name(
            #db_pat: &::wavedb::Db,
            #(#arg_sig),*
        ) #output {
            #db_pat.call_fn::<#ret>(#name::STRUCT_HASH, #payload).await
        }
    })
}
