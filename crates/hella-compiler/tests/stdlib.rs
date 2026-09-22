//! End-to-end stdlib regressions using the public compiler API and a real linker.
//! No prebuilt CLI or installed ~/.hella stdlib; imports resolve to this checkout.
use hella_compiler::{async_req, codegen, lexer, modules, parse, sema};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        // Unique per test: the suite runs tests in parallel and each
        // test rewrites `main.hll`/`test.o`/the binary on every build.
        let dir = root.join("target").join(format!(
            "stdlib-e2e-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn compile(&self, source: &str, opt: codegen::OptLevel) -> PathBuf {
        let entry = self.0.join("main.hll");
        fs::write(&entry, source).unwrap();
        let lexed = lexer::lex(source);
        assert!(lexed.errors.is_empty(), "{:?}", lexed.errors);
        let parsed = parse::parse(lexed.tokens, source.to_owned()).unwrap();
        let expanded = modules::expand_imports(parsed, &entry);
        assert!(expanded.errors.is_empty(), "import expansion failed");
        let errors = sema::check(&expanded.program);
        assert!(errors.is_empty(), "{errors:?}");
        // Modules over `runtime/hella_sync.c` (`std::time`/`sync`/`chan`/`net`,
        // hence `std::log` via `std::time`) need the sync runtime linked,
        // mirroring the CLI's conditional link (B1/B2).
        let needs_sync = async_req::uses_sync_runtime(&expanded.program);
        let object = self.0.join("test.o");
        codegen::compile_to_object(&expanded.program, &object, opt).unwrap();
        let exe = self.0.join(if cfg!(windows) { "test.exe" } else { "test" });
        let mut command = Command::new(
            std::env::var_os("HELLA_LINKER").unwrap_or_else(|| "clang".into()),
        );
        command.arg(&object).arg("-o").arg(&exe);
        if cfg!(windows) {
            command.arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../runtime/hella_rt.c"),
            );
        } else {
            command.arg("-lm");
        }
        if needs_sync {
            command.arg(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/hella_sync.c"),
            );
            if !cfg!(windows) {
                command.arg("-pthread");
            }
        }
        let linked = command
            .output()
            .expect("C linker required (clang or HELLA_LINKER)");
        assert!(
            linked.status.success(),
            "{}",
            String::from_utf8_lossy(&linked.stderr)
        );
        exe
    }
    fn run(&self, exe: &Path, input: &str, success: bool) -> (String, String) {
        let stdin = self.0.join("stdin");
        let stdout = self.0.join("stdout");
        let stderr = self.0.join("stderr");
        fs::write(&stdin, input).unwrap();
        let mut child = Command::new(exe)
            .current_dir(&self.0)
            .stdin(Stdio::from(fs::File::open(stdin).unwrap()))
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("stdlib binary exceeded 10 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let out = fs::read_to_string(stdout).unwrap();
        let err = fs::read_to_string(stderr).unwrap();
        assert_eq!(
            status.success(),
            success,
            "status={status}; stdout={out}; stderr={err}"
        );
        (out, err)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn stdlib_compiles_links_and_runs_debug_and_release() {
    let scratch = Scratch::new("all");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for opt in [codegen::OptLevel::Debug, codegen::OptLevel::Release] {
        for (name, expected) in [
            ("string", "string ok\n"),
            ("fmt", "fmt ok\n"),
            ("rand", "rand ok\n"),
            ("num", "num ok\n"),
            ("math", "doubles ok\nmath ok\n"),
            ("collections", "collections ok\n"),
            ("env", "env ok\n"),
            ("fs", "fs ok\n"),
            ("path", "path ok\n"),
            ("io", "io: ok\n42\nA\nname? hello Ada\nage? 42\n"),
            ("encoding", "encoding ok\n"),
            ("hash", "hash ok\n"),
            ("url", "url ok\n"),
            ("terminal", "stdout is not a terminal\nterminal ok\n"),
            ("time", "time ok\n"),
        ] {
            let source = fs::read_to_string(
                root.join(format!("examples/stdlib_{name}.hll")),
            )
            .unwrap();
            let exe = scratch.compile(&source, opt);
            let (out, err) = scratch.run(&exe, "Ada\n42\n", true);
            assert_eq!(out, expected, "example {name}");
            assert_eq!(err, if name == "io" { "err: ok\n" } else { "" });
        }
        let exe = scratch.compile(
            r#"
import std::io
import std::str
int main() do
    assert equals(readLine(), "")
    assert equals(readLine(), "Ada")
    assert len(readLine()) is 10000
    assert len(readLine()) is 255
    assert len(readLine()) is 256
    assert equals(readLine(), "tail")
    assert equals(readLine(), "")
    assert equals(readLine(), "")
    println("lines ok")
    return 0
end
"#,
            opt,
        );
        let input = format!(
            "\nAda\n{}\n{}\n{}\ntail",
            "x".repeat(10000),
            "x".repeat(255),
            "x".repeat(256)
        );
        assert_eq!(scratch.run(&exe, &input, true).0, "lines ok\n");
        // Check overflow before multiplication/allocation, also in optimized builds.
        for call in [
            "repeat(\"xx\", 9223372036854775807)",
            "padEnd(\"x\", 9223372036854775807)",
        ] {
            let source = format!(
                "import std::fmt\nint main() do\n    string s = {call}\n    return 0\nend\n"
            );
            let exe = scratch.compile(&source, opt);
            scratch.run(&exe, "", false);
        }
        // Log lines carry wall-clock millis (nondeterministic), so match
        // level tags + messages rather than whole lines. Filtering must
        // hide debug/info/warn at LOG_ERROR while errors still show.
        let source = fs::read_to_string(root.join("examples/stdlib_log.hll")).unwrap();
        let exe = scratch.compile(&source, opt);
        let (out, err) = scratch.run(&exe, "", true);
        assert_eq!(out, "log ok\n");
        for visible in [
            "DEBUG debug visible",
            "INFO info visible",
            "WARN warn visible",
            "ERROR error visible",
            "CUSTOM custom visible",
            "ERROR error still visible",
        ] {
            assert!(err.contains(visible), "stderr missing `{visible}`:\n{err}");
        }
        for hidden in ["debug hidden", "info hidden", "warn hidden"] {
            assert!(!err.contains(hidden), "stderr leaked `{hidden}`:\n{err}");
        }
    }
}

#[test]
fn stdlib_selective_imports_keep_transitive_helpers() {
    // A selective import keeps the wanted symbols plus everything they
    // reference — values AND types: `trim` needs `substring`/`allocateString`,
    // `toString` needs its nested `std::str` deps, `join` needs nested
    // `std::vector` deps, `RED` is a const (previously kept nothing),
    // `flip` needs the module-level generator globals, and `count` needs
    // `indexOf`/`indexOfFrom` transitively. (`dateUtc`'s `struct Tm`
    // closure has its own small test below: this giant main already
    // nears a pre-existing debug codegen frame-size crash.)
    let scratch = Scratch::new("selective");
    for opt in [codegen::OptLevel::Debug, codegen::OptLevel::Release] {
        let exe = scratch.compile(
            r#"
import std::io
import std::str::{trim, equals, len, count, equalsIgnoreCase, trimPrefix, splitNextSpace}
import std::terminal::ansi::{RED, RESET}
import std::rand::{seed, flip}
import std::num::{toString}
import std::fmt::{join}
int main() do
    assert equals(trim("  hi  "), "hi")
    assert len(RED) is 5
    println("{RED}hi{RESET}")
    seed(1)
    bool a = flip()
    seed(1)
    assert flip() is a
    assert equals(toString(-42), "-42")
    string vec parts = ["a", "b"]
    assert equals(join(",", parts), "a,b")
    assert count("aaaaa", "aa") is 2
    assert equalsIgnoreCase("HeLLo", "hello") is true
    assert equals(trimPrefix("hello", "hell"), "o")
    int cursor = 0
    assert equals(splitNextSpace("  a b ", ref cursor), "a")
    assert equals(splitNextSpace("  a b ", ref cursor), "b")
    assert cursor is -1
    println("selective ok")
    return 0
end
"#,
            opt,
        );
        assert_eq!(
            scratch.run(&exe, "", true).0,
            "\u{1b}[31mhi\u{1b}[0m\nselective ok\n"
        );
    }
}

#[test]
fn stdlib_selective_imports_keep_type_deps() {
    // Selective imports close over *types* as well as values: `dateUtc`
    // needs `struct Tm` plus its `renderDate`/`twoDigits`/`wallMs` helpers
    // and `concat` from the already-visited `std::str` (diamond merge).
    // Deliberately small: folding this into the giant test above trips a
    // pre-existing debug codegen frame-size crash (EXC_ARM_SP_ALIGN,
    // reproducible on unmodified main — not an import bug).
    let scratch = Scratch::new("seltypes");
    for opt in [codegen::OptLevel::Debug, codegen::OptLevel::Release] {
        let exe = scratch.compile(
            r#"
import std::io
import std::str::{len}
import std::time::{dateUtc}
int main() do
    string du = dateUtc()
    assert len(du) is 19
    println("selective types ok")
    return 0
end
"#,
            opt,
        );
        assert_eq!(scratch.run(&exe, "", true).0, "selective types ok\n");
    }
}
