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
///
/// The `HELLA_HOME` environment variable, when set to a non-empty path,
/// replaces the home directory outright (hermetic CI, isolated tests):
/// `HELLA_HOME=/tmp/hh` puts the cache in `/tmp/hh/pkg`, and so on.
pub fn hella_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HELLA_HOME") {
        if !h.trim().is_empty() {
            return Some(PathBuf::from(h));
        }
    }    #[cfg(unix)]
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
/// `<pkg_root>/github.com/owner/repo/1.2.3`. Local Windows paths arrive in
/// the Windows-safe `local/__drive_C/...` / `local/__unc__/...` encoding
/// (see `manifest::normalize_git_source`); legacy `local/C:/...` values
/// (with `:`) are migrated on the fly so `create_dir_all` never fails with
/// OS error 123 on Windows.
pub fn pkg_slot_dir(pkg_root: &Path, git: &str, version: &str) -> PathBuf {
    let git = if git.contains([':', '\\']) {
        crate::manifest::normalize_git_source(git)
    } else {
        git.to_string()
    };
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
/// module root per locked dependency in `hella.lock` — the full closure,
/// direct *and* transitive, so a dependency's own `import`s resolve against
/// the importing project's bases. Manifest-only deps without a pin yet fall
/// back to their requested version. Only existing directories are returned.
/// Returns empty when there is no manifest or nothing is fetched yet —
/// plain projects are unaffected.
pub fn dep_bases_for_project(project_root: &Path, pkg_root: &Path) -> Vec<PathBuf> {
    let Ok(Some(manifest)) = crate::manifest::read_manifest_file(project_root) else {
        return Vec::new();
    };
    let locked = crate::manifest::read_lockfile(project_root)
        .ok()
        .flatten()
        .unwrap_or_default();
    if manifest.dependencies.is_empty() && locked.packages.is_empty() {
        return Vec::new();
    }
    let mut bases = Vec::new();
    let mut push_slot = |git: &str, version: &str, package: &Option<String>| {
        let slot = pkg_slot_dir(pkg_root, git, version);
        let root = match package {
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
    };
    // Direct deps first (manifest order), so their modules win ties.
    for (name, req) in &manifest.dependencies {
        // Skip names that could never be imported (defensive: the manifest
        // parser already rejects them, but lockfiles are hand-editable).
        if !crate::manifest::is_valid_dep_name(name) {
            continue;
        }
        match locked.packages.iter().find(|p| &p.name == name) {
            Some(p) => push_slot(&p.git, &p.version, &p.package),
            None => push_slot(&req.git, &req.version, &req.package),
        }
    }
    // Then transitive pins (lock order is by name — deterministic).
    for p in &locked.packages {
        if !crate::manifest::is_valid_dep_name(&p.name) {
            continue;
        }
        push_slot(&p.git, &p.version, &p.package);
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
    /// `@cfg(debug)` truthiness for this expansion (see [`crate::cfg`]).
    debug_mode: bool,
    /// Bare names already inlined from previous imports (first wins). When
    /// two imported modules expose the same symbol, only the first
    /// definition is emitted — no duplicate LLVM symbols, no sema
    /// "duplicate function" error. Bare uses are then diagnosed as ambiguous
    /// (§34) while qualified `alias::sym` resolves to the single kept
    /// declaration. `extern` blocks are exempt (linkage, always kept).
    /// Entry-file definitions bypass this set so shadowing still errors.
    seen_imported: HashSet<String>,
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
    expand_imports_with_cfg(program, importer, false)
}

/// [`expand_imports`] with an explicit `debug` flag for `@cfg(debug)`
/// (the CLI passes `true` only for debug link profiles; `check`/LSP and
/// `--release` pass `false`, so analysis never depends on the link mode).
pub fn expand_imports_with_cfg(program: Program, importer: &Path, debug_mode: bool) -> Expanded {
    let bases = search_bases(importer);
    // Entry-level namespaces first (before `program.items` is consumed):
    // each `import a::b` introduces alias `b` (and full `a::b`) so that
    // `b::sym` / `a::b::sym` resolve to the same declaration as bare `sym`.
    let entry_imports: Vec<ResolvedImport> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Import(imp) => {
                let alias = imp.path.last().cloned().unwrap_or_default();
                let full = imp.path.join("::");
                let requested: Option<Vec<String>> =
                    imp.symbols.as_ref().map(|v| v.iter().map(|(s, _)| s.clone()).collect());
                let provided = match resolve_import(&imp.path, &bases) {
                    Some(path) => collect_provided_names(&path, debug_mode)
                        .map(|mut names| {
                            if let Some(req) = &requested {
                                let want: HashSet<String> = req.iter().cloned().collect();
                                names.retain(|n| want.contains(n));
                                // Keep requested names even if the target file
                                // does not define them (sema reports unknown);
                                // this preserves ambiguity info for typos.
                                for r in req {
                                    if !names.contains(r) {
                                        names.push(r.clone());
                                    }
                                }
                            }
                            names
                        })
                        .unwrap_or_else(|| requested.clone().unwrap_or_default()),
                    None => requested.clone().unwrap_or_default(),
                };
                Some(ResolvedImport {
                    path: imp.path.clone(),
                    alias,
                    full,
                    provided,
                    symbols: requested,
                })
            }
            _ => None,
        })
        .collect();
    let mut ctx = Ctx {
        bases,
        visited: HashSet::new(),
        errors: Vec::new(),
        debug_mode,
        seen_imported: HashSet::new(),
    };
    let span = program.span;
    let mut items = expand_items(program.items, importer, &mut ctx, true);
    // Entry file's own `@cfg(...)` items (imports above already filtered
    // the imported files' item lists).
    crate::cfg::apply_cfg(&mut items, ctx.debug_mode);
    let mut files: Vec<PathBuf> = ctx.visited.into_iter().collect();
    files.push(importer.to_path_buf());
    files.sort();
    Expanded {
        program: Program { items, span },
        errors: ctx.errors,
        files,
        imports: entry_imports,
    }
}

/// Top-level declared names in the file at `path` (after `@cfg` filtering),
/// for namespace / ambiguity bookkeeping. Empty on any I/O/lex/parse error
/// (the import error itself is reported through the normal path).
fn collect_provided_names(path: &Path, debug_mode: bool) -> Option<Vec<String>> {
    let src = std::fs::read_to_string(path).ok()?;
    let toks = crate::lexer::lex(&src);
    if !toks.errors.is_empty() {
        return None;
    }
    let mut sub = crate::parse::parse(toks.tokens, src).ok()?;
    crate::cfg::apply_cfg(&mut sub.items, debug_mode);
    let mut names = Vec::new();
    for it in &sub.items {
        // Skip nested imports: only real declarations provide symbols.
        if matches!(unwrap_item(it), Item::Import(_)) {
            continue;
        }
        if let Some(n) = item_name(it) {
            if !names.contains(&n.to_string()) {
                names.push(n.to_string());
            }
        }
    }
    Some(names)
}

/// One entry-level import that produced a usable module namespace.
///
/// `alias` is the last segment (`std::io` → `io`,
/// `std::terminal::ansi` → `ansi`); `full` is the whole path
/// (`std::io`, `std::terminal::ansi`). `provided` lists the top-level
/// value/type names the module defines (functions, consts, structs,
/// classes, enums, traits, typedefs, vars) — used for qualified
/// resolution (`io::println`) and ambiguity diagnostics.
#[derive(Debug, Clone)]
pub struct ResolvedImport {
    pub path: Vec<String>,
    pub alias: String,
    pub full: String,
    pub provided: Vec<String>,
    /// Explicit symbol list for `import m::{a, b}`; `None` = whole module.
    pub symbols: Option<Vec<String>>,
}

/// Output of [`expand_imports`].
pub struct Expanded {
    pub program: Program,
    pub errors: Vec<ImportError>,
    /// Entry file + all resolved imports (sorted, deduplicated).
    pub files: Vec<PathBuf>,
    /// Entry-level imports in source order (namespace aliases).
    pub imports: Vec<ResolvedImport>,
}

/// Unwrap `@cfg`-surviving attributes for name matching and linkage
/// detection: a surviving `@cfg(unix) extern ...` block is still an
/// extern block, and `@cfg(unix) u64 osEntropy() ...` is still a
/// function named `osEntropy`.
fn unwrap_item(item: &Item) -> &Item {
    match item {
        Item::Attributed { item, .. } => unwrap_item(item),
        other => other,
    }
}

/// Push an imported (non-entry, non-extern) item with first-wins dedup by
/// bare name. Returns true when pushed. Extern blocks must not go through
/// here (they are linkage requirements, always kept).
fn push_imported(out_items: &mut Vec<Item>, ctx: &mut Ctx, it: Item) -> bool {
    if let Some(n) = item_name(&it) {
        if !ctx.seen_imported.insert(n.to_string()) {
            return false;
        }
    }
    out_items.push(it);
    true
}

/// Declared name of a top-level item, if it has one.
fn item_name(item: &Item) -> Option<&str> {
    match unwrap_item(item) {
        Item::Function(f) => Some(&f.name),
        Item::Struct(s) => Some(&s.name),
        Item::Class(c) => Some(&c.name),
        Item::Enum(e) => Some(&e.name),
        Item::Trait(t) => Some(&t.name),
        Item::Typedef(t) => Some(&t.name),
        Item::Distinct(d) => Some(&d.name),
        Item::Const(c) => Some(&c.name),
        Item::Var(v) => Some(&v.name),
        _ => None,
    }
}

fn expand_items(
    items: Vec<Item>,
    importer: &Path,
    ctx: &mut Ctx,
    // True only for the entry file's own item list. Dedup (first import
    // wins) applies here so two imported modules exposing the same symbol
    // yield one definition (§34: bare use is ambiguous, qualified works).
    // Nested frames pass false so selective-closure computation sees
    // complete symbol sets; marking seen there would drop helpers that the
    // parent's selective filter still needs.
    is_entry: bool,
) -> Vec<Item> {
    /// Push an imported item, applying first-wins dedup at the entry level.
    /// Extern blocks always ride along (linkage, never deduped).
    fn push_nested(
        out_items: &mut Vec<Item>,
        ctx: &mut Ctx,
        is_entry: bool,
        it: Item,
    ) {
        if is_entry {
            if matches!(unwrap_item(&it), Item::Extern(_)) {
                out_items.push(it);
            } else {
                push_imported(out_items, ctx, it);
            }
        } else if matches!(unwrap_item(&it), Item::Extern(_)) {
            out_items.push(it);
        } else {
            // Nested frame: no dedup (see `is_entry` docs).
            out_items.push(it);
        }
    }
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
        let mut sub_items = expand_items(sub.items, &path, ctx, false);
        // `@cfg(...)`: drop conditionally-absent items before sema, so no
        // conditional declaration reaches analysis or codegen.
        crate::cfg::apply_cfg(&mut sub_items, ctx.debug_mode);
        if let Some(ref syms) = imp.symbols {
            let wanted: HashSet<String> =
                syms.iter().map(|(s, _)| s.clone()).collect();
            // Selective imports keep the wanted symbols plus everything
            // they reference, so `import std::str::{trim}` still brings
            // `substring`/`allocateString`, `import std::rand::{flip}`
            // still brings the generator globals, and
            // `import std::terminal::ansi::{RED}` finds the const at all.
            // `extern` blocks are linkage requirements, not selectable
            // symbols: they always ride along.
            let mut by_name: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for (i, it) in sub_items.iter().enumerate() {
                if let Some(n) = item_name(it) {
                    by_name.entry(n).or_insert(i);
                }
            }
            let mut keep: HashSet<String> = HashSet::new();
            let mut stack: Vec<String> = Vec::new();
            for n in wanted.iter() {
                if by_name.contains_key(n.as_str()) && keep.insert(n.clone()) {
                    stack.push(n.clone());
                }
            }
            while let Some(name) = stack.pop() {
                let Some(&idx) = by_name.get(name.as_str()) else {
                    continue;
                };
                let mut refs: Vec<String> = Vec::new();
                let mut collect = |e: &crate::ast::Expr| match &e.kind {
                    crate::ast::ExprKind::Call { callee, .. } => {
                        refs.push(callee.clone())
                    }
                    crate::ast::ExprKind::Ident(n) => refs.push(n.clone()),
                    _ => {}
                };
                match unwrap_item(&sub_items[idx]) {
                    Item::Function(f) => {
                        crate::lint::walk::function_exprs(f, &mut collect)
                    }
                    Item::Const(c) => {
                        crate::lint::walk::exprs(&c.init, &mut collect)
                    }
                    Item::Var(v) => {
                        if let Some(init) = &v.init {
                            crate::lint::walk::exprs(init, &mut collect);
                        }
                    }
                    _ => {}
                }
                for r in refs {
                    if by_name.contains_key(r.as_str())
                        && keep.insert(r.clone())
                    {
                        stack.push(r);
                    }
                }
            }
            for it in sub_items {
                match unwrap_item(&it) {
                    Item::Extern(_) => out_items.push(it),
                    _ if item_name(&it).is_some_and(|n| keep.contains(n)) => {
                        push_nested(&mut out_items, ctx, is_entry, it);
                    }
                    _ => {}
                }
            }
        } else {
            for it in sub_items.into_iter().filter(|i| !matches!(i, Item::Import(_))) {
                // Extern blocks always ride along (linkage); named declarations
                // dedup by bare name at the entry level (first import wins —
                // §34 ambiguity + single codegen symbol).
                if matches!(unwrap_item(&it), Item::Extern(_)) {
                    out_items.push(it);
                } else {
                    push_nested(&mut out_items, ctx, is_entry, it);
                }
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
