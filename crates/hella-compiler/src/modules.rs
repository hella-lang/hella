//! Module resolution shared by the CLI (`hella build/check/run`) and the
//! language server (`hella-lsp`).
//!
//! Four roots, in order:
//!
//! 1. **Entry directory** — modules next to the entry file win and can
//!    never be hijacked by a dependency.
//! 2. **Project root** — the nearest ancestor of the entry file containing a
//!    `hella.toml` (falling back to `main.hll`, else the entry's own
//!    directory). Every other `.hll` file next to it is a module
//!    (`import foo` → `foo.hll`), and directories are traversed by qualified
//!    paths (`import net::http` → `net/http.hll`). A bare directory is also
//!    importable through its entry file (`import net` → `net.hll`, else
//!    `net/mod.hll`).
//! 3. **Dependency roots** — one `<pkg>/…/<version>/` slot per dependency
//!    allow-listed in the project's `hella.toml` + `hella.lock`
//!    (`import mylib` → the `mylib` slot). Cached but unlisted deps are
//!    invisible even though they sit on disk.
//! 4. **Standard-library roots** — a dev-checkout `stdlib/` found by ancestor
//!    search from the entry (so working in the repo uses live sources),
//!    then `~/.hella/lib` on UNIX-like systems (populated by `hella setup`,
//!    stdlib-owned — third-party sources never go there).
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

