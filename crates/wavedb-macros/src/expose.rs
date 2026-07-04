//! `expose_server!` / `expose_client!` — the **declared registry**.
//!
//! ```text
//! expose_server! {
//!     AboutUser,                                   // full generated op set
//!     Order { save: audited_save, remove: never }, // per-op override / exclusion
//! }
//! ```
//!
//! The list expands to one `match` on the 64-bit `STRUCT_HASH` per operation,
//! each arm calling the item's `#[wavedb]`-generated `__wavedb_<op>` step (or
//! the override path, substituted **inside the arm** at expansion time; a
//! `never` arm refuses). Static dispatch only — no `dyn`, no fn-pointer
//! tables, no runtime registration. The server emission implements the full
//! [`Exposure`] trait (`REGISTRY`); the client one implements only the
//! reachability half (`CLIENT_REGISTRY`) — typed call stubs arrive with
//! `#[server]`/`Db` (M4).
//!
//! [`Exposure`]: wavedb_core::expose::Exposure

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, Path, Token, braced};

/// The per-item ops an entry can override or exclude — one per wire
/// [`Command`](wavedb_core::expose::Command).
const OPS: [&str; 7] = [
    "get", "save", "insert", "update", "remove", "all", "history",
];

/// One declared item: a struct (with optional per-op overrides) or, when
/// prefixed with `fn`, a `#[server]` function (uniform dispatch, no overrides).
struct Entry {
    is_fn: bool,
    path: Path,
    /// `(op, None)` = excluded (`never`); `(op, Some(path))` = override.
    overrides: Vec<(Ident, Option<Path>)>,
}

impl Parse for Entry {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let is_fn = input.peek(Token![fn]);
        if is_fn {
            input.parse::<Token![fn]>()?;
        }
        let path: Path = input.parse()?;
        let mut overrides = Vec::new();
        if input.peek(syn::token::Brace) {
            if is_fn {
                return Err(input.error(
                    "a `fn` entry takes no per-op overrides — its hash is \
                     the whole operation",
                ));
            }
            let inner;
            braced!(inner in input);
            for pair in
                Punctuated::<OpOverride, Token![,]>::parse_terminated(&inner)?
            {
                overrides.push((pair.op, pair.target));
            }
        }
        Ok(Self {
            is_fn,
            path,
            overrides,
        })
    }
}

/// `op: never` or `op: path`.
struct OpOverride {
    op: Ident,
    target: Option<Path>,
}

impl Parse for OpOverride {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let op: Ident = input.parse()?;
        if !OPS.contains(&op.to_string().as_str()) {
            return Err(syn::Error::new_spanned(
                &op,
                "unknown op; expected one of \
                 get/save/insert/update/remove/all/history",
            ));
        }
        input.parse::<Token![:]>()?;
        let target: Path = input.parse()?;
        let target = if target.is_ident("never") {
            None
        } else {
            Some(target)
        };
        Ok(Self { op, target })
    }
}

/// The whole declaration: a comma-separated entry list.
struct Declaration {
    entries: Vec<Entry>,
}

impl Parse for Declaration {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let entries = Punctuated::<Entry, Token![,]>::parse_terminated(input)?
            .into_iter()
            .collect();
        Ok(Self { entries })
    }
}

