//! Conditional compilation (`@cfg(...)`, EBNF §34 attribute).
//!
//! `@cfg(...)` marks a top-level item as present only when the condition
//! holds. Evaluation happens during import expansion (before sema), so a
//! dropped item is invisible to every downstream pass — no conditional
//! code survives to codegen.
//!
//! Grammar (v1):
//!
//! ```hll
//! @cfg(os = "macos")
//! @cfg(os = "linux" or os = "windows")
//! @cfg(target = "aarch64-apple-darwin")
//! @cfg(debug)              // truthy only in `hella build`/`run` debug
//! @cfg(debug = false)      // truthy in release / `check`/LSP
//! void platform_thing() do ... end
//! ```
//!
//! Semantics:
//! * `os = "<name>"` matches the OS component of the host triple
//!   (`macos`, `linux`, `windows`). `or` joins subconditions (any true
//!   wins); there is no `and` / `not` yet — they are a parse error.
//! * `target = "<triple>"` matches the full host triple exactly
//!   (e.g. `aarch64-apple-darwin`).
//! * `debug` (bare, no value) is true only for `hella build`/`run`
//!   debug profiles; `debug = false` inverts it. Everywhere else
//!   (`check`, `lint`, LSP, `--release`) `debug` is false — analysis
//!   never depends on the link profile, matching how `debug_assert`
//!   stays sema-checked but codegen-stripped.
//! * Unknown conditions evaluate to false (forward compatibility: a
//!   toolchain that doesn't know the condition drops the item rather
//!   than guessing).

use crate::ast::{Attribute, AttributeArg, Expr, ExprKind, Item};

/// Host triple of the compiling toolchain (from `env!` at build time).
///
/// Populated via `rustc -vV` in `crates/hella-compiler/build.rs`; when the
/// toolchain triple is unavailable (shouldn't happen in practice), the
/// fallback is a conservative `unknown-unknown-unknown` that makes every
/// `target = "..."` condition false.
pub const HOST_TRIPLE: &str = env!("HELLA_HOST_TRIPLE");

/// OS component of the host triple (`macos` for `*-apple-darwin`, `linux`
/// for `*-linux-*`, `windows` for `*-windows-*` / `*-pc-windows-*`).
pub fn host_os() -> &'static str {
    let t = HOST_TRIPLE;
    if t.contains("darwin") || t.contains("apple") {
        "macos"
    } else if t.contains("linux") {
        "linux"
    } else if t.contains("windows") {
        "windows"
    } else {
        "unknown"
    }
}

/// True when `attr` is a `@cfg(...)` condition (vs some other attribute).
pub fn is_cfg(attr: &Attribute) -> bool {
    attr.name == "cfg"
}

/// Evaluate a single `@cfg(...)` condition. `debug_mode` is true only for
/// the CLI debug link profile (see module docs); `target_override` lets a
/// caller (the LSP, tests) substitute a different triple, `None` = host.
pub fn eval_cfg(attr: &Attribute, debug_mode: bool, target_override: Option<&str>) -> bool {
    if !is_cfg(attr) {
        return true;
    }
    let triple = target_override.unwrap_or(HOST_TRIPLE);
    let os = host_os();
    let mut saw_true = false;
    let mut saw_any = false;
    for arg in &attr.args {
        saw_any = true;
        let v = eval_arg(arg, debug_mode, triple, os);
        saw_true = saw_true || v;
    }
    // Empty `@cfg()` is vacuously true (matches Rust's `cfg()` never used,
    // but keep the item rather than dropping it silently).
    if !saw_any {
        return true;
    }
    saw_true
}

/// `key = value` inside `@cfg(...)`: `Assign { lhs: Ident, value }` is how
/// the attribute-arg grammar delivers it (it accepts general expressions).
fn eval_named(key: &str, value: &Expr, debug_mode: bool, triple: &str, os: &str) -> bool {
    match key {
        "os" => match &value.kind {
            ExprKind::StringLit(s) => s.as_str() == os,
            _ => false,
        },
        "target" => match &value.kind {
            ExprKind::StringLit(s) => s.as_str() == triple,
            _ => false,
        },
        "debug" => match &value.kind {
            ExprKind::BoolLit(b) => debug_mode == *b,
            _ => false,
        },
        _ => false,
    }
}

