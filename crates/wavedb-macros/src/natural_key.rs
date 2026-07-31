//! The `#[wavedb::key(...)]` natural-key declaration: parsing, field
//! resolution, and the STRUCT_HASH fold — split from `wavedb_attr` for the
//! file budget. The generated `natural_key` impl itself is emitted with
//! the collection types (`generated::nonunique_types`).

use proc_macro2::Span;
use syn::{Attribute, Ident};

use crate::args::{KeySpec, Shape};

/// Take the `#[wavedb::key(...)]` natural-key declaration (at most one),
/// resolve its fields, and fold it into the STRUCT_HASH field list as a
/// synthetic `#key` entry (`#` can't open a real field name): the key IS
/// the record's address derivation, so changing it = a different type,
/// like any schema change.
pub fn take_and_fold_key(
    attrs: &mut Vec<Attribute>,
    named: &syn::FieldsNamed,
    shape: Shape,
    hash_fields: &mut Vec<(String, String)>,
) -> syn::Result<Option<Vec<(Ident, syn::Type)>>> {
    let key_fields = take_key_spec(attrs)?
        .map(|spec| resolve_key_fields(&spec, named))
        .transpose()?;
    if let Some(fields) = &key_fields {
        if shape != Shape::NonUnique {
            return Err(syn::Error::new(
                Span::call_site(),
                "#[wavedb::key(...)] is only valid on a #[wavedb(NonUnique)] \
                 struct — a Unique type already has a fixed per-tenant anchor",
            ));
        }
        let joined = fields
            .iter()
            .map(|(f, _)| f.to_string())
            .collect::<Vec<_>>()
            .join(",");
        hash_fields.push(("#key".into(), joined));
    }
    Ok(key_fields)
}

/// Remove the `#[wavedb::key(...)]` attribute — at most one; a natural key
/// is a single declaration, composite via multiple fields inside it.
fn take_key_spec(attrs: &mut Vec<Attribute>) -> syn::Result<Option<KeySpec>> {
    let mut spec = None;
    let mut kept = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        if is_key_attr(&attr) {
            if spec.is_some() {
                return Err(syn::Error::new_spanned(
                    &attr,
                    "a struct declares at most one #[wavedb::key(...)]",
                ));
            }
            spec = Some(attr.parse_args::<KeySpec>()?);
        } else {
            kept.push(attr);
        }
    }
    *attrs = kept;
    Ok(spec)
}

/// `true` for a `#[wavedb::key(...)]` helper attribute.
fn is_key_attr(attr: &Attribute) -> bool {
    let segs = &attr.path().segments;
    segs.len() == 2 && segs[0].ident == "wavedb" && segs[1].ident == "key"
}

/// Resolve the declared natural-key fields against the struct's named
/// fields — an unknown field is a compile error at the declaration site.
fn resolve_key_fields(
    spec: &KeySpec,
    named: &syn::FieldsNamed,
) -> syn::Result<Vec<(Ident, syn::Type)>> {
    spec.fields
        .iter()
        .map(|ident| {
            named
                .named
                .iter()
                .find(|f| f.ident.as_ref() == Some(ident))
                .map(|f| (ident.clone(), f.ty.clone()))
                .ok_or_else(|| {
                    syn::Error::new_spanned(
                        ident,
                        "#[wavedb::key(...)] names a field this struct \
                         does not declare",
                    )
                })
        })
        .collect()
}
