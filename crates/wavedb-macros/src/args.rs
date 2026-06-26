//! Parsing of `#[wavedb(...)]` attribute arguments.
//!
//! Grammar (all optional, comma-separated):
//!
//! ```text
//! #[wavedb]                                  // Unique (default)
//! #[wavedb(NonUnique)]                       // NonUnique shape
//! #[wavedb(validate = path, preprocess = p)] // hook fns (either shape)
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
}

impl Default for WavedbArgs {
    fn default() -> Self {
        Self {
            shape: Shape::Unique,
            validate: None,
            preprocess: None,
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
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unsupported #[wavedb(...)] argument; expected `NonUnique`, \
                         `Unique`, `validate = fn`, or `preprocess = fn`",
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
