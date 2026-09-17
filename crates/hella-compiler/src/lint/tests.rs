//! H001 is advisory and syntactic: traversal fixtures need not pass sema.
use super::*;

fn parse(src: &str) -> Program {
    let lexed = crate::lexer::lex(src);
    assert!(lexed.errors.is_empty(), "{src}\n{:?}", lexed.errors);
    crate::parse::parse(lexed.tokens, src.to_owned())
        .unwrap_or_else(|e| panic!("{src}\n{e}"))
}

fn check_body(body: &str) -> Vec<Warning> {
    check(&parse(&format!("void main() do\n{body}\nend\n")))
}

#[test]
fn explicit_ref_out_duplicates() {
    for args in [
        "ref x, ref x",
        "ref x, out x",
        "out x, ref x",
        "out x, out x",
        "ref ((x)), ref (x)",
    ] {
        let src = format!("void main() do\nf({args})\nend\n");
        let warnings = check(&parse(&src));
        assert_eq!(warnings.len(), 1, "{args}: {warnings:?}");
        let second = args.split(", ").nth(1).unwrap();
        let start = src.rfind(second).unwrap();
        assert_eq!(warnings[0].span, Span::new(start, start + second.len()));
        assert_eq!(warnings[0].code, "H001");
        assert!(warnings[0].message.contains("`x`"));
        assert!(warnings[0].message.contains("ref/out"));
    }
}

#[test]
fn distinct_names_calls_and_implicit_arguments_are_not_duplicates() {
    for body in [
        "f(ref x, ref y, out z)",
        "f(ref x)\nf(ref x)",
        "f(out x)\ng(out x)",
        "f(x, x)",
        "f(x, ref x)",
        "f(ref x, x)",
        "f(ref obj.x, ref obj.x)",
        "f(ref xs[0], ref xs[0])",
        "f(ref x, g(ref x))",
    ] {
        assert!(check_body(body).is_empty(), "{body}");
    }
}

#[test]
fn typed_out_is_conservative_but_marks_the_name_seen() {
    // Typed out deliberately never warns, even if sema/codegen reuse a visible
    // binding. It resets that name's history and marks it for following args.
    for args in [
        "ref x, out int x",
        "out x, out int x",
        "out int x, out int x",
    ] {
        assert!(check_body(&format!("f({args})")).is_empty(), "{args}");
    }
    for args in [
        "out int x, ref x",
        "out int x, out x",
        "ref x, out int x, ref x",
        "out x, out int x, out x",
        "out int x, out int x, ref x",
        "ref y, out int x, ref y",
    ] {
        let src = format!("void main() do\nf({args})\nend\n");
        let warnings = check(&parse(&src));
        assert_eq!(warnings.len(), 1, "{args}: {warnings:?}");
        let last = args.rsplit(", ").next().unwrap();
        let start = src.rfind(last).unwrap();
        assert_eq!(warnings[0].span, Span::new(start, start + last.len()));
    }
}

#[test]
fn every_repeat_is_reported_and_nested_warnings_are_source_ordered() {
    let src = "void main() do\nf(ref x, g(ref y, out y), out x, ref x)\nend\n";
    let warnings = check(&parse(src));
    let spans: Vec<_> = warnings
        .iter()
        .map(|w| &src[w.span.start..w.span.end])
        .collect();
    assert_eq!(spans, ["out y", "out x", "ref x"]);
    assert!(
        warnings
            .windows(2)
            .all(|ws| ws[0].span.start < ws[1].span.start)
    );
}

#[test]
fn warning_is_advisory_for_a_semantically_valid_program() {
    let src = "void update(ref int a, out int b) do\na = 1\nb = 2\nend\n\
               void main() do\nint x = 0\nupdate(ref x, out x)\nend\n";
    let program = parse(src);
    let errors = crate::sema::check(&program);
    assert!(errors.is_empty(), "sema errors: {errors:?}");
    let warnings = check(&program);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "H001");
    assert_eq!(&src[warnings[0].span.start..warnings[0].span.end], "out x");
}

#[test]
fn call_method_and_constructor_arguments_are_checked() {
    for call in [
        "f(ref x, out x)",
        "obj.f(ref x, out x)",
        "new C(ref x, out x)",
    ] {
        assert_eq!(check_body(call).len(), 1, "{call}");
    }
}

#[test]
fn nested_expression_and_statement_traversal() {
    // These parser-only fixtures isolate each route to a nested call. Unknown
    // names/types are intentional: lint does not depend on semantic resolution.
    for body in [
        "return f(ref x, out x)",
        "int y = 1 + f(ref x, out x)",
        "y = f(ref x, out x)",
        "y += f(ref x, out x)",
        "int y = true ? f(ref x, out x) : 0",
        "int y = -f(ref x, out x)",
        "f(ref x, out x).method()",
        "obj.method(value: f(ref x, out x))",
        "int y = xs[f(ref x, out x)]",
        "int y = xs[0..f(ref x, out x)]",
        "int y = (0, f(ref x, out x))",
        "int arr ys = [f(ref x, out x)]",
        "any c = || => f(ref x, out x)",
        "any c = || do\nreturn f(ref x, out x)\nend",
        "string s = \"value {f(ref x, out x)}\"",
        "defer f(ref x, out x)",
        "defer do\nf(ref x, out x)\nend",
        "delete f(ref x, out x)",
        "if f(ref x, out x) do\nend",
        "if true do\nend else do\nf(ref x, out x)\nend",
        "while f(ref x, out x) do\nend",
        "loop do\nf(ref x, out x)\nend",
        "for i in 0..f(ref x, out x) do\nend",
        "assert true, f(ref x, out x)",
        "int y = match x do\n_ -> f(ref x, out x)\nend",
        "int y = match x do\n_ -> do\nreturn f(ref x, out x)\nend\nend",
        "int y = match x do\n_ if f(ref x, out x) -> 0\nend",
        ".Some(ref x, out x)",
    ] {
        assert_eq!(check_body(body).len(), 1, "{body}");
    }
}

#[test]
fn declaration_defaults_and_object_bodies_are_traversed() {
    for src in [
        "int f(int y = g(ref x, out x)) do\nreturn y\nend",
        "struct S has\nint y = g(ref x, out x)\nend",
        "class C has\nint y = g(ref x, out x)\nend",
        "class C has\nvoid method() do\ng(ref x, out x)\nend\nend",
        "class C has\nC() initialize do\ng(ref x, out x)\nend\nend",
        "class C has\n~C() do\ng(ref x, out x)\nend\nend",
        "class C has\nint value get do\nreturn g(ref x, out x)\nend\nend",
    ] {
        assert_eq!(check(&parse(src)).len(), 1, "{src}");
    }
}
