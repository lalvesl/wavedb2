//! Parsing of `#[wavedb(...)]` attribute arguments.
//!
//! Grammar (all optional, comma-separated):
//!
//! ```text
//! #[wavedb]                                  // Unique (default)
//! #[wavedb(NonUnique)]                       // NonUnique shape
//! #[wavedb(validate = path, preprocess = p)] // hook fns (either shape)
//! #[wavedb(compress = false)]                // opt this type's pages out of zstd
//! ```

use syn::punctuated::Punctuated;
use syn::{Ident, Meta, Path, Token, parse::ParseStream};

/// The cardinality shape declared on a `#[wavedb]` struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Unique,
    NonUnique,
}

impl Shape {
    /// The canonical name folded into `STRUCT_HASH` and emitted as `Shape::_`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unique => "Unique",
            Self::NonUnique => "NonUnique",
        }
    }

    /// The `::wavedb_core::Shape` variant ident.
    pub const fn core_variant(self) -> &'static str {
        self.as_str()
    }
}

/// Parsed `#[wavedb(...)]` arguments.
#[derive(Debug, Clone)]
pub struct WavedbArgs {
    pub shape: Shape,
    pub validate: Option<Path>,
    pub preprocess: Option<Path>,
    /// Whether this type's pages run through zstd (storage policy — not part
    /// of the schema, so it never feeds the `STRUCT_HASH`).
    pub compress: bool,
}

impl Default for WavedbArgs {
    fn default() -> Self {
        Self {
            shape: Shape::Unique,
            validate: None,
            preprocess: None,
            compress: true,
        }
    }
}

impl WavedbArgs {
    /// Parse the token stream inside `#[wavedb(...)]`.
    pub fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut args = Self::default();
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        for meta in metas {
            match &meta {
                // Bare path: a shape marker.
                Meta::Path(p) if p.is_ident("NonUnique") => {
                    args.shape = Shape::NonUnique;
                }
                Meta::Path(p) if p.is_ident("Unique") => {
                    args.shape = Shape::Unique;
                }
                // name = value: a hook.
                Meta::NameValue(nv) if nv.path.is_ident("validate") => {
                    args.validate = Some(expr_as_path(&nv.value)?);
                }
                Meta::NameValue(nv) if nv.path.is_ident("preprocess") => {
                    args.preprocess = Some(expr_as_path(&nv.value)?);
                }
                Meta::NameValue(nv) if nv.path.is_ident("compress") => {
                    args.compress = expr_as_bool(&nv.value)?;
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unsupported #[wavedb(...)] argument; expected `NonUnique`, \
                         `Unique`, `validate = fn`, `preprocess = fn`, or \
                         `compress = bool`",
                    ));
                }
            }
        }
        Ok(args)
    }
}

/// Interpret a `name = value` value as a function path (`validate = my_fn`).
fn expr_as_path(expr: &syn::Expr) -> syn::Result<Path> {
    if let syn::Expr::Path(p) = expr {
        Ok(p.path.clone())
    } else {
        Err(syn::Error::new_spanned(expr, "expected a function path"))
    }
}

/// Interpret a `name = value` value as a bool literal (`compress = false`).
fn expr_as_bool(expr: &syn::Expr) -> syn::Result<bool> {
    if let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Bool(b),
        ..
    }) = expr
    {
        Ok(b.value)
    } else {
        Err(syn::Error::new_spanned(expr, "expected `true` or `false`"))
    }
}

/// One `#[wavedb::pivot(...)]` declaration: the indexed field(s) in
/// declaration order — `pivot(amount)` or a composite `pivot((customer, date))`
/// of two or three fields (the `IndexKey` tuple arities).
#[derive(Debug, Clone)]
pub struct PivotSpec {
    pub fields: Vec<Ident>,
}

impl syn::parse::Parse for PivotSpec {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.peek(syn::token::Paren) {
            let inner;
            syn::parenthesized!(inner in input);
            let idents =
                Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
            let fields: Vec<Ident> = idents.into_iter().collect();
            if !(2..=3).contains(&fields.len()) {
                return Err(syn::Error::new(
                    inner.span(),
                    "a composite #[wavedb::pivot((..))] takes 2 or 3 fields",
                ));
            }
            Ok(Self { fields })
        } else {
            Ok(Self {
                fields: vec![input.parse()?],
            })
        }
    }
}
