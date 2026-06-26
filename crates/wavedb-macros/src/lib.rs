//! WaveDB compile-time front door: `#[wavedb]` and `#[derive(WaveWire)]`.
//!
//! - **`#[derive(WaveWire)]`** emits a [`Wire`](wavedb_core::wire::Wire) impl for a
//!   struct (no serde, no `repr(C)`).
//! - **`#[wavedb]`** turns a struct into a WaveDB object: it computes the
//!   `STRUCT_HASH`, emits the `Wire` impl and the
//!   [`WaveDbStruct`](wavedb_core::traits::WaveDbStruct) impl, and — for
//!   `NonUnique` shapes — generates the `PivotId` / `Pivot` collection types.
//!
//! `#[server]` and the build-time registry scanner are staged separately. See
//! `crates/wavedb-macros/README.md` for the full target surface.

use proc_macro::TokenStream;

mod args;
mod generated;
mod struct_hash;
mod wavedb_attr;
mod wire_derive;

/// Derive [`Wire`](wavedb_core::wire::Wire) for a struct (named, tuple, or unit).
///
/// Field stack slots encode inline in declaration order; heap payloads append
/// depth-first. See [`docs/wire_format.md`](../../../docs/wire_format.md).
#[proc_macro_derive(WaveWire)]
pub fn wave_wire(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    wire_derive::derive(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Turn a struct into a WaveDB object.
///
/// ```ignore
/// #[wavedb]                 // Unique: one live record per tenant
/// pub struct AboutUser { pub name: String }
///
/// #[wavedb(NonUnique)]      // many per tenant, reached through a Pivot
/// #[wavedb::pivot(amount)]  // + a secondary index on `amount`
/// pub struct Order { pub amount: u64 }
/// ```
#[proc_macro_attribute]
pub fn wavedb(attr: TokenStream, item: TokenStream) -> TokenStream {
    wavedb_attr::expand(attr.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
