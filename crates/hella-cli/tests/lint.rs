use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hella-cli-lint-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(
            path.join("hella.toml"),
            "[package]\nname = \"lint-test\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(path.join("src/main.hll"), source).unwrap();
        Self(path)
    }
    fn lint(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_hella"))
            .current_dir(&self.0)
            .args(["lint", "--quiet", "--offline"])
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
const WARNING: &str = "void update(ref int a, out int b) do\na = 1\nb = 2\nend\nvoid main() do\nint x = 0\nupdate(ref x, out x)\nend\n";
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn clean_and_warning_exit_policies_and_explicit_file() {
    let clean = Fixture::new(&WARNING.replace("out x)", "out int y)"));
    let out = clean.lint(&["--deny-warnings"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains("H001"));

    let fixture = Fixture::new(WARNING);
    let out = fixture.lint(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("main.hll:7:15: warning[H001]"),
        "{}",
        stderr(&out)
    );
    let out = fixture.lint(&["-f", "src/main.hll", "--deny-warnings"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("warning[H001]"));
    assert!(stderr(&out).contains("--deny-warnings"));
}

#[test]
fn type_errors_fail_before_linting() {
    let fixture = Fixture::new(&WARNING.replace("int x = 0", "int x = true"));
    for args in [vec![], vec!["--deny-warnings"]] {
        let out = fixture.lint(&args);
        assert!(!out.status.success());
        assert!(stderr(&out).contains("semantic error"), "{}", stderr(&out));
        assert!(!stderr(&out).contains("H001"));
    }
}

#[test]
fn ownership_error_in_interpolation_reports_actual_line_and_column() {
    let source = "struct Pet has\n string name\nend\nvoid main() do\n own Pet a = new Pet(\"x\")\n own Pet b = a\n string s = \"{a.name}\"\nend\n";
    let fixture = Fixture::new(source);
    let out = fixture.lint(&[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("use of moved or deleted value `a`"), "{err}");
    assert!(err.contains("main.hll:7:15"), "{err}");
    assert!(!err.contains("main.hll:1:1"), "{err}");
}

#[test]
fn imports_are_checked_but_not_linted_as_entry_spans() {
    let fixture = Fixture::new("import util\nvoid main() do\nend\n");
    std::fs::write(
        fixture.0.join("src/util.hll"),
        WARNING.replace("void main()", "void exercise()"),
    )
    .unwrap();
    let out = fixture.lint(&["--deny-warnings"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains("H001"));
    let out = fixture.lint(&["-f", "src/util.hll", "--deny-warnings"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("util.hll:7:15: warning[H001]"),
        "{}",
        stderr(&out)
    );
    std::fs::write(
        fixture.0.join("src/util.hll"),
        "int bad() do\nreturn true\nend\n",
    )
    .unwrap();
    let out = fixture.lint(&[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("semantic error"));
}
