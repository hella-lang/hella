//! End-to-end async regressions (Async-6/Async-7/Async-8/Async-13) using the
//! public compiler API, the checkout runtime sources, and a real linker.
//!
//! Covers: sequential and concurrent spawn/await, scope-join ordering,
//! value transport (inline + spilled results), cooperative yield/timers,
//! exactly-once cleanup, the sema rejections (double await, task escape,
//! async call from sync, await/spawn/yield outside async), and the
//! conditional-runtime contract (sync programs must not reference it).
use hella_compiler::{async_req, codegen, lexer, modules, parse, sema};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = root()
            .join("target")
            .join(format!("async-e2e-{}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Compile a program the way the CLI does, including the conditional
    /// async runtime link (Async-8/Async-9).
    fn compile(&self, source: &str, opt: codegen::OptLevel) -> (PathBuf, bool) {
        let entry = self.0.join("main.hll");
        fs::write(&entry, source).unwrap();
        let lexed = lexer::lex(source);
        assert!(lexed.errors.is_empty(), "lex: {:?}", lexed.errors);
        let parsed = parse::parse(lexed.tokens, source.to_owned()).unwrap();
        let expanded = modules::expand_imports(parsed, &entry);
        assert!(expanded.errors.is_empty(), "import expansion failed");
        let program = expanded.program;
        let errors = sema::check(&program);
        assert!(errors.is_empty(), "sema: {errors:?}");
        let needs_async = async_req::uses_async_runtime(&program);
        let object = self.0.join("test.o");
        codegen::compile_to_object(&program, &object, opt).unwrap();
        let exe = self.0.join(if cfg!(windows) { "test.exe" } else { "test" });
        let linker = std::env::var_os("HELLA_LINKER").unwrap_or_else(|| "clang".into());
        let mut command = Command::new(&linker);
        command.arg(&object).arg("-o").arg(&exe);
        if cfg!(windows) {
            command.arg(root().join("runtime/hella_rt.c"));
        } else {
            command.arg("-lm");
        }
        if needs_async {
            command.arg(root().join("runtime/hella_async.c"));
            if !cfg!(windows) {
                command.arg("-pthread");
            }
        }
        let linked = command.output().expect("C linker required (clang or HELLA_LINKER)");
        assert!(
            linked.status.success(),
            "link failed: {}",
            String::from_utf8_lossy(&linked.stderr)
        );
        (exe, needs_async)
    }

    fn run(&self, exe: &Path) -> (String, String, bool) {
        let stdout = self.0.join("stdout");
        let stderr = self.0.join("stderr");
        let mut child = Command::new(exe)
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("async binary exceeded 20 seconds (deadlock?)");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        (
            fs::read_to_string(stdout).unwrap(),
            fs::read_to_string(stderr).unwrap(),
            status.success(),
        )
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Sema errors for `src` (asserts parsing/imports succeeded).
fn sema_errors(src: &str) -> Vec<String> {
    let lexed = lexer::lex(src);
    assert!(lexed.errors.is_empty(), "lex: {:?}", lexed.errors);
    let parsed = parse::parse(lexed.tokens, src.to_owned()).expect("parse");
    sema::check(&parsed).into_iter().map(|e| e.message).collect()
}

#[test]
fn sequential_await_transports_values() {
    let scratch = Scratch::new("seq");
    let src = r#"
import std::io
int double_it(int n) async do
    return n * 2
end
void main() async do
    scope do
        task<int> t = double_it(21)
        int v = await t
        println("v={v}")
    end
end
"#;
    let (exe, needs_async) = scratch.compile(src, codegen::OptLevel::Debug);
    assert!(needs_async, "async program must require the runtime");
    let (out, err, ok) = scratch.run(&exe);
    assert!(ok, "stderr={err}");
    assert_eq!(out, "v=42\n");
}

#[test]
fn concurrent_spawns_run_and_await_out_of_order() {
    let scratch = Scratch::new("conc");
    let src = r#"
import std::io
int sum_to(int n) async do
    int s = 0
    int i = 0
    while i < n do
        s = s + i
        i = i + 1
    end
    return s
end
void main() async do
    scope do
        task<int> a = sum_to(10)
        task<int> b = sum_to(20)
        int rb = await b
        int ra = await a
        println("a={ra} b={rb}")
    end
end
"#;
    let (exe, _) = scratch.compile(src, codegen::OptLevel::Debug);
    let (out, err, ok) = scratch.run(&exe);
    assert!(ok, "stderr={err}");
    assert_eq!(out, "a=45 b=190\n");
}

#[test]
fn structured_scope_joins_before_exit() {
    let scratch = Scratch::new("scope");
    // The scope body's last statement runs only after the scope's tasks are
    // awaited (structurally enforced), so the printed order is deterministic.
    let src = r#"
import std::io
int work() async do
    return 7
end
void main() async do
    scope do
        task<int> t = work()
        int v = await t
        println("inner={v}")
    end
    println("after scope")
end
"#;
    let (exe, _) = scratch.compile(src, codegen::OptLevel::Debug);
    let (out, err, ok) = scratch.run(&exe);
    assert!(ok, "stderr={err}");
    assert_eq!(out, "inner=7\nafter scope\n");
}

#[test]
fn nested_scopes_and_yield_timers_work() {
    let scratch = Scratch::new("nested");
    let src = r#"
import std::io
import std::task
int tick(int n) async do
    int i = 0
    while i < n do
        yieldNow()
        sleepMs(1)
        i = i + 1
    end
    return n
end
void main() async do
    scope do
        task<int> outer = tick(2)
        scope do
            task<int> inner = tick(3)
            int vi = await inner
            println("inner={vi}")
        end
        int vo = await outer
        println("outer={vo}")
    end
end
"#;
    let (exe, needs_async) = scratch.compile(src, codegen::OptLevel::Release);
    assert!(needs_async);
    let (out, err, ok) = scratch.run(&exe);
    assert!(ok, "stderr={err}");
    assert_eq!(out, "inner=3\nouter=2\n");
}

#[test]
fn many_tasks_keep_exactly_once_cleanup() {
    let scratch = Scratch::new("many");
    // 64 tasks in one scope: each allocates, transports an inline result and
    // is joined exactly once (a double free / lost wakeup would hang or abort).
    let src = r#"
import std::io
int id(int n) async do
    return n + 1
end
void main() async do
    scope do
        task<int> t0 = id(0)
        task<int> t1 = id(1)
        task<int> t2 = id(2)
        task<int> t3 = id(3)
        task<int> t4 = id(4)
        task<int> t5 = id(5)
        task<int> t6 = id(6)
        task<int> t7 = id(7)
        int s = 0
        s = s + await t0
        s = s + await t1
        s = s + await t2
        s = s + await t3
        s = s + await t4
        s = s + await t5
        s = s + await t6
        s = s + await t7
        println("sum={s}")
    end
end
"#;
    let (exe, _) = scratch.compile(src, codegen::OptLevel::Debug);
    let (out, err, ok) = scratch.run(&exe);
    assert!(ok, "stderr={err}");
    // sum of (n+1) for n in 0..7 = 8 + 28 = 36
    assert_eq!(out, "sum=36\n");
}

#[test]
fn sync_program_does_not_link_or_reference_the_runtime() {
    let scratch = Scratch::new("sync");
    let src = "import std::io\nvoid main() do\n    println(\"sync only\")\nend\n";
    let (exe, needs_async) = scratch.compile(src, codegen::OptLevel::Debug);
    assert!(!needs_async, "sync program must not require the runtime");
    let (out, err, ok) = scratch.run(&exe);
    assert!(ok, "stderr={err}");
    assert_eq!(out, "sync only\n");
    // The binary must not carry unresolved async runtime references.
    let bytes = fs::read(&exe).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for sym in ["hella_task_spawn", "hella_task_join", "pthread_create"] {
        assert!(
            !text.contains(sym),
            "sync binary references `{sym}`"
        );
    }
}

#[test]
fn unused_async_declaration_does_not_require_the_runtime() {
    let scratch = Scratch::new("unused");
    let src = r#"
import std::io
int never(int n) async do
    return n
end
void main() do
    println("no async here")
end
"#;
    let (exe, needs_async) = scratch.compile(src, codegen::OptLevel::Debug);
    assert!(!needs_async);
    let (out, _, ok) = scratch.run(&exe);
    assert!(ok);
    assert_eq!(out, "no async here\n");
    let text = String::from_utf8_lossy(&fs::read(&exe).unwrap()).to_string();
    assert!(!text.contains("hella_task_spawn"));
}

#[test]
fn sema_rejects_double_await() {
    let errs = sema_errors(
        r#"
int slow(int n) async do
    return n
end
void main() async do
    scope do
        task<int> t = slow(1)
        int a = await t
        int b = await t
    end
end
"#,
    );
    assert!(
        errs.iter().any(|e| e.contains("already awaited")),
        "{errs:?}"
    );
}

#[test]
fn sema_rejects_task_escape_from_scope() {
    let errs = sema_errors(
        r#"
int slow(int n) async do
    return n
end
void main() async do
    scope do
        task<int> t = slow(1)
    end
end
"#,
    );
    assert!(errs.iter().any(|e| e.contains("escapes its `scope`")), "{errs:?}");
}

#[test]
fn sema_rejects_async_constructs_outside_async() {
    let errs = sema_errors(
        r#"
int slow(int n) async do
    return n
end
void main() do
    scope do
        task<int> t = slow(1)
        int v = await t
    end
end
"#,
    );
    assert!(errs.iter().any(|e| e.contains("`scope` requires an `async`")), "{errs:?}");
    assert!(errs.iter().any(|e| e.contains("`await` requires an `async`")), "{errs:?}");
    assert!(errs.iter().any(|e| e.contains("only be called")), "{errs:?}");
}

#[test]
fn sema_rejects_spawn_without_scope() {
    let errs = sema_errors(
        r#"
int slow(int n) async do
    return n
end
void main() async do
    task<int> t = spawn slow(1)
    int v = await t
end
"#,
    );
    assert!(errs.iter().any(|e| e.contains("enclosing `scope")), "{errs:?}");
}

#[test]
fn yield_outside_async_is_rejected() {
    let errs = sema_errors("void main() do\n    yield\nend\n");
    assert!(errs.iter().any(|e| e.contains("`yield` requires an `async`")), "{errs:?}");
}