fn eval_arg(arg: &AttributeArg, debug_mode: bool, triple: &str, os: &str) -> bool {
    match arg {
        AttributeArg::Expr(e) => {
            // Named form (`os = "macos"`) parses as `Assign` (general exprs).
            if let ExprKind::Assign { lhs, value } = &e.kind {
                if let ExprKind::Ident(key) = &lhs.kind {
                    return eval_named(key, value, debug_mode, triple, os);
                }
                return false;
            }
            match &e.kind {
                ExprKind::Ident(name) => {
                    // Bare identifier: truthy set of known predicates.
                    match name.as_str() {
                        "debug" => debug_mode,
                        "windows" => os == "windows",
                        // POSIX family: everything that is not Windows. An
                        // unrecognized host (`unknown`) counts as neither,
                        // so platform-specific code is never guessed.
                        "unix" => os != "windows" && os != "unknown",
                        _ => false,
                    }
                }
                _ => false,
            }
        }
        AttributeArg::Named(key, _, value) | AttributeArg::Assign(key, _, value) => {
            eval_named(key, value, debug_mode, triple, os)
        }
    }
}

/// Drop items whose `@cfg` evaluates false. Pure function on the item list;
/// errors are impossible (unknown conditions evaluate false), so callers
/// never surface a failure here.
pub fn apply_cfg(items: &mut Vec<crate::ast::Item>, debug_mode: bool) {
    items.retain(|item| {
        let crate::ast::Item::Attributed { attrs, .. } = item else {
            return true;
        };
        attrs.iter().all(|a| !is_cfg(a) || eval_cfg(a, debug_mode, None))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_attr(src: &str) -> Attribute {
        let full = format!("{}\nvoid f() do\nend\n", src);
        let lexed = crate::lexer::lex(&full);
        assert!(lexed.errors.is_empty(), "lex: {:?}", lexed.errors);
        let prog = crate::parse::parse(lexed.tokens, full).unwrap();
        match prog.items.into_iter().next().unwrap() {
            crate::ast::Item::Attributed { attrs, .. } => attrs.into_iter().next().unwrap(),
            other => panic!("expected attributed item, got {other:?}"),
        }
    }

    #[test]
    fn os_condition() {
        let a = cfg_attr("@cfg(os = \"macos\")");
        assert_eq!(eval_cfg(&a, true, None), host_os() == "macos");
        let a2 = cfg_attr("@cfg(os = \"linux\")");
        assert_eq!(eval_cfg(&a2, true, None), host_os() == "linux");
    }

    #[test]
    fn os_or() {
        let a = cfg_attr("@cfg(os = \"nope\" or os = \"alsono\")");
        assert!(!eval_cfg(&a, true, None));
        let b = cfg_attr("@cfg(os = \"macos\" or os = \"linux\")");
        assert_eq!(eval_cfg(&b, true, None), host_os() != "windows");
    }

    #[test]
    fn target_condition() {
        let a = cfg_attr("@cfg(target = \"aarch64-apple-darwin\")");
        assert_eq!(eval_cfg(&a, true, None), HOST_TRIPLE.contains("darwin"));
        let a2 = cfg_attr("@cfg(target = \"x86_64-unknown-linux-gnu\")");
        assert_eq!(eval_cfg(&a2, true, None), HOST_TRIPLE.contains("linux"));
    }

    #[test]
    fn debug_flag() {
        let a = cfg_attr("@cfg(debug)");
        assert!(eval_cfg(&a, true, None));
        assert!(!eval_cfg(&a, false, None));
        let b = cfg_attr("@cfg(debug = false)");
        assert!(!eval_cfg(&b, true, None));
        assert!(eval_cfg(&b, false, None));
    }

    #[test]
    fn unix_and_windows_predicates_are_exclusive() {
        let unix = cfg_attr("@cfg(unix)");
        let win = cfg_attr("@cfg(windows)");
        match host_os() {
            "windows" => {
                assert!(!eval_cfg(&unix, true, None));
                assert!(eval_cfg(&win, true, None));
            }
            "unknown" => {
                assert!(!eval_cfg(&unix, true, None));
                assert!(!eval_cfg(&win, true, None));
            }
            _ => {
                assert!(eval_cfg(&unix, true, None));
                assert!(!eval_cfg(&win, true, None));
            }
        }
    }

    #[test]
    fn unknown_condition_is_false() {
        let a = cfg_attr("@cfg(feature = \"simd\")");
        assert!(!eval_cfg(&a, true, None));
    }

    #[test]
    fn apply_drops_false_items() {
        let src = r#"
@cfg(os = "definitely_not_a_real_os")
void hidden() do
end

void shown() do
end
"#;
        let lexed = crate::lexer::lex(src);
        assert!(lexed.errors.is_empty());
        let mut prog = crate::parse::parse(lexed.tokens, src.to_string()).unwrap();
        apply_cfg(&mut prog.items, true);
        assert_eq!(prog.items.len(), 1);
        match &prog.items[0] {
            crate::ast::Item::Function(f) => assert_eq!(f.name, "shown"),
            other => panic!("expected function, got {other:?}"),
        }
    }
}
