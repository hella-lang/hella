//! Conditional compilation (`@cfg(...)`) end-to-end: expansion drops
//! absent items before sema, so mutually exclusive declarations never
//! collide, and profile-dependent items follow the link profile.
use hella_compiler::{cfg, lexer, modules, parse, sema};
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Expand `src` as if built from a scratch directory (no imports needed).
fn expand(src: &str, debug_mode: bool) -> hella_compiler::ast::Program {
    let dir = root().join("target").join("cfg-expand");
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("probe.hll");
    fs::write(&entry, src).unwrap();
    let lexed = lexer::lex(src);
    assert!(lexed.errors.is_empty(), "lex: {:?}", lexed.errors);
    let parsed = parse::parse(lexed.tokens, src.to_string()).expect("parse");
    let expanded = modules::expand_imports_with_cfg(parsed, &entry, debug_mode);
    assert!(expanded.errors.is_empty(), "expansion: {:?}", expanded.errors);
    expanded.program
}

fn fn_names(prog: &hella_compiler::ast::Program) -> Vec<String> {
    use hella_compiler::ast::Item;
    prog.items
        .iter()
        .filter_map(|i| match i {
            Item::Function(f) => Some(f.name.clone()),
            // Surviving items keep their attributes (`cfg::apply_cfg` only
            // drops whole items), so unwrap for the name list.
            Item::Attributed { item, .. } => match item.as_ref() {
                Item::Function(f) => Some(f.name.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn absent_os_variant_is_dropped_before_sema() {
    // Two declarations of the same function under mutually exclusive OS
    // conditions: exactly one survives, so sema sees no duplicate.
    let src = r#"
@cfg(os = "definitely_not_an_os")
int which() do
    return 1
end

@cfg(os = "also_not_an_os")
int other() do
    return 2
end
"#;
    let prog = expand(src, true);
    assert!(fn_names(&prog).is_empty(), "{:?}", fn_names(&prog));
}

#[test]
fn host_os_variant_survives_and_alternatives_do_not() {
    let host = cfg::host_os();
    let src = format!(
        r#"
@cfg(os = "{host}")
int host_only() do
    return 1
end

@cfg(os = "not_{host}_at_all")
int never() do
    return 2
end
"#
    );
    let prog = expand(&src, true);
    assert_eq!(fn_names(&prog), vec!["host_only".to_string()]);
}

#[test]
fn or_chains_keep_the_item_when_any_condition_holds() {
    let host = cfg::host_os();
    let src = format!(
        r#"
@cfg(os = "nope" or os = "{host}")
int kept() do
    return 1
end

@cfg(os = "nope" or os = "alsonope")
int dropped() do
    return 2
end
"#
    );
    let prog = expand(&src, true);
    assert_eq!(fn_names(&prog), vec!["kept".to_string()]);
}

#[test]
fn comma_and_or_are_equivalent() {
    let host = cfg::host_os();
    let src = format!(
        "@cfg(os = \"nope\", os = \"{host}\")\nint kept() do\n    return 1\nend\n"
    );
    let prog = expand(&src, true);
    assert_eq!(fn_names(&prog), vec!["kept".to_string()]);
}

#[test]
fn debug_condition_follows_the_link_profile() {
    let src = r#"
@cfg(debug)
int only_debug() do
    return 1
end

@cfg(debug = false)
int only_release_or_check() do
    return 2
end
"#;
    assert_eq!(fn_names(&expand(src, true)), vec!["only_debug".to_string()]);
    assert_eq!(
        fn_names(&expand(src, false)),
        vec!["only_release_or_check".to_string()]
    );
}

#[test]
fn target_condition_matches_exact_triple() {
    let triple = cfg::HOST_TRIPLE;
    let src = format!(
        "@cfg(target = \"{triple}\")\nint exact() do\n    return 1\nend\n"
    );
    assert_eq!(fn_names(&expand(&src, true)), vec!["exact".to_string()]);
    let wrong = "@cfg(target = \"definitely-not-a-triple\")\nint no() do\n    return 1\nend\n";
    assert!(fn_names(&expand(wrong, true)).is_empty());
}

#[test]
fn unknown_condition_drops_the_item() {
    let src = "@cfg(feature = \"simd\")\nint gated() do\n    return 1\nend\n";
    assert!(fn_names(&expand(src, true)).is_empty());
}

#[test]
fn stacked_cfg_attributes_all_must_hold() {
    let host = cfg::host_os();
    let src = format!(
        r#"
@cfg(os = "{host}")
@cfg(debug)
int both() do
    return 1
end

@cfg(os = "{host}")
@cfg(debug = false)
int os_only() do
    return 2
end
"#
    );
    assert_eq!(fn_names(&expand(&src, true)), vec!["both".to_string()]);
    assert_eq!(
        fn_names(&expand(&src, false)),
        vec!["os_only".to_string()]
    );
}

#[test]
fn non_cfg_attributes_are_untouched() {
    let src = "@inline\n@doc(text: \"hi\")\nint kept() do\n    return 1\nend\n";
    let prog = expand(src, true);
    assert_eq!(fn_names(&prog), vec!["kept".to_string()]);
    // The item stays attributed (attributes are not consumed by cfg).
    match &prog.items[0] {
        hella_compiler::ast::Item::Attributed { attrs, .. } => assert_eq!(attrs.len(), 2),
        other => panic!("expected attributed item, got {other:?}"),
    }
}

#[test]
fn gated_items_are_sema_checked_when_present() {
    // A surviving gated item participates in normal analysis.
    let host = cfg::host_os();
    let src = format!(
        r#"
@cfg(os = "{host}")
int broken() do
    return missing_name
end

void main() do
    int x = 1
end
"#
    );
    let prog = expand(&src, true);
    let errors = sema::check(&prog);
    assert!(
        errors.iter().any(|e| e.message.contains("missing_name") || e.message.contains("undefined")),
        "{errors:?}"
    );
}

#[test]
fn dropped_items_are_never_sema_checked() {
    // Identical body, condition false: no error, because the item is gone.
    let src = r#"
@cfg(os = "definitely_not_an_os")
int broken() do
    return missing_name
end

void main() do
    int x = 1
end
"#;
    let prog = expand(src, true);
    let errors = sema::check(&prog);
    assert!(errors.is_empty(), "{errors:?}");
}
