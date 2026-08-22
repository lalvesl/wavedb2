//! Parsing of `#[wavedb(...)]` attribute arguments.
//!
//! Grammar (all optional, comma-separated):
//!
//! ```text
//! #[wavedb]                                  // Unique (default)
//! #[wavedb(NonUnique)]                       // NonUnique shape
//! #[wavedb(validate = path, preprocess = p)] // hook fns (either shape)
//! #[wavedb(compress = false)]                // opt out of zstd (folds into the hash)
//! #[wavedb(NonUnique, page = 50)]           // segment capacity (folds too)
//! ```

use syn::punctuated::Punctuated;
use syn::{Meta, Path, Token, parse::ParseStream};

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
    /// Whether this type's pages run through zstd. Storage policy, but it
    /// reaches stored bytes, so it **folds into** the `STRUCT_HASH` (RFC 0052):
    /// flipping it mints a new type rather than reinterpreting old pages.
    pub compress: bool,
    /// `page = N` — the record chain's segment capacity as a **minimum**
    /// (RFC 0052): a segment holds N…2N records, splits at 2N, merges at N/2.
    /// `None` = the engine default. Folds into the `STRUCT_HASH`, because a
    /// chain laid out at one capacity cannot be reinterpreted at another.
    pub page: Option<usize>,
}

impl Default for WavedbArgs {
    fn default() -> Self {
        Self {
            shape: Shape::Unique,
            validate: None,
            preprocess: None,
            compress: true,
            page: None,
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
                Meta::NameValue(nv) if nv.path.is_ident("page") => {
                    args.page = Some(expr_as_page(&nv.value)?);
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unsupported #[wavedb(...)] argument; expected `NonUnique`, \
                         `Unique`, `validate = fn`, `preprocess = fn`, \
                         `compress = bool`, or `page = N`",
                    ));
                }
            }
        }
        Ok(args)
    }
}

/// Interpret a `name = value` value as a function path (`validate = my_fn`).
pub fn expr_as_path(expr: &syn::Expr) -> syn::Result<Path> {
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

/// Interpret a `name = value` value as a segment capacity (`page = 50`).
///
/// Zero is refused rather than clamped: the split trigger is `len >= 2N`, so
/// `N = 0` splits every segment on every insert, including an empty one. A
/// silently-corrected capacity would also be a lie, since the value folds into
/// the identity — the type would not be the one the declaration names.
pub fn expr_as_page(expr: &syn::Expr) -> syn::Result<usize> {
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Int(n),
        ..
    }) = expr
    else {
        return Err(syn::Error::new_spanned(
            expr,
            "expected an integer literal, e.g. `page = 50`",
        ));
    };
    match n.base10_parse::<usize>()? {
        0 => Err(syn::Error::new_spanned(
            expr,
            "`page` must be at least 1 — a segment capacity of 0 would split \
             on every insert",
        )),
        n => Ok(n),
    }
}
