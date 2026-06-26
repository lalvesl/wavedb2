//! Source scanning: find `#[wavedb]` structs under a `src/` tree and resolve each
//! one's module path.

use std::fs;
use std::path::Path;

use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use syn::{Attribute, Item};

/// One discovered `#[wavedb]` struct: its module path (relative to the crate root)
/// and its identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectEntry {
    /// Module path segments from the crate root (empty = crate root).
    pub module_path: Vec<String>,
    /// The struct identifier.
    pub ident: String,
}

impl ObjectEntry {
    /// The identifier as a token (the `Object` variant name).
    #[must_use]
    pub fn ident_token(&self) -> Ident {
        Ident::new(&self.ident, Span::call_site())
    }

    /// The absolute `crate::…::Ident` path to the struct.
    #[must_use]
    pub fn path_tokens(&self) -> TokenStream {
        let segs: Vec<Ident> = self
            .module_path
            .iter()
            .map(|s| Ident::new(s, Span::call_site()))
            .collect();
        let ident = self.ident_token();
        quote!(crate #(:: #segs)* :: #ident)
    }
}

/// Scan every `.rs` file under `src`, returning the discovered objects sorted by
/// (module path, ident) for deterministic codegen.
///
/// # Errors
/// Returns an error string if the directory can't be read or a file fails to parse.
pub fn scan_dir(src: &Path) -> Result<Vec<ObjectEntry>, String> {
    let mut files = Vec::new();
    collect_rs_files(src, &mut files)?;
    files.sort();

    let mut out = Vec::new();
    for file in &files {
        let rel = file.strip_prefix(src).unwrap_or(file);
        let module_path = module_path_for(rel);
        let content = fs::read_to_string(file)
            .map_err(|e| format!("reading {}: {e}", file.display()))?;
        let parsed = syn::parse_file(&content)
            .map_err(|e| format!("parsing {}: {e}", file.display()))?;
        collect_items(&parsed.items, &module_path, &mut out);
    }

    out.sort();
    Ok(out)
}

/// Recursively gather `.rs` file paths under `dir`.
fn collect_rs_files(
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("reading dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| format!("dir entry in {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Map a `src`-relative file path to its module path.
///
/// `lib.rs` / `main.rs` → crate root; `foo.rs` → `["foo"]`; `foo/mod.rs` →
/// `["foo"]`; `foo/bar.rs` → `["foo", "bar"]`.
fn module_path_for(rel: &Path) -> Vec<String> {
    let mut comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(last) = comps.last_mut() {
        if let Some(stem) = last.strip_suffix(".rs") {
            *last = stem.to_string();
        }
    }
    if let Some("lib" | "main" | "mod") = comps.last().map(String::as_str) {
        comps.pop();
    }
    comps
}

/// Walk items at one module level, recursing into inline `mod { … }` blocks.
fn collect_items(
    items: &[Item],
    module_path: &[String],
    out: &mut Vec<ObjectEntry>,
) {
    for item in items {
        match item {
            Item::Struct(s) if has_wavedb_attr(&s.attrs) => {
                out.push(ObjectEntry {
                    module_path: module_path.to_vec(),
                    ident: s.ident.to_string(),
                });
            }
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    let mut nested = module_path.to_vec();
                    nested.push(m.ident.to_string());
                    collect_items(inner, &nested, out);
                }
            }
            _ => {}
        }
    }
}

/// `true` if any attribute is a bare `#[wavedb]` or `#[wavedb(...)]` (a single
/// `wavedb` path segment) — excludes the `#[wavedb::pivot(...)]` helper.
fn has_wavedb_attr(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("wavedb"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{module_path_for, scan_dir};
    use std::path::Path;

    #[test]
    fn module_path_resolution() {
        assert_eq!(module_path_for(Path::new("lib.rs")), Vec::<String>::new());
        assert_eq!(module_path_for(Path::new("main.rs")), Vec::<String>::new());
        assert_eq!(module_path_for(Path::new("foo.rs")), vec!["foo"]);
        assert_eq!(module_path_for(Path::new("foo/mod.rs")), vec!["foo"]);
        assert_eq!(
            module_path_for(Path::new("foo/bar.rs")),
            vec!["foo", "bar"]
        );
    }

    #[test]
    fn finds_structs_across_files_and_inline_mods() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();

        fs::write(
            src.join("lib.rs"),
            r"
                #[wavedb]
                pub struct AtRoot { pub x: u64 }

                mod inline {
                    #[wavedb(NonUnique)]
                    #[wavedb::pivot(x)]
                    pub struct Inner { pub x: u64 }

                    // not a wavedb struct
                    pub struct Plain;
                }
            ",
        )
        .unwrap();
        fs::write(
            src.join("sub").join("mod.rs"),
            "#[wavedb] pub struct InSub { pub y: u32 }",
        )
        .unwrap();

        let mut got = scan_dir(&src).unwrap();
        got.sort();
        assert_eq!(got.len(), 3);
        assert!(
            got.iter()
                .any(|e| e.module_path.is_empty() && e.ident == "AtRoot")
        );
        assert!(
            got.iter()
                .any(|e| e.module_path == ["inline"] && e.ident == "Inner")
        );
        assert!(
            got.iter()
                .any(|e| e.module_path == ["sub"] && e.ident == "InSub")
        );
        // The non-wavedb struct is ignored.
        assert!(!got.iter().any(|e| e.ident == "Plain"));
    }

    #[test]
    fn missing_src_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_dir(&dir.path().join("nope")).unwrap().is_empty());
    }
}
