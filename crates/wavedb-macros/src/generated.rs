//! Per-NonUnique generated types: the `{Name}PivotId` handle and the `{Name}Pivot`
//! roots holder.
//!
//! These carry no business data — pure addressing. `PivotId` is a `LocalId`
//! newtype callers store in a field to reference the collection; `Pivot` holds the
//! `current` / `dead` B+tree roots plus one root per secondary index, and
//! implements [`wavedb_core::index::Pivot`].
//!
//! The full `BpTree` *implementation* lives in `wavedb-storage` (it needs the page
//! engine); the macro generates only the data-carrying types here.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, parse_quote};

use crate::{struct_hash, wire_derive};

/// Emit the `PivotId` newtype and the `Pivot` roots holder for a NonUnique struct
/// named `name` with `num_secondaries` secondary indexes.
pub fn nonunique_types(
    name: &Ident,
    num_secondaries: usize,
) -> syn::Result<TokenStream> {
    let pivot_id = format_ident!("{}PivotId", name);
    let pivot = format_ident!("{}Pivot", name);

    // The pivot record's own identity: hashed like any struct, under the
    // reserved `Pivot` shape discriminator so it can never collide with a
    // user-declared `Unique`/`NonUnique` type of the same name and fields.
    let pivot_hash = struct_hash::compute(
        &pivot.to_string(),
        "Pivot",
        &[
            ("current".into(), "LocalId".into()),
            ("dead".into(), "LocalId".into()),
            ("secondaries".into(), format!("[LocalId;{num_secondaries}]")),
            ("permission".into(), "Option<PermissionRef>".into()),
        ],
    );

    // `struct {Name}PivotId(pub LocalId);` — WaveWire by delegation to LocalId.
    let pivot_id_def: syn::DeriveInput = parse_quote! {
        #[derive(::core::clone::Clone, ::core::marker::Copy,
                 ::core::cmp::PartialEq, ::core::cmp::Eq,
                 ::core::fmt::Debug, ::core::default::Default)]
        pub struct #pivot_id(pub ::wavedb_core::LocalId);
    };
    let pivot_id_wire = wire_derive::derive(&pivot_id_def)?;

    // `struct {Name}Pivot { current, dead, secondaries: [LocalId; N], permission }`.
    // `#[repr(C)]` keeps a clean layout when `N == 0` (a trailing zero-sized array
    // is otherwise a lint footgun); WaveWire never depends on the repr.
    let pivot_def: syn::DeriveInput = parse_quote! {
        #[repr(C)]
        #[derive(::core::clone::Clone, ::core::cmp::PartialEq, ::core::cmp::Eq,
                 ::core::fmt::Debug, ::core::default::Default)]
        pub struct #pivot {
            pub current: ::wavedb_core::LocalId,
            pub dead: ::wavedb_core::LocalId,
            pub secondaries: [::wavedb_core::LocalId; #num_secondaries],
            pub permission: ::core::option::Option<::wavedb_core::PermissionRef>,
        }
    };
    let pivot_wire = wire_derive::derive(&pivot_def)?;

    Ok(quote! {
        #pivot_id_def
        #pivot_id_wire

        impl #pivot_id {
            /// Wrap a `LocalId` as this collection's handle.
            #[must_use]
            pub const fn new(local: ::wavedb_core::LocalId) -> Self {
                Self(local)
            }
            /// The underlying `LocalId`.
            #[must_use]
            pub const fn local_id(self) -> ::wavedb_core::LocalId {
                self.0
            }
        }

        #pivot_def
        #pivot_wire

        impl ::wavedb_core::NonUniqueStruct for #name {
            type Pivot = #pivot;
        }

        impl #name {
            /// The typed handle into an existing collection of this type,
            /// referenced by the `PivotId` an owning record stores.
            #[must_use]
            pub const fn collection(
                pivot: #pivot_id,
                tenant: ::wavedb_core::U48,
            ) -> ::wavedb_core::Collection<Self> {
                ::wavedb_core::Collection::at(pivot.local_id(), tenant)
            }

            /// Create a new, empty collection of this type under `tenant` —
            /// explicit, never automatic. Store the returned `PivotId` in an
            /// owning record to keep the collection reachable.
            ///
            /// # Errors
            /// Propagates a [`Store`](::wavedb_core::Store) failure.
            #[allow(clippy::future_not_send)]
            pub async fn create_pivot<S: ::wavedb_core::Store>(
                store: &S,
                tenant: ::wavedb_core::U48,
            ) -> ::wavedb_core::Result<#pivot_id> {
                ::wavedb_core::Collection::<Self>::create(store, tenant)
                    .await
                    .map(#pivot_id::new)
            }
        }

        impl ::wavedb_core::index::Pivot for #pivot {
            const STRUCT_HASH: u64 = #pivot_hash;

            fn current(&self) -> ::wavedb_core::LocalId { self.current }
            fn dead(&self) -> ::wavedb_core::LocalId { self.dead }
            fn secondaries(&self) -> &[::wavedb_core::LocalId] { &self.secondaries }
            fn permission(&self) -> ::core::option::Option<&::wavedb_core::PermissionRef> {
                self.permission.as_ref()
            }
            fn replace_roots(
                &self,
                current: ::wavedb_core::LocalId,
                dead: ::wavedb_core::LocalId,
            ) -> Self {
                Self {
                    current,
                    dead,
                    secondaries: self.secondaries,
                    permission: ::core::clone::Clone::clone(&self.permission),
                }
            }
        }
    })
}
