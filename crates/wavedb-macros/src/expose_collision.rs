//! The generated registry **collision guard** (RFC 0040 §6) — split from
//! [`crate::expose`] for the file budget.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::expose::hash_expr;
use crate::expose_parse::{Entry, Kind};

/// A `#[cfg(test)]` guard every `expose_*` emits: every listed item must be
/// distinct on the full 64-bit `STRUCT_HASH` (a dispatch `match` would otherwise
/// silently shadow an arm) and — for stored types (`Fn`s aren't stored) — on the
/// 15-bit `type_salt`, the flat-keyspace / archive-slot discriminator that
/// coexisting schema versions share. Fails the suite, naming the offenders, if
/// two ever clash.
///
/// Emitted at the invocation scope (not a nested module) so entry paths resolve
/// exactly as written in the declaration.
pub fn collision_check(fn_name: &str, entries: &[Entry]) -> TokenStream {
    let fn_ident = format_ident!("{fn_name}");
    let pair = |e: &Entry| {
        let path = &e.path;
        let h = hash_expr(e);
        quote!((::core::stringify!(#path), #h))
    };
    let hash_items: Vec<TokenStream> = entries.iter().map(&pair).collect();
    let salt_items: Vec<TokenStream> = entries
        .iter()
        .filter(|e| e.kind != Kind::Fn)
        .map(&pair)
        .collect();
    quote! {
        #[cfg(test)]
        #[test]
        fn #fn_ident() {
            let mut hashes: ::std::vec::Vec<(&'static str, u64)> =
                ::std::vec![ #(#hash_items),* ];
            hashes.sort_unstable_by_key(|&(_, h)| h);
            for w in hashes.windows(2) {
                ::core::assert_ne!(
                    w[0].1, w[1].1,
                    "STRUCT_HASH collision: {} and {}", w[0].0, w[1].0,
                );
            }
            let mut salts: ::std::vec::Vec<(&'static str, u64)> =
                ::std::vec![ #(#salt_items),* ];
            salts.sort_unstable_by_key(|&(_, h)| h & 0x7FFF);
            for w in salts.windows(2) {
                ::core::assert_ne!(
                    w[0].1 & 0x7FFF, w[1].1 & 0x7FFF,
                    "type_salt collision: {} and {}", w[0].0, w[1].0,
                );
            }
        }
    }
}
