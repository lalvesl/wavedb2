//! Parsing for the `expose_server!` / `expose_client!` declarations: the
//! entry grammar (`Path`, `fn Path`, `store Path`, per-op override blocks)
//! — expansion lives in [`crate::expose`].

use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, Path, Token, braced};

/// The per-item ops an entry can override or exclude — one per wire
/// [`Command`](wavedb_core::expose::Command).
pub const OPS: [&str; 7] = [
    "get", "save", "insert", "update", "remove", "all", "history",
];

/// What a declared entry contributes.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    /// A struct: the full per-command dispatch + its storage slots.
    Struct,
    /// A `#[server]` function: one uniform dispatch arm, no storage.
    Fn,
    /// Storage-only: engine slots, **no** wire surface at all.
    Store,
}

/// One declared item: a struct (with optional per-op overrides), a `fn`
/// `#[server]` function, or a `store` storage-only type.
pub struct Entry {
    pub kind: Kind,
    pub path: Path,
    /// `(op, None)` = excluded (`never`); `(op, Some(path))` = override.
    pub overrides: Vec<(Ident, Option<Path>)>,
}

impl Parse for Entry {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kind = if input.peek(Token![fn]) {
            input.parse::<Token![fn]>()?;
            Kind::Fn
        } else if is_store_marker(input) {
            input.parse::<Ident>()?; // the contextual `store` keyword
            Kind::Store
        } else {
            Kind::Struct
        };
        let path: Path = input.parse()?;
        let mut overrides = Vec::new();
        if input.peek(syn::token::Brace) {
            if kind != Kind::Struct {
                return Err(input.error(
                    "only a struct entry takes per-op overrides — a `fn` \
                     hash is the whole operation, a `store` entry has no \
                     wire surface",
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
            kind,
            path,
            overrides,
        })
    }
}

/// `true` when the next tokens are the contextual `store` marker followed by
/// the entry's path — distinguishes `store Credentials` from a struct that
/// is itself named `store`.
fn is_store_marker(input: ParseStream) -> bool {
    input.peek(Ident)
        && input.fork().parse::<Ident>().is_ok_and(|i| i == "store")
        && (input.peek2(Ident) || input.peek2(Token![::]))
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
pub struct Declaration {
    pub entries: Vec<Entry>,
}

impl Parse for Declaration {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let entries = Punctuated::<Entry, Token![,]>::parse_terminated(input)?
            .into_iter()
            .collect();
        Ok(Self { entries })
    }
}
