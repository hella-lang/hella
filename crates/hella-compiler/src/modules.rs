//! Module resolution shared by the CLI (`hella build/check/run`) and the
//! language server (`hella-lsp`).
//!
//! Two roots, in order:
//!
//! 1. **Project root** — the nearest ancestor of the entry file containing a
//!    `main.hll` (falling back to the entry file's own directory). Every
//!    other `.hll` file next to it is a module (`import foo` → `foo.hll`),
//!    and directories are traversed by qualified paths
//!    (`import net::http` → `net/http.hll`). A bare directory is also
//!    importable through its entry file (`import net` → `net.hll`, else
//!    `net/mod.hll`).
//! 2. **Standard-library roots** — a dev-checkout `stdlib/` found by ancestor
//!    search from the entry (so working in the repo uses live sources),
//!    then `~/.hella/lib` on UNIX-like systems (populated by `hella setup`).
//!
//! Resolution is textual inlining before sema (see `expand_imports`).
//! Cycles (`a` ↔ `b`) are cut by tracking visited files.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast::{Item, Program};
use crate::parse::ParseError;
use crate::token::Span;

/// A failed import. `span` is in the coordinates of `file` (the document
/// containing the failing `import` statement), so CLI and LSP can each
/// decide whether and where to render it.
#[derive(Debug, Clone)]
pub struct ImportError {
    pub message: String,
    pub span: Span,
    pub file: PathBuf,
}

impl From<ImportError> for ParseError {
    fn from(e: ImportError) -> Self {
        ParseError {
            message: e.message,
            span: e.span,
        }
    }
}

