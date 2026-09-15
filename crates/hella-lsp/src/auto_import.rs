//! Auto-import resolution: map an undefined symbol to its provider file
//! and derive the Hella import path (`a::b::c`) that would make it visible.
//!
//! Shared by two LSP surfaces:
//!
//! - `textDocument/codeAction` quickfix for `undefined …` / `unknown type …`
//! - `textDocument/completion` items with `additionalTextEdits` that insert the import
//!
//! The scan is bounded (≤500 files, ≤20k entries) and reuses
//! [`hella_compiler::modules::search_bases`] so dev-checkout `stdlib/`
//! and `~/.hella/lib` are both considered.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use hella_compiler::ast::Item;

/// Bases to search for a provider file: workspace roots plus the
/// importer-relative bases (project root, stdlib, `~/.hella/lib`).
pub fn import_bases(
    importer: Option<&Path>,
    workspace_roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |p: PathBuf| {
        if seen.insert(p.clone()) {
            bases.push(p);
        }
    };
    for r in workspace_roots {
        push(r.clone());
    }
    if let Some(imp) = importer {
        for b in hella_compiler::modules::search_bases(imp) {
            push(b);
        }
    } else if let Some(first) = workspace_roots.first() {
        // No open document path (untitled buffer) but we have a workspace:
        // still surface stdlib + project root via a synthetic entry.
        for b in hella_compiler::modules::search_bases(&first.join("main.hll")) {
            push(b);
        }
    } else {
        if let Some(home) = hella_compiler::modules::hella_lib_dir() {
            push(home);
        }
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(ws) = workspace_stdlib_root(&cwd) {
                push(ws);
            }
        }
    }
    bases
}

fn workspace_stdlib_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent().map(|p| p.to_path_buf()).unwrap_or_default()
    };
    loop {
        if dir.join("stdlib").is_dir() {
            return Some(dir.join("stdlib"));
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => return None,
        }
    }
}

/// Import path for `file` that lives under `base` (`std/io.hll` → `std::io`).
/// `None` when `file` is not under `base` or is an unimportable entry
/// (`main.hll` at the project root is not a module).
pub fn import_path_for(base: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(base).ok()?;
    if rel == Path::new("main.hll") {
        return None;
    }
    let s = rel.to_string_lossy();
    if s == "mod.hll" {
        return None;
    }
    if rel.ends_with("mod.hll") {
        let parent = rel.parent()?;
        if parent.as_os_str().is_empty() {
            return None;
        }
        return Some(
            parent
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("::"),
        );
    }
    let without_ext = rel.with_extension("");
    Some(
        without_ext
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("::"),
    )
}

