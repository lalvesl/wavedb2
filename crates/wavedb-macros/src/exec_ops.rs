//! The per-item **execution steps** `#[wavedb]` emits: one
//! `__wavedb_<command>` fn per wire command, uniform across shapes so the
//! exposure macros can reference them without knowing a path's shape.
//!
//! Each fn has the exposure-op signature
//! `async fn(&S, U48, &[u8]) -> Result<Reply>` — the same signature a
//! `expose_server!` per-op override must have. A command the shape doesn't
//! support refuses with `UnknownStructHash` (indistinguishable from an
//! unlisted type, on purpose). Defined here, **reachable only when listed**
//! in an exposure declaration.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

/// One refusal body — the uniform "this does not exist" answer.
fn refuse(name: &Ident) -> TokenStream {
    quote! {
        ::core::result::Result::Err(
            ::wavedb_core::Error::UnknownStructHash(
                <#name as ::wavedb_core::WaveDbStruct>::STRUCT_HASH,
            ),
        )
    }
}

/// The op-fn skeleton: `#[doc(hidden)] pub async fn __wavedb_<op>(…)`.
fn op_fn(op: &str, body: &TokenStream) -> TokenStream {
    let ident = quote::format_ident!("__wavedb_{}", op);
    quote! {
        #[doc(hidden)]
        #[allow(clippy::future_not_send)]
        pub async fn #ident<S: ::wavedb_core::Store>(
            store: &S,
            tenant: ::wavedb_core::U48,
            payload: &[u8],
        ) -> ::wavedb_core::Result<::wavedb_core::expose::Reply> {
            #body
        }
    }
}

/// The `Unique` shape's execution steps: `get` / `save` real, the NonUnique
/// commands refuse.
pub fn unique_ops(name: &Ident) -> TokenStream {
    let refuse = refuse(name);
    let get = op_fn(
        "get",
        &quote! {
            let _ = payload;
            let anchor = ::wavedb_core::Id::new(
                <#name as ::wavedb_core::WaveDbStruct>::STRUCT_HASH,
                tenant,
                true,
                0,
            );
            ::wavedb_core::expose::get_value::<#name, S>(store, anchor).await
        },
    );
    let save = op_fn(
        "save",
        &quote! {
            let value: #name = ::wavedb_core::wire::from_wire(payload)?;
            ::wavedb_core::collection::save_unique(store, tenant, &value)
                .await?;
            ::core::result::Result::Ok(::wavedb_core::expose::Reply::Done)
        },
    );
    let insert =
        op_fn("insert", &quote!(let _ = (store, tenant, payload); #refuse));
    let update =
        op_fn("update", &quote!(let _ = (store, tenant, payload); #refuse));
    let remove =
        op_fn("remove", &quote!(let _ = (store, tenant, payload); #refuse));
    let all = op_fn("all", &quote!(let _ = (store, tenant, payload); #refuse));
    quote! {
        impl #name {
            #get
            #save
            #insert
            #update
            #remove
            #all
        }
    }
}

/// The `NonUnique` shape's execution steps: the collection ops real (a
/// handle-less `update`/`remove` reaches the collection through the record's
/// `Metadata.pivot_id` back-link), `save` refuses (NonUnique updates ride
/// `Update`).
pub fn nonunique_ops(name: &Ident, pivot_id: &Ident) -> TokenStream {
    let refuse = refuse(name);
    let get = op_fn(
        "get",
        &quote! {
            let _ = tenant;
            let id: ::wavedb_core::Id =
                ::wavedb_core::wire::from_wire(payload)?;
            ::wavedb_core::expose::get_value::<#name, S>(store, id).await
        },
    );
    let insert = op_fn(
        "insert",
        &quote! {
            let (pivot, value): (::wavedb_core::LocalId, #name) =
                ::wavedb_core::wire::from_wire(payload)?;
            let col = #name::collection(#pivot_id::new(pivot), tenant);
            let id = col.insert(store, &value).await?;
            ::core::result::Result::Ok(
                ::wavedb_core::expose::Reply::Inserted(id),
            )
        },
    );
    let update = op_fn(
        "update",
        &quote! {
            let (id, value): (::wavedb_core::Id, #name) =
                ::wavedb_core::wire::from_wire(payload)?;
            let pivot =
                ::wavedb_core::expose::record_pivot::<#name, S>(store, id)
                    .await?;
            let col = #name::collection(#pivot_id::new(pivot), tenant);
            col.save(store, id, &value).await?;
            ::core::result::Result::Ok(::wavedb_core::expose::Reply::Done)
        },
    );
    let remove = op_fn(
        "remove",
        &quote! {
            let id: ::wavedb_core::Id =
                ::wavedb_core::wire::from_wire(payload)?;
            let pivot =
                ::wavedb_core::expose::record_pivot::<#name, S>(store, id)
                    .await?;
            let col = #name::collection(#pivot_id::new(pivot), tenant);
            let removed = col.remove(store, id).await?;
            ::core::result::Result::Ok(
                ::wavedb_core::expose::Reply::Removed(removed),
            )
        },
    );
    let save =
        op_fn("save", &quote!(let _ = (store, tenant, payload); #refuse));
    let all = op_fn(
        "all",
        &quote! {
            let pivot: ::wavedb_core::LocalId =
                ::wavedb_core::wire::from_wire(payload)?;
            ::wavedb_core::expose::all_values::<#name, S>(store, pivot, tenant)
                .await
        },
    );
    quote! {
        impl #name {
            #get
            #insert
            #update
            #remove
            #save
            #all
        }
    }
}
