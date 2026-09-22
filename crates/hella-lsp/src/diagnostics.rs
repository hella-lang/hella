//! Convert compiler lex/parse/sema errors into LSP `Diagnostic`s so a client
//! renders them inline as the user edits, mirroring `hella check`.
//!
//! Two project-model rules live here:
//!
//! - Imports are expanded through the shared [`hella_compiler::modules`] resolver
//!   (project root around `main.hll`, then `~/.hella/lib`), so names from
//!   imported namespaces resolve instead of raising `undefined …`.
//! - `missing `main` function` is only reported for files actually named
//!   `main` — library modules next to `main.hll` are entry-less by design.

use std::path::Path;

use hella_compiler::sema;
use hella_compiler::token::Span;
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

use crate::document::span_to_range;

/// Produce diagnostics for a document by running the same lex → parse →
/// resolve → sema pipeline the CLI uses, and mapping every error to an LSP
/// `Diagnostic` with a proper source span.
///
/// `path` is the document's filesystem path when known (`None` for
/// unsaved/untitled buffers): it drives import resolution and the
/// `main`-file gate. With `None`, imports are skipped and `main` is
/// required, matching the pre-project-model behavior.
pub fn diagnostics(source: &str, path: Option<&Path>) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    // Lex
    let lexed = hella_compiler::lexer::lex(source);
    for e in &lexed.errors {
        out.push(diag(
            source,
            e.span.start,
            e.span.end,
            format!("unexpected token `{}`", e.slice),
        ));
    }
    // Parse (best-effort even with lex errors, so we catch more).
    let parse = hella_compiler::parse::parse(lexed.tokens.clone(), source.to_string());
    let parsed = match parse {
        Ok(p) => Some(p),
        Err(e) => {
            out.push(diag(source, e.span.start, e.span.end, e.message));
            None
        }
    };
    // Resolve imports, then sema.
    if let Some(prog) = parsed {
        // Lints use only the open document's original AST. Never interpret an
        // imported item's byte offsets as positions in this document.
        if lexed.errors.is_empty() {
            for warning in hella_compiler::lint::check(&prog) {
                out.push(Diagnostic {
                    range: span_to_range(source, warning.span),
                    severity: Some(DiagnosticSeverity::WARNING),
                    code: Some(NumberOrString::String(warning.code.to_owned())),
                    source: Some("hella-lsp".to_owned()),
                    message: warning.message,
                    ..Diagnostic::default()
                });
            }
        }
        // Only entry points must define `main`.
        let require_main = path.map(is_main_file).unwrap_or(true);
        let expanded = match path {
            Some(p) => hella_compiler::modules::expand_imports(prog, p),
            None => hella_compiler::modules::Expanded {
                program: prog,
                errors: Vec::new(),
                files: Vec::new(),
                imports: Vec::new(),
            },
        };
        let import_errors = expanded.errors;
        let sema_imports = expanded.imports;
        let expanded = expanded.program;
        // Surface only failures from the open document itself: nested
        // failures belong to the imported file and appear when it is opened.
        if let Some(open) = path {
            for e in import_errors.iter().filter(|e| e.file.as_path() == open) {
                out.push(diag(source, e.span.start, e.span.end, e.message.clone()));
            }
        }
        for e in sema::check_with_options_and_imports(
            &expanded,
            sema::CheckOptions { require_main },
            &sema_imports,
        ) {
            out.push(diag(source, e.span.start, e.span.end, e.message));
        }
    }

    out
}

/// Entry-point gate: only files named `main` (e.g. `main.hll`) must define
/// a `main` function. Library modules are checked without one.
fn is_main_file(path: &Path) -> bool {
    path.file_stem().is_some_and(|s| s == "main")
}