/// The call expression for one entry's one op: the generated step, the
/// override path, or the `never` refusal — resolved at expansion time.
fn op_call(entry: &Entry, op: &str) -> TokenStream {
    let path = &entry.path;
    match entry.overrides.iter().find(|(o, _)| o == op) {
        Some((_, Some(target))) => {
            quote!(#target(store, tenant, payload).await)
        }
        Some((_, None)) => quote! {
            ::core::result::Result::Err(
                ::wavedb_core::Error::UnknownStructHash(
                    <#path as ::wavedb_core::WaveDbStruct>::STRUCT_HASH,
                ),
            )
        },
        None => {
            let step = format_ident!("__wavedb_{}", op);
            quote!(#path::#step(store, tenant, payload).await)
        }
    }
}

/// The `STRUCT_HASH` expression for an entry (structs read it off
/// `WaveDbStruct`, `#[server]` fn-types carry it inherently).
fn hash_expr(entry: &Entry) -> TokenStream {
    let path = &entry.path;
    if entry.is_fn {
        quote!(#path::STRUCT_HASH)
    } else {
        quote!(<#path as ::wavedb_core::WaveDbStruct>::STRUCT_HASH)
    }
}

/// The `StorageRegistry` impl for `ServerRegistry` — native only, flattening
/// each listed struct's `storage_entries()` (functions hold no storage; the
/// reserved BpTree-node slot is added by `PageStore::open`).
fn storage_registry_impl(struct_paths: &[&Path]) -> TokenStream {
    quote! {
        #[cfg(not(target_arch = "wasm32"))]
        impl ::wavedb_storage::StorageRegistry for ServerRegistry {
            fn storage_entries(
                &self,
            ) -> ::std::vec::Vec<&'static ::wavedb_storage::StructStorage>
            {
                let mut slots = ::std::vec::Vec::new();
                #(
                    slots.extend_from_slice(&#struct_paths::storage_entries());
                )*
                slots
            }
        }
    }
}

/// Expand `expose_server!`: the full [`Exposure`] impl + `REGISTRY`.
pub fn expand_server(input: TokenStream) -> syn::Result<TokenStream> {
    let decl: Declaration = syn::parse2(input)?;
    let struct_paths: Vec<&Path> = decl
        .entries
        .iter()
        .filter(|e| !e.is_fn)
        .map(|e| &e.path)
        .collect();
    let hashes: Vec<TokenStream> = decl.entries.iter().map(hash_expr).collect();
    let storage_impl = storage_registry_impl(&struct_paths);

    let decode_arms = decl.entries.iter().map(|entry| {
        let path = &entry.path;
        let h = hash_expr(entry);
        if entry.is_fn {
            // A function payload is an args tuple, not a struct body — the
            // header gate only proves the hash is served.
            quote!(h if h == #h => ::core::result::Result::Ok(()),)
        } else {
            quote! {
                h if h == #h => {
                    ::wavedb_core::expose::decode_check::<#path>(bytes)
                }
            }
        }
    });

    let execute_arms = decl.entries.iter().map(|entry| {
        let path = &entry.path;
        let h = hash_expr(entry);
        if entry.is_fn {
            return quote! {
                h if h == #h => {
                    #path::__wavedb_dispatch(store, tenant, command, payload)
                        .await
                }
            };
        }
        let ops = OPS.iter().map(|op| {
            let variant =
                format_ident!("{}{}", op[..1].to_uppercase(), &op[1..]);
            let call = op_call(entry, op);
            quote!(::wavedb_core::expose::Command::#variant => #call,)
        });
        quote! {
            h if h == #h => {
                match command {
                    #(#ops)*
                }
            }
        }
    });

    Ok(quote! {
        /// The node-side registry `expose_server!` declared: exactly the
        /// listed items are wire-reachable, dispatched by a per-hash `match`.
        #[derive(Debug, Clone, Copy)]
        pub struct ServerRegistry;

        /// What a node's `.registry(…)` takes — the declared surface.
        pub const REGISTRY: ServerRegistry = ServerRegistry;

        impl ::wavedb_core::expose::Exposure for ServerRegistry {
            fn knows(&self, struct_hash: u64) -> bool {
                [#(#hashes),*].contains(&struct_hash)
            }

            fn decode_check(
                &self,
                struct_hash: u64,
                bytes: &[u8],
            ) -> ::wavedb_core::Result<()> {
                match struct_hash {
                    #(#decode_arms)*
                    other => ::core::result::Result::Err(
                        ::wavedb_core::Error::UnknownStructHash(other),
                    ),
                }
            }

            // Store-generic seam — Send only when the backing store is,
            // the same stance the core traits take.
            #[allow(clippy::future_not_send)]
            async fn execute<S: ::wavedb_core::Store>(
                &self,
                store: &S,
                tenant: ::wavedb_core::U48,
                struct_hash: u64,
                command: ::wavedb_core::expose::Command,
                payload: &[u8],
            ) -> ::wavedb_core::Result<::wavedb_core::expose::Reply> {
                match struct_hash {
                    #(#execute_arms)*
                    other => ::core::result::Result::Err(
                        ::wavedb_core::Error::UnknownStructHash(other),
                    ),
                }
            }
        }

        #storage_impl
    })
}

/// Expand `expose_client!`: the reachability half only (`knows` +
/// `decode_check`; `execute` keeps the trait's refusing default — a client
/// never runs the engine).
pub fn expand_client(input: TokenStream) -> syn::Result<TokenStream> {
    let decl: Declaration = syn::parse2(input)?;
    for entry in &decl.entries {
        if let Some((op, _)) = entry.overrides.first() {
            return Err(syn::Error::new_spanned(
                op,
                "expose_client! entries take no per-op overrides — \
                 overrides shape the server surface",
            ));
        }
    }
    let hashes: Vec<TokenStream> = decl.entries.iter().map(hash_expr).collect();
    let decode_arms = decl.entries.iter().map(|entry| {
        let path = &entry.path;
        let h = hash_expr(entry);
        if entry.is_fn {
            quote!(h if h == #h => ::core::result::Result::Ok(()),)
        } else {
            quote! {
                h if h == #h => {
                    ::wavedb_core::expose::decode_check::<#path>(bytes)
                }
            }
        }
    });

    Ok(quote! {
        /// The client-side allowlist `expose_client!` declared: which items
        /// this binary's typed stubs may route (stubs land with `#[server]`).
        #[derive(Debug, Clone, Copy)]
        pub struct ClientRegistry;

        /// The declared client surface.
        pub const CLIENT_REGISTRY: ClientRegistry = ClientRegistry;

        impl ::wavedb_core::expose::Exposure for ClientRegistry {
            fn knows(&self, struct_hash: u64) -> bool {
                [#(#hashes),*].contains(&struct_hash)
            }

            fn decode_check(
                &self,
                struct_hash: u64,
                bytes: &[u8],
            ) -> ::wavedb_core::Result<()> {
                match struct_hash {
                    #(#decode_arms)*
                    other => ::core::result::Result::Err(
                        ::wavedb_core::Error::UnknownStructHash(other),
                    ),
                }
            }
        }
    })
}