/// Standard-library home (`~/.hella/lib` on UNIX-like systems,
/// `%USERPROFILE%\.hella\lib` on Windows). Populated by `hella setup`.
/// `None` elsewhere / when the home variable is missing.
pub fn hella_lib_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".hella").join("lib"))
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .ok()
            .map(|h| PathBuf::from(h).join(".hella").join("lib"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Dev-checkout fallback: nearest ancestor directory (of the entry file)
/// that contains a `stdlib/` directory, e.g. the repo root when working
/// inside it. `None` for entries outside any checkout.
fn workspace_stdlib_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };
    loop {
        let candidate = dir.join("stdlib");
        if candidate.is_dir() {
            return Some(candidate);
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    None
}

/// Project root for an entry file: nearest ancestor holding a `hella.toml`
/// (project manifest), else the nearest ancestor containing a `main.hll`,
/// else the entry's own directory.
pub fn project_root(entry: &Path) -> PathBuf {
    let start = if entry.is_dir() {
        entry.to_path_buf()
    } else {
        entry
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let mut ancestors = vec![start.clone()];
    let mut dir = start;
    while let Some(parent) = dir.parent().map(|p| p.to_path_buf()) {
        ancestors.push(parent.clone());
        dir = parent;
    }
    for marker in ["hella.toml", "main.hll"] {
        for dir in &ancestors {
            if dir.join(marker).is_file() {
                return dir.clone();
            }
        }
    }
    if entry.is_dir() {
        entry.to_path_buf()
    } else {
        entry
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Ordered search bases for an entry file: the entry's own directory
/// (modules next to the entry win), then the project root, then stdlib
/// roots (dev checkout first so repo work uses live sources, then
/// `~/.hella/lib`).
pub fn search_bases(entry: &Path) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    let mut push = |p: PathBuf| {
        if !bases.contains(&p) {
            bases.push(p);
        }
    };
    if entry.is_dir() {
        push(entry.to_path_buf());
    } else if let Some(parent) = entry.parent() {
        push(parent.to_path_buf());
    }
    push(project_root(entry));
    if let Some(ws) = workspace_stdlib_root(entry) {
        push(ws);
    }
    if let Some(home) = hella_lib_dir() {
        push(home);
    }
    bases
}

/// Resolve a qualified import path (`["std", "io"]`) against ordered bases.
/// Tries `<base>/std/io.hll`, then the directory-module entry
/// `<base>/std/io/mod.hll`.
pub fn resolve_import(path: &[String], bases: &[PathBuf]) -> Option<PathBuf> {
    let rel = path.join("/");
    for base in bases {
        let file = base.join(format!("{rel}.hll"));
        if file.is_file() {
            return Some(file);
        }
        let mod_file = base.join(&rel).join("mod.hll");
        if mod_file.is_file() {
            return Some(mod_file);
        }
    }
    None
}

struct Ctx {
    bases: Vec<PathBuf>,
    visited: HashSet<PathBuf>,
    errors: Vec<ImportError>,
}

/// Inline `import` items recursively (textual inclusion before sema).
/// Never fails fatally: unresolvable imports are collected in the returned
/// error list (spans in the importing file's coordinates) and skipped, so
/// the LSP can still check the rest of the document. The CLI treats the
/// first error as fatal, preserving `hella build` behavior.
///
/// `files` is the entry file plus every successfully resolved import —
/// the complete source set a build depends on (used for rebuild checks).
pub fn expand_imports(program: Program, importer: &Path) -> Expanded {
    let mut ctx = Ctx {
        bases: search_bases(importer),
        visited: HashSet::new(),
        errors: Vec::new(),
    };
    let span = program.span;
    let items = expand_items(program.items, importer, &mut ctx);
    let mut files: Vec<PathBuf> = ctx.visited.into_iter().collect();
    files.push(importer.to_path_buf());
    files.sort();
    Expanded {
        program: Program { items, span },
        errors: ctx.errors,
        files,
    }
}

/// Output of [`expand_imports`].
pub struct Expanded {
    pub program: Program,
    pub errors: Vec<ImportError>,
    /// Entry file + all resolved imports (sorted, deduplicated).
    pub files: Vec<PathBuf>,
}

fn expand_items(
    items: Vec<Item>,
    importer: &Path,
    ctx: &mut Ctx,
) -> Vec<Item> {
    let mut out_items: Vec<Item> = Vec::new();
    for item in items {
        let Item::Import(imp) = item else {
            out_items.push(item);
            continue;
        };
        let Some(path) = resolve_import(&imp.path, &ctx.bases) else {
            let searched = ctx
                .bases
                .iter()
                .map(|b| b.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            ctx.errors.push(ImportError {
                message: format!(
                    "cannot resolve import `{}` (looked in {searched})",
                    imp.path.join("::")
                ),
                span: imp.span,
                file: importer.to_path_buf(),
            });
            continue;
        };
        // Cycle guard: same file reached twice (directly or transitively).
        if !ctx.visited.insert(path.clone()) {
            continue;
        }
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                ctx.errors.push(ImportError {
                    message: format!("failed to read import {}: {e}", path.display()),
                    span: imp.span,
                    file: importer.to_path_buf(),
                });
                continue;
            }
        };
        let toks = crate::lexer::lex(&src);
        if !toks.errors.is_empty() {
            ctx.errors.push(ImportError {
                message: format!("lex error in import {}", path.display()),
                span: imp.span,
                file: importer.to_path_buf(),
            });
            continue;
        }
        let sub = match crate::parse::parse(toks.tokens, src.clone()) {
            Ok(p) => p,
            Err(e) => {
                ctx.errors.push(ImportError {
                    message: format!("parse error in {}: {}", path.display(), e.message),
                    span: imp.span,
                    file: importer.to_path_buf(),
                });
                continue;
            }
        };
        let sub_items = expand_items(sub.items, &path, ctx);
        if let Some(ref syms) = imp.symbols {
            let wanted: HashSet<String> = syms.iter().map(|(s, _)| s.clone()).collect();
            for it in sub_items {
                match &it {
                    Item::Function(f) if wanted.contains(&f.name) => out_items.push(it),
                    Item::Struct(s) if wanted.contains(&s.name) => out_items.push(it),
                    Item::Class(c) if wanted.contains(&c.name) => out_items.push(it),
                    Item::Enum(e) if wanted.contains(&e.name) => out_items.push(it),
                    Item::Import(_) => {}
                    // `extern` blocks are linkage requirements, not
                    // selectable symbols: a selective import like
                    // `import std::io::{print}` still needs the libc
                    // declarations its wrappers call into.
                    Item::Extern(_) => out_items.push(it),
                    _ => {}
                }
            }
        } else {
            for it in sub_items.into_iter().filter(|i| !matches!(i, Item::Import(_))) {
                out_items.push(it);
            }
        }
    }
    out_items
}
