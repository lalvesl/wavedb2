//! The generated registry **collision guard** — split from [`crate::expose`]
//! for the file budget.
//!
//! Both checks are pairwise over the declared entries and both resolve at
//! **compile time**, so a clash is caught by `cargo check`, never deferred to a
//! test run:
//!
//! - **Full 64-bit `STRUCT_HASH`** — a hard error. Two entries sharing it would
//!   have one arm silently shadow the other in every dispatch `match`, and the
//!   wire could not tell the types apart at all.
//! - **15-bit `type_salt`** (the low bits of the same hash) — a warning. Reads
//!   stay correct (the full head is verified), but the pair shares archive slots
//!   and loses its separation in the browser's flat keyspace, so it is surfaced
//!   rather than enforced.

use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned as _;

use crate::expose::hash_expr;
use crate::expose_parse::{Entry, Kind};

/// Emit the compile-time guards for `entries` (see the module docs).
///
/// Emitted at the invocation scope (not a nested module) so entry paths resolve
/// exactly as written in the declaration, and every diagnostic is spanned at the
/// **second entry of the clashing pair** — the compiler underlines the offending
/// line in the exposure list itself.
pub fn collision_guard(entries: &[Entry]) -> TokenStream {
    let mut guards = TokenStream::new();
    for (i, b) in entries.iter().enumerate() {
        for a in &entries[..i] {
            guards.extend(identity_error(a, b));
            // `fn`s share the dispatch hash space but are never stored, so the
            // storage-slot discriminator does not apply to them.
            if a.kind != Kind::Fn && b.kind != Kind::Fn {
                guards.extend(salt_warning(a, b));
            }
        }
    }
    guards
}

/// The hard error: a pair sharing the full 64-bit identity.
fn identity_error(a: &Entry, b: &Entry) -> TokenStream {
    let (ha, hb) = (hash_expr(a), hash_expr(b));
    let message = format!(
        "STRUCT_HASH collision between `{}` and `{}`: the two are one identity \
         on the wire, so one would shadow the other in every dispatch match. \
         Rename the type or a field to reshuffle the hash.",
        render(&a.path),
        render(&b.path),
    );
    let span = b.path.span();
    quote_spanned! { span =>
        const _: () = ::core::assert!(#ha != #hb, #message);
    }
}

/// A path as written, without the token stream's inter-token spacing
/// (`tw1 :: Twin` → `tw1::Twin`).
fn render(path: &syn::Path) -> String {
    quote!(#path).to_string().split_whitespace().collect()
}

/// The warning: a pair sharing the 15-bit storage discriminator.
fn salt_warning(a: &Entry, b: &Entry) -> TokenStream {
    let (ha, hb) = (hash_expr(a), hash_expr(b));
    quote_spanned! { b.path.span() =>
        const _: () = ::wavedb_core::expose::SaltGuard::<
            { (#ha & 0x7FFF) != (#hb & 0x7FFF) },
        >::check();
    }
}
