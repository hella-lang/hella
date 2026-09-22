//! Module-namespace imports (§34-35): `import std::io` makes both
//! `println(..)` and `io::println(..)` (plus `std::io::println(..)`) refer
//! to the same declaration — no duplication. Two modules exposing the same
//! bare symbol make unqualified use ambiguous while qualified use works.
use hella_compiler::{lexer, modules, parse, sema};
use std::fs;
use std::path::{Path, PathBuf};

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .canonicalize()
        .unwrap()
        .join(format!("ns-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Expand `entry_src` written to `entry_name` under `dir`, returning sema
/// messages (with import namespaces registered).
fn check_entry(dir: &Path, entry_name: &str, entry_src: &str) -> Vec<String> {
    let entry = dir.join(entry_name);
    write_file(&entry, entry_src);
    let lexed = lexer::lex(entry_src);
    assert!(lexed.errors.is_empty(), "lex: {:?}", lexed.errors);
    let parsed = parse::parse(lexed.tokens, entry_src.to_owned()).expect("parse");
    let expanded = modules::expand_imports(parsed, &entry);
    assert!(
        expanded.errors.is_empty(),
        "import expansion failed: {:?}",
        expanded.errors
    );
    sema::check_with_options_and_imports(
        &expanded.program,
        sema::CheckOptions { require_main: true },
        &expanded.imports,
    )
    .into_iter()
    .map(|e| e.message)
    .collect()
}

fn std_case(dir: &Path, src: &str) -> Vec<String> {
    // Resolve against the checkout stdlib (ancestor `stdlib/` search).
    check_entry(dir, "main.hll", src)
}

#[test]
fn direct_import_still_works() {
    let dir = scratch_dir("direct");
    let errs = std_case(
        &dir,
        "import std::io\n\nvoid main() do\n    println(\"Hello\")\nend\n",
    );
    assert!(errs.is_empty(), "unexpected: {errs:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn qualified_usage_works() {
    let dir = scratch_dir("qual");
    let errs = std_case(
        &dir,
        "import std::io\n\nvoid main() do\n    io::println(\"Hello\")\nend\n",
    );
    assert!(errs.is_empty(), "unexpected: {errs:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn both_forms_refer_to_same_declaration() {
    let dir = scratch_dir("both");
    let errs = std_case(
        &dir,
        "import std::io\n\nvoid main() do\n    println(\"Hello\")\n    io::println(\"Hello again\")\nend\n",
    );
    assert!(errs.is_empty(), "unexpected: {errs:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn full_path_qualifier_works() {
    let dir = scratch_dir("full");
    let errs = std_case(
        &dir,
        "import std::io\n\nvoid main() do\n    std::io::println(\"full\")\nend\n",
    );
    assert!(errs.is_empty(), "unexpected: {errs:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn qualified_const_works() {
    let dir = scratch_dir("const");
    let errs = std_case(
        &dir,
        "import std::io\nimport std::terminal::ansi\n\nvoid main() do\n    io::println(ansi::RED)\n    println(RED)\nend\n",
    );
    assert!(errs.is_empty(), "unexpected: {errs:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn selective_import_keeps_qualified_access() {
    let dir = scratch_dir("sel");
    let errs = std_case(
        &dir,
        "import std::io::{println}\n\nvoid main() do\n    println(\"sel\")\n    io::println(\"sel qual\")\nend\n",
    );
    assert!(errs.is_empty(), "unexpected: {errs:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unknown_symbol_in_module_is_precise() {
    let dir = scratch_dir("unknown");
    let errs = std_case(
        &dir,
        "import std::io\n\nvoid main() do\n    io::nope(\"x\")\nend\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("unknown symbol `nope`")
            && m.contains("module `io`")),
        "expected unknown-symbol diagnostic, got: {errs:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn without_import_is_still_undefined() {
    let dir = scratch_dir("noimport");
    let errs = std_case(&dir, "void main() do\n    println(\"Hello\")\nend\n");
    assert!(
        errs.iter().any(|m| m.contains("undefined function `println`")),
        "expected undefined-function diagnostic, got: {errs:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ambiguous_bare_use_errors_but_qualified_works() {
    let dir = scratch_dir("amb");
    write_file(&dir.join("foo.hll"), "void something(string s) do\nend\n");
    write_file(&dir.join("bar.hll"), "void something(string s) do\nend\n");
    let amb = check_entry(
        &dir,
        "amb.hll",
        "import foo\nimport bar\n\nvoid main() do\n    something(\"hi\")\nend\n",
    );
    assert!(
        amb.iter().any(|m| m.contains("ambiguous `something`")),
        "expected ambiguity diagnostic, got: {amb:?}"
    );
    let qual = check_entry(
        &dir,
        "qual.hll",
        "import foo\nimport bar\n\nvoid main() do\n    foo::something(\"a\")\n    bar::something(\"b\")\nend\n",
    );
    assert!(qual.is_empty(), "qualified should work, got: {qual:?}");
    let _ = fs::remove_dir_all(&dir);
}
