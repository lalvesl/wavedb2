//! The native per-struct storage slot: a `#[cfg(not(target_arch = "wasm32"))]`
//! `static StructStorage` (plus accessors) emitted on every `#[wavedb]` type
//! and on each generated `{Name}Pivot`.
//!
//! This is what replaces the engine's runtime `STRUCT_HASH → state` map: each
//! type owns its cache and its page directory as compile-time state, behind
//! its own locks — `Todo::storage_mem_cache()` touches nothing shared with any
//! other type. The statics are handed to `PageStore::open` as an explicit
//! registry (`T::storage_entries()`), the same declared-not-discovered stance
//! the exposure lists take.
//!
//! Consequence for consumers: on non-wasm targets, a crate using `#[wavedb]`
//! needs `wavedb-storage` as a (target-gated) dependency. The wasm expansion
//! omits all of this — the browser store is IndexedDB, no pages, no slots.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

/// Emit the storage-slot impl for `ty`; `hash_expr` is the `const` expression
/// yielding its `STRUCT_HASH` (inherent for structs, the `Pivot` trait const
/// for generated pivots). `compress = false` (`#[wavedb(compress = false)]`)
/// opts the type's pages out of zstd — storage policy, never schema identity.
pub fn statics_for(
    ty: &Ident,
    hash_expr: &TokenStream,
    compress: bool,
) -> TokenStream {
    let ctor = if compress {
        quote!(new)
    } else {
        quote!(without_compression)
    };
    quote! {
        #[cfg(not(target_arch = "wasm32"))]
        impl #ty {
            /// This type's own storage slot — its in-memory cache, page
            /// directory, and compression dictionary, generated at compile
            /// time. Register it (via `storage_entries()`) when opening the
            /// node's `PageStore`.
            #[must_use]
            pub fn struct_storage() -> &'static ::wavedb_storage::StructStorage {
                static SLOT: ::wavedb_storage::StructStorage =
                    ::wavedb_storage::StructStorage::#ctor(#hash_expr);
                &SLOT
            }

            /// This type's in-memory cache — its own `RwLock`, shared with no
            /// other type, so cross-type reads never contend.
            #[must_use]
            pub fn storage_mem_cache() -> &'static ::wavedb_storage::StructMemCache {
                Self::struct_storage().mem_cache()
            }

            /// This type's page-directory slot — its own `Mutex`, shared with
            /// no other type.
            #[must_use]
            pub fn storage_directory() -> &'static ::wavedb_storage::StructDirectory {
                Self::struct_storage().directory()
            }

            /// This type's compression slot — zstd policy + its own
            /// raw-content dictionary, shared with no other type.
            #[must_use]
            pub fn storage_dictionary() -> &'static ::wavedb_storage::StructDictionary {
                Self::struct_storage().dictionary()
            }
        }
    }
}

/// Emit `storage_entries()` on the record type: every slot the type needs
/// registered at `PageStore::open` — its own, plus its Pivot's for NonUnique.
pub fn entries_for(name: &Ident, pivot: Option<&Ident>) -> TokenStream {
    let (len, list) = pivot.map_or_else(
        || (quote!(1usize), quote!([Self::struct_storage()])),
        |pivot| {
            (
                quote!(2usize),
                quote!([Self::struct_storage(), #pivot::struct_storage()]),
            )
        },
    );
    quote! {
        #[cfg(not(target_arch = "wasm32"))]
        impl #name {
            /// The storage slots to register at `PageStore::open` for this
            /// type: its own, plus the generated Pivot's for a NonUnique.
            #[must_use]
            pub fn storage_entries()
            -> [&'static ::wavedb_storage::StructStorage; #len] {
                #list
            }
        }
    }
}
