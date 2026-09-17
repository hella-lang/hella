use hella_compiler::{lexer, parse, sema, token::Span};

#[test]
fn ownership_errors_in_interpolation_keep_file_offsets() {
    for value in [
        r#""{a.name}""#,
        r#""prefix {  a.name  }""#,
        r#""\n\t\x41\u0042 😀 {a.name}""#,
        "\"\"\"first line\n😀 { a.name }\nlast line\"\"\"",
        r#""{\"nested {a.name}\"}""#,
        r#""{b.name} {a.name}""#,
    ] {
        for action in
            ["own Pet b = a", "delete a\n    own Pet b = new Pet(\"y\")"]
        {
            let source = format!(
                "struct Pet has\n    string name\nend\n\nvoid main() do\n    own Pet a = new Pet(\"x\")\n    {action}\n    string s = {value}\nend\n"
            );
            let lexed = lexer::lex(&source);
            assert!(lexed.errors.is_empty());
            let program = parse::parse(lexed.tokens, source.clone()).unwrap();
            let errors = sema::check(&program);
            assert_eq!(errors.len(), 1, "{source}\n{errors:?}");
            assert_eq!(errors[0].message, "use of moved or deleted value `a`");
            let start = source.rfind("a.name").unwrap();
            assert_eq!(errors[0].span, Span::new(start, start + 1), "{source}");
        }
    }
}

#[test]
fn interpolation_lexemes_still_parse_after_escape_mapping() {
    // Parsing still slices decoded text for literals/names even though spans
    // refer to original file bytes. Include a nested escaped string literal.
    let source = r#"
void main() do
    string s = "\n{12 + 34} {\"\u00e9\"} {true}"
end
"#;
    let lexed = lexer::lex(source);
    let program = parse::parse(lexed.tokens, source.to_owned()).unwrap();
    assert!(sema::check(&program).is_empty());
}