/// Collect `.hll` files under `bases` (deduped, bounded). Returns
/// `(base, file)` pairs so the import path can be derived.
pub fn collect_hll_files(bases: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    const BUDGET: usize = 20_000;
    const FILE_CAP: usize = 500;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut visited: usize = 0;
    let mut stack: Vec<PathBuf> = bases.to_vec();
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > BUDGET || out.len() >= FILE_CAP {
                return out;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if name == "target" || name == "out" || name == "node_modules" {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() && path.extension().is_some_and(|e| e == "hll") {
                if seen.insert(path.clone()) {
                    out.push((dir.clone(), path));
                } else {
                    // Same file reachable via two bases — keep first base's path.
                }
            }
        }
    }
    // Fallback: files directly under bases that are themselves files are missed
    // by read_dir loop when bases contain file paths; handled by caller via
    // search_bases which only returns directories, so no action here.
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

/// Top-level names defined in `src` (first occurrence per name, no locals).
fn top_names(src: &str) -> HashSet<String> {
    let out = hella_compiler::lexer::lex(src);
    let prog = match hella_compiler::parse::parse(out.tokens, src.to_string()) {
        Ok(p) => p,
        Err(_) => return HashSet::new(),
    };
    let mut names = HashSet::new();
    for item in &prog.items {
        match item {
            Item::Function(f) => { names.insert(f.name.clone()); }
            Item::Struct(s) => { names.insert(s.name.clone()); }
            Item::Class(c) => { names.insert(c.name.clone()); }
            Item::Enum(e) => { names.insert(e.name.clone()); }
            Item::Trait(t) => { names.insert(t.name.clone()); }
            Item::Typedef(t) => { names.insert(t.name.clone()); }
            Item::Distinct(d) => { names.insert(d.name.clone()); }
            Item::Const(c) => { names.insert(c.name.clone()); }
            Item::Var(v) => { names.insert(v.name.clone()); }
            Item::Extern(e) => {
                for m in &e.members {
                    match m {
                        hella_compiler::ast::ExternMember::Function { name, .. } => { names.insert(name.clone()); }
                        hella_compiler::ast::ExternMember::Struct { name, .. } => { names.insert(name.clone()); }
                        hella_compiler::ast::ExternMember::Enum { name, .. } => { names.insert(name.clone()); }
                        hella_compiler::ast::ExternMember::Const { name, .. } => { names.insert(name.clone()); }
                    }
                }
            }
            _ => {}
        }
    }
    names
}

/// First import path that provides `symbol`, scanning `bases`.
/// Skips `importer` itself and any `main.hll` entry.
pub fn find_import_for_symbol(
    symbol: &str,
    importer: Option<&Path>,
    workspace_roots: &[PathBuf],
) -> Option<String> {
    let bases = import_bases(importer, workspace_roots);
    let files = collect_hll_files(&bases);
    // Collect candidates, dedup by import path (first wins, deterministic).
    let mut best: Option<(String, PathBuf)> = None;
    for (base, file) in files {
        if let Some(imp) = importer {
            if file == imp {
                continue;
            }
        }
        let import = import_path_for(&base, &file)?;
        // Avoid offering `main` as an importable module (already filtered).
        let src = std::fs::read_to_string(&file).ok()?;
        if top_names(&src).contains(symbol) {
            match &best {
                Some((prev_import, _)) if prev_import <= &import => continue,
                _ => best = Some((import, file)),
            }
        }
    }
    best.map(|(imp, _)| imp)
}

/// Line where a new `import` should be inserted (after the last import, else 0).
pub fn import_insertion_line(text: &str) -> u32 {
    let mut last: Option<usize> = None;
    for (idx, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("import ") {
            last = Some(idx);
        }
    }
    last.map(|i| i as u32 + 1).unwrap_or(0)
}

/// All `(symbol, import)` pairs whose symbol starts with `prefix`
/// (case-insensitive) and that are not already defined in `existing`.
/// Bounded by the same file cap; results sorted by symbol.
pub fn candidates_with_prefix(
    prefix: &str,
    importer: Option<&Path>,
    workspace_roots: &[PathBuf],
    existing: &HashSet<String>,
) -> Vec<(String, String)> {
    let lower = prefix.to_lowercase();
    let bases = import_bases(importer, workspace_roots);
    let files = collect_hll_files(&bases);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen_sym: HashSet<String> = HashSet::new();
    for (base, file) in files {
        if let Some(imp) = importer {
            if file == imp {
                continue;
            }
        }
        let import = match import_path_for(&base, &file) {
            Some(p) => p,
            None => continue,
        };
        let src = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for name in top_names(&src) {
            if existing.contains(&name) || !seen_sym.insert(name.clone()) {
                continue;
            }
            if lower.is_empty() || name.to_lowercase().starts_with(&lower) {
                out.push((name, import.clone()));
            }
        }
        if out.len() >= 100 {
            break;
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hella-auto-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn import_path_derivation() {
        let base = PathBuf::from("/proj");
        assert_eq!(import_path_for(&base, &PathBuf::from("/proj/utils.hll")).unwrap(), "utils");
        assert_eq!(import_path_for(&base, &PathBuf::from("/proj/net/http.hll")).unwrap(), "net::http");
        assert_eq!(import_path_for(&base, &PathBuf::from("/proj/net/mod.hll")).unwrap(), "net");
        assert_eq!(import_path_for(&base, &PathBuf::from("/proj/main.hll")), None);
        assert_eq!(import_path_for(&base, &PathBuf::from("/other/utils.hll")), None);
    }

    #[test]
    fn finds_provider_in_workspace() {
        let root = tmp_root("find");
        std::fs::write(root.join("utils.hll"), "int helper(int x) do\n  return x\nend\n").unwrap();
        std::fs::write(root.join("main.hll"), "void main() do\nend\n").unwrap();
        let imp = root.join("main.hll");
        let got = find_import_for_symbol("helper", Some(&imp), &[root.clone()]);
        assert_eq!(got.as_deref(), Some("utils"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_self_import() {
        let root = tmp_root("self");
        std::fs::write(root.join("main.hll"), "int helper(int x) do\n  return x\nend\nvoid main() do\nend\n").unwrap();
        let imp = root.join("main.hll");
        assert_eq!(find_import_for_symbol("helper", Some(&imp), &[root.clone()]), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prefix_candidates() {
        let root = tmp_root("prefix");
        std::fs::write(root.join("alpha.hll"), "int alpha_one() do\n  return 1\nend\nint alpha_two() do\n  return 2\nend\n").unwrap();
        std::fs::write(root.join("beta.hll"), "int beta_one() do\n  return 1\nend\n").unwrap();
        let existing = HashSet::new();
        let got = candidates_with_prefix("alp", None, &[root.clone()], &existing);
        assert!(got.iter().any(|(n, _)| n == "alpha_one"), "got {got:?}");
        assert!(got.iter().any(|(n, _)| n == "alpha_two"), "got {got:?}");
        assert!(!got.iter().any(|(n, _)| n == "beta_one"), "prefix filtered {got:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
