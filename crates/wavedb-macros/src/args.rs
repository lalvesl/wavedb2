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

/// Interpret a `name = value` value as a segment capacity (`page = 50`).
///
/// Zero is refused rather than clamped: the split trigger is `len >= 2N`, so
/// `N = 0` splits every segment on every insert, including an empty one. A
/// silently-corrected capacity would also be a lie, since the value folds into
/// the identity — the type would not be the one the declaration names.
fn expr_as_page(expr: &syn::Expr) -> syn::Result<usize> {
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

/// One `#[wavedb::list(...)]` declaration ([RFC 0051]): the ordering's field(s)
/// and its own optional segment capacity.
///
/// ```text
/// #[wavedb::list]                    // on a field: that field is the ordering
/// #[wavedb::list(page = 25)]         // on a field, with its own capacity
/// #[wavedb::list(name)]              // on the struct
/// #[wavedb::list((city, name))]      // composite
/// #[wavedb::list((city, name), page = 25)]
/// ```
///
/// The capacity is per **list** rather than inherited from the struct's `page`
/// because the two chains have opposite write profiles: the built-in chain is
/// modification-ordered, so every save rewrites its growth-end segment whole and
/// wants a small N, while a list keyed by a domain value is rewritten in place
/// and can afford the N a rendered page actually needs (RFC 0052).
///
/// [RFC 0051]: https://github.com/wavedb/wavedb/blob/main/rfcs/0051-ordered-record-lists.md
#[derive(Debug, Clone)]
pub struct ListSpec {
    /// The ordering's fields; empty for the field-level spelling, where the
    /// field it sits on is the ordering.
    pub fields: Vec<Ident>,
    /// This list's segment capacity, or `None` to inherit the struct's.
    pub page: Option<usize>,
}

impl syn::parse::Parse for ListSpec {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut spec = Self {
            fields: Vec::new(),
            page: None,
        };
        // A leading `(f1, f2)` or bare `field` names the ordering; `page = N`
        // is a `name = value` and never an ordering, so peeking for it is what
        // separates the field-level spelling from the struct-level one.
        if input.peek(syn::token::Paren) {
            let inner;
            syn::parenthesized!(inner in input);
            spec.fields =
                Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?
                    .into_iter()
                    .collect();
            if !(2..=3).contains(&spec.fields.len()) {
                return Err(syn::Error::new(
                    inner.span(),
                    "a composite #[wavedb::list((..))] takes 2 or 3 fields",
                ));
            }
        } else if input.peek(Ident) && !input.peek2(Token![=]) {
            spec.fields = vec![input.parse()?];
        }
        if !input.is_empty() {
            input.parse::<Token![,]>().ok();
        }
        for meta in Punctuated::<Meta, Token![,]>::parse_terminated(input)? {
            let Meta::NameValue(nv) = &meta else {
                return Err(syn::Error::new_spanned(
                    &meta,
                    "expected `page = N` after the ordering's field(s)",
                ));
            };
            if !nv.path.is_ident("page") {
                return Err(syn::Error::new_spanned(
                    &nv.path,
                    "the only #[wavedb::list(...)] option is `page = N`",
                ));
            }
            spec.page = Some(expr_as_page(&nv.value)?);
        }
        Ok(spec)
    }
}

/// The one `#[wavedb::key(...)]` declaration a keyed struct carries: the
/// natural-key field(s), declaration order — the record's anchor is
/// SeaHash over their wire bytes, so these fields ARE the identity.
#[derive(Debug, Clone)]
pub struct KeySpec {
    pub fields: Vec<Ident>,
}

impl syn::parse::Parse for KeySpec {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let idents = Punctuated::<Ident, Token![,]>::parse_terminated(input)?;
        let fields: Vec<Ident> = idents.into_iter().collect();
        if fields.is_empty() {
            return Err(syn::Error::new(
                input.span(),
                "#[wavedb::key(...)] takes at least one field",
            ));
        }
        Ok(Self { fields })
    }
}