/// Hella home (`~/.hella` on UNIX-like systems,
/// `%USERPROFILE%\.hella` on Windows): parent of the stdlib `lib/` dir, the
/// third-party `pkg/` cache, the `bin/` tool installs, and the `cache/`
/// scratch area. `None` elsewhere / when the home variable is missing.
pub fn hella_home() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".hella"))
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .ok()
            .map(|h| PathBuf::from(h).join(".hella"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Standard-library home (`~/.hella/lib` on UNIX-like systems,
/// `%USERPROFILE%\.hella\lib` on Windows). Populated by `hella setup`.
/// Stdlib-owned: third-party sources must never be placed here (see
/// [`hella_pkg_dir`]). `None` elsewhere / when the home variable is missing.
pub fn hella_lib_dir() -> Option<PathBuf> {
    hella_home().map(|h| h.join("lib"))
}

/// Third-party package cache (`~/.hella/pkg`). Immutable per-version slots
/// populated by `hella add` / `hella fetch`; visibility is enforced by the
/// resolver allow-list ([`dep_search_bases`]), not by filesystem permissions.
pub fn hella_pkg_dir() -> Option<PathBuf> {
    hella_home().map(|h| h.join("pkg"))
}

/// Global tool installs (`~/.hella/bin`, populated by `hella install`).
pub fn hella_bin_dir() -> Option<PathBuf> {
    hella_home().map(|h| h.join("bin"))
}

/// Fetch scratch area (`~/.hella/cache`: clone tarballs, temp dirs).
pub fn hella_cache_dir() -> Option<PathBuf> {
    hella_home().map(|h| h.join("cache"))
}

/// Sanitize a version string for use as a single path segment.
/// Exact pins (`1.2.3`, commit SHAs) pass through; anything else maps
/// non-`[A-Za-z0-9._-]` bytes to `_` so requests can never escape the slot.
pub fn sanitize_version_segment(version: &str) -> String {
    if version.is_empty() {
        return "_".to_string();
    }
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Cache slot for one dependency version: `<pkg_root>/<git>/<version>/`.
/// `git` is already normalized (host + path, no scheme); each `/` becomes
/// a directory level, e.g. `github.com/owner/repo` + `1.2.3` →
/// `<pkg_root>/github.com/owner/repo/1.2.3`.
pub fn pkg_slot_dir(pkg_root: &Path, git: &str, version: &str) -> PathBuf {
    pkg_root
        .join(git.trim_matches('/'))
        .join(sanitize_version_segment(version))
}

/// Module root inside a fetched slot: the slot itself, or the `package`
/// sub-path (`tool::cli` → `tool/cli`) when the Hella module root is not
/// the repo root.
pub fn dep_module_root(pkg_root: &Path, dep: &crate::manifest::LockedDependency) -> PathBuf {
    let slot = pkg_slot_dir(pkg_root, &dep.git, &dep.version);
    match &dep.package {
        Some(pkg) => {
            let mut root = slot;
            for seg in pkg.split("::") {
                root = root.join(seg);
            }
            root
        }
        None => slot,
    }
}

/// Dependency search bases for the project owning `project_root`: one
/// module root per locked dependency in `hella.lock` (falling back to the
/// `hella.toml` request version when no pin exists yet), in manifest order.
/// Only existing directories are returned. Returns empty when there is no
/// manifest, no `pkg` dir, or no dependencies — plain projects are unaffected.
pub fn dep_bases_for_project(project_root: &Path, pkg_root: &Path) -> Vec<PathBuf> {
    let Ok(Some(manifest)) = crate::manifest::read_manifest_file(project_root) else {
        return Vec::new();
    };
    if manifest.dependencies.is_empty() {
        return Vec::new();
    }
    let locked = crate::manifest::read_lockfile(project_root)
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut bases = Vec::new();
    for (name, req) in &manifest.dependencies {
        // Skip names that could never be imported (defensive: the manifest
        // parser already rejects them, but lockfiles are hand-editable).
        if !crate::manifest::is_valid_dep_name(name) {
            continue;
        }
        let pinned = locked.packages.iter().find(|p| &p.name == name);
        let (git, version, package) = match pinned {
            Some(p) => (p.git.clone(), p.version.clone(), p.package.clone()),
            None => (req.git.clone(), req.version.clone(), req.package.clone()),
        };
        let slot = pkg_slot_dir(pkg_root, &git, &version);
        let root = match &package {
            Some(pkg) => {
                let mut r = slot;
                for seg in pkg.split("::") {
                    r = r.join(seg);
                }
                r
            }
            None => slot,
        };
        if root.is_dir() && !bases.contains(&root) {
            bases.push(root);
        }
    }
    bases
}

/// Dependency bases for an entry file: the owning project's locked
/// dependencies. Empty outside a project or when nothing is fetched yet.
pub fn dep_search_bases(entry: &Path) -> Vec<PathBuf> {
    let root = project_root(entry);
    match hella_pkg_dir() {
        Some(pkg) => dep_bases_for_project(&root, &pkg),
        None => Vec::new(),
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
/// (modules next to the entry win and can never be hijacked by a
/// dependency), then the project root, then the project's locked dependency
/// roots (only names allow-listed in `hella.toml` + `hella.lock` — cached
/// but unlisted deps stay invisible), then stdlib roots (dev checkout
/// first so repo work uses live sources, then `~/.hella/lib`).
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
    for dep_base in dep_search_bases(entry) {
        push(dep_base);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hella-mod-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn slot_layout_and_version_sanitization() {
        let pkg = PathBuf::from("/home/u/.hella/pkg");
        assert_eq!(
            pkg_slot_dir(&pkg, "github.com/owner/repo", "1.2.3"),
            pkg.join("github.com/owner/repo/1.2.3")
        );
        // Requests can never escape the slot.
        assert_eq!(sanitize_version_segment(""), "_");
        assert_eq!(sanitize_version_segment("^1.2"), "_1.2");
        assert_eq!(
            pkg_slot_dir(&pkg, "github.com/o/r", "^1.2"),
            pkg.join("github.com/o/r/_1.2")
        );
    }

    #[test]
    fn dep_bases_use_locked_versions_and_hide_unlisted() {
        let scratch = scratch_dir("bases");
        let project = scratch.join("proj");
        let pkg = scratch.join("pkg");
        write_file(
            &project.join("hella.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\
             [dependencies]\n\
             mylib = { git = \"github.com/o/lib\", version = \"^1.0\" }\n\
             tool = { git = \"github.com/o/tool\", version = \"0.4.0\", package = \"tool::cli\" }\n",
        );
        // Lock pins mylib to 1.2.3 even though the request is ^1.0.
        write_file(
            &project.join("hella.lock"),
            "[[package]]\nname = \"mylib\"\ngit = \"github.com/o/lib\"\n\
             version = \"1.2.3\"\nrev = \"abc123\"\n\
             \n[[package]]\nname = \"tool\"\ngit = \"github.com/o/tool\"\n\
             version = \"0.4.0\"\nrev = \"def456\"\npackage = \"tool::cli\"\n",
        );
        // Fetched slots (plus a decoy for an unlisted dep). The `tool` dep
        // declares `package = "tool::cli"`, so its module root is the
        // `tool/cli` subdir of its slot (a directory holding `.hll` files).
        write_file(&pkg.join("github.com/o/lib/1.2.3/mylib.hll"), "// lib\n");
        write_file(&pkg.join("github.com/o/lib/1.0.0/mylib.hll"), "// stale\n");
        write_file(
            &pkg.join("github.com/o/tool/0.4.0/tool/cli/tool.hll"),
            "// cli\n",
        );
        write_file(&pkg.join("github.com/o/evil/9.9.9/evil.hll"), "// evil\n");

        let bases = dep_bases_for_project(&project, &pkg);
        assert_eq!(bases.len(), 2);
        assert_eq!(bases[0], pkg.join("github.com/o/lib/1.2.3"));
        assert_eq!(bases[1], pkg.join("github.com/o/tool/0.4.0/tool/cli"));

        // Unlisted cache entries are invisible to the resolver.
        assert!(
            resolve_import(&["evil".to_string()], &bases).is_none(),
            "unlisted deps must not resolve"
        );
        // Listed deps resolve through their slots.
        assert_eq!(
            resolve_import(&["mylib".to_string()], &bases),
            Some(pkg.join("github.com/o/lib/1.2.3/mylib.hll"))
        );
        // Missing slots are skipped, not returned.
        let _ = std::fs::remove_dir_all(&pkg.join("github.com/o/lib/1.2.3"));
        let bases = dep_bases_for_project(&project, &pkg);
        assert_eq!(bases, vec![pkg.join("github.com/o/tool/0.4.0/tool/cli")]);

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dep_bases_fall_back_to_request_without_lock() {
        let scratch = scratch_dir("nolock");
        let project = scratch.join("proj");
        let pkg = scratch.join("pkg");
        write_file(
            &project.join("hella.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nmylib = \"github.com/o/lib\"\n",
        );
        write_file(&pkg.join("github.com/o/lib/latest/mylib.hll"), "// lib\n");
        let bases = dep_bases_for_project(&project, &pkg);
        assert_eq!(bases, vec![pkg.join("github.com/o/lib/latest")]);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn no_manifest_means_no_dep_bases() {
        let scratch = scratch_dir("plain");
        let pkg = scratch.join("pkg");
        assert!(dep_bases_for_project(&scratch, &pkg).is_empty());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn home_layout_nesting() {
        // Pure nesting check: lib stays stdlib-only, pkg/bin/cache are siblings.
        let home = PathBuf::from("/home/u/.hella");
        assert_eq!(home.join("lib"), home.join("lib"));
        assert_eq!(
            pkg_slot_dir(&home.join("pkg"), "github.com/o/r", "1.0.0"),
            home.join("pkg/github.com/o/r/1.0.0")
        );
    }
}