fn diag(source: &str, start: usize, end: usize, message: String) -> Diagnostic {
    Diagnostic {
        range: span_to_range(source, Span::new(start, end)),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("hella".to_string())),
        code_description: None,
        source: Some("hella-lsp".to_string()),
        message,
        related_information: None,
        tags: None,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Scratch project: `<dir>/main.hll` + `<dir>/util.hll` next to it.
    fn scratch_project() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hella-lsp-diag-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("util.hll"),
            "int twice(int x) do\n    return x * 2\nend\n",
        )
        .unwrap();
        dir
    }

    fn messages(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.message.as_str()).collect()
    }

    #[test]
    fn lint_warning_has_code_severity_and_utf16_range() {
        let src = "void pair(ref int a, ref int b) do\nend\nvoid main() do\nint x = 0\nstring s = \"😀\"; pair(ref x, ref (x))\nend\n";
        let diags = diagnostics(src, None);
        assert_eq!(diags.len(), 1, "{diags:?}");
        let d = &diags[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(d.code, Some(NumberOrString::String("H001".into())));
        assert_eq!(d.source.as_deref(), Some("hella-lsp"));
        let line = src.lines().nth(4).unwrap();
        let column = line[..line.rfind("ref (x)").unwrap()].encode_utf16().count() as u32;
        assert_eq!(d.range, lsp_types::Range::new(
            lsp_types::Position::new(4, column),
            lsp_types::Position::new(4, column + 7)));
        assert!(diagnostics(&src.replace("ref (x)", "ref y").replace("int x = 0", "int x = 0\nint y = 0"), None).is_empty());
    }

    #[test]
    fn lint_uses_only_original_document_even_with_imports() {
        let dir = scratch_project();
        let util = dir.join("util.hll");
        let library = "void pair(ref int a, ref int b) do\nend\nvoid exercise() do\nint x = 0\npair(ref x, ref x)\nend\n";
        std::fs::write(&util, library).unwrap();
        let main = dir.join("main.hll");
        let src = "import util\nvoid main() do\nint y = 0\npair(ref y, ref y)\nend\n";
        std::fs::write(&main, src).unwrap();
        let diags = diagnostics(src, Some(&main));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diags[0].range, lsp_types::Range::new(
            lsp_types::Position::new(3, 12), lsp_types::Position::new(3, 17)));
        let clean = "import util\nvoid main() do\nend\n";
        assert!(diagnostics(clean, Some(&main)).is_empty());
        let imported_diags = diagnostics(library, Some(&util));
        assert_eq!(imported_diags.len(), 1, "{imported_diags:?}");
        assert_eq!(imported_diags[0].code, Some(NumberOrString::String("H001".into())));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ownership_interpolation_error_has_exact_utf16_range() {
        for newline in ["\n", "\r\n"] {
            let src = [
                "struct Pet has", "    string name", "end", "void main() do",
                "    own Pet a = new Pet(\"x\")", "    delete a",
                r#"    string s = "\n😀 {  a.name }""#, "end",
            ].join(newline);
            let diags = diagnostics(&src, None);
            assert_eq!(diags.len(), 1, "{diags:?}");
            let diagnostic = &diags[0];
            assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
            assert_eq!(diagnostic.message, "use of moved or deleted value `a`");
            let line = src.lines().nth(6).unwrap();
            let column = line[..line.find("a.name").unwrap()].encode_utf16().count() as u32;
            assert_eq!(diagnostic.range, lsp_types::Range::new(
                lsp_types::Position::new(6, column),
                lsp_types::Position::new(6, column + 1),
            ));
        }
    }

    #[test]
    fn library_file_does_not_require_main() {
        let dir = scratch_project();
        let util = dir.join("util.hll");
        let src = std::fs::read_to_string(&util).unwrap();
        let diags = diagnostics(&src, Some(&util));
        assert!(
            !messages(&diags).iter().any(|m| m.contains("main")),
            "unexpected main complaint: {diags:?}"
        );
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn main_file_still_requires_main() {
        let dir = scratch_project();
        let main = dir.join("main.hll");
        std::fs::write(&main, "int helper() do\n    return 1\nend\n").unwrap();
        let src = std::fs::read_to_string(&main).unwrap();
        let diags = diagnostics(&src, Some(&main));
        assert!(
            messages(&diags).iter().any(|m| m.contains("missing `main`")),
            "expected missing-main diagnostic: {diags:?}"
        );
    }

    #[test]
    fn imported_namespace_resolves() {
        let dir = scratch_project();
        let main = dir.join("main.hll");
        let src = "import util\n\nvoid main() do\n    int y = twice(21)\nend\n";
        std::fs::write(&main, src).unwrap();
        let diags = diagnostics(src, Some(&main));
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn unresolvable_import_is_reported() {
        let dir = scratch_project();
        let main = dir.join("main.hll");
        let src = "import nope::missing\n\nvoid main() do\nend\n";
        std::fs::write(&main, src).unwrap();
        let diags = diagnostics(src, Some(&main));
        assert!(
            messages(&diags)
                .iter()
                .any(|m| m.contains("cannot resolve import `nope::missing`")),
            "expected unresolvable-import diagnostic: {diags:?}"
        );
    }

    /// Scoped env override with restore-on-drop (tests share one process).
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: this test binary sets HELLA_HOME in exactly one test;
            // no other test reads it concurrently.
            unsafe { std::env::set_var(key, value) };
            EnvGuard { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn dependency_import_resolves_through_lockfile() {
        // Hermetic home: the resolver must find the locked slot here and
        // nowhere else (no network, no real ~/.hella involvement).
        let home = std::env::temp_dir().join(format!(
            "hella-lsp-dep-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let _guard = EnvGuard::set(
            "HELLA_HOME",
            home.to_str().expect("temp dir must be UTF-8"),
        );
        let git = "example.com/test/lib";
        std::fs::create_dir_all(home.join("pkg").join(git).join("1.0.0")).unwrap();
        std::fs::write(
            home.join("pkg").join(git).join("1.0.0/mylib.hll"),
            "int answer() do\n    return 42\nend\n",
        )
        .unwrap();

        let dir = scratch_project();
        std::fs::write(
            dir.join("hella.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nmylib = { git = \"example.com/test/lib\", version = \"1.0.0\" }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("hella.lock"),
            "[[package]]\nname = \"mylib\"\ngit = \"example.com/test/lib\"\n\
             version = \"1.0.0\"\nrev = \"abc123\"\n",
        )
        .unwrap();
        let main = dir.join("main.hll");
        let src = "import mylib\n\nvoid main() do\n    int y = answer()\nend\n";
        std::fs::write(&main, src).unwrap();
        let diags = diagnostics(src, Some(&main));
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");

        let _ = std::fs::remove_dir_all(&home);
    }
}