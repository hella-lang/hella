//! Quickfixes (`textDocument/codeAction`, kind `quickfix`).
//!
//! Each fix is anchored on a diagnostic the server already emits, so
//! actions never fire on clean code:
//!
//! - `cannot resolve import …` → remove that import line.
//! - `unused \`new\` value is a guaranteed leak …` → assign to a fresh
//!   `own` variable (`new Cat()` → `own Cat cat = new Cat()`).
//! - `expected \`end\` to close …` → insert the missing `end`.

use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, Position, Range, TextEdit, Uri,
    WorkspaceEdit,
};

use crate::document::{offset_to_position, position_to_offset};

const LEAK_MSG: &str = "unused `new` value is a guaranteed leak";

/// Quickfixes for diagnostics overlapping `range` in `text`.
pub fn code_actions(
    uri: &Uri,
    text: &str,
    range: &Range,
    diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    code_actions_with_imports(uri, text, range, diagnostics, None, &[])
}

/// Same, but with workspace context for auto-import quickfixes.
pub fn code_actions_with_imports(
    uri: &Uri,
    text: &str,
    range: &Range,
    diagnostics: &[Diagnostic],
    importer: Option<&std::path::Path>,
    workspace_roots: &[std::path::PathBuf],
) -> Vec<CodeActionOrCommand> {
    let mut out = Vec::new();
    for diag in diagnostics {
        if !overlaps(&diag.range, range) {
            continue;
        }
        if let Some(a) = remove_import(uri, text, diag) {
            out.push(CodeActionOrCommand::CodeAction(a));
        } else if let Some(a) = assign_own(uri, text, diag) {
            out.push(CodeActionOrCommand::CodeAction(a));
        } else if let Some(a) = insert_end(uri, text, diag) {
            out.push(CodeActionOrCommand::CodeAction(a));
        } else if let Some(a) = auto_import(uri, text, diag, importer, workspace_roots) {
            out.push(CodeActionOrCommand::CodeAction(a));
        }
    }
    out
}

fn overlaps(a: &Range, b: &Range) -> bool {
    !(a.end.line < b.start.line
        || b.end.line < a.start.line
        || (a.end.line == b.start.line && a.end.character < b.start.character)
        || (b.end.line == a.start.line && b.end.character < a.start.character))
}

fn quickfix(
    uri: &Uri,
    title: String,
    diag: &Diagnostic,
    edit_range: Range,
    new_text: String,
) -> CodeAction {
    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit { range: edit_range, new_text }],
    );
    CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diag.clone()]),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        command: None,
        is_preferred: Some(true),
        disabled: None,
        data: None,
    }
}

/// `cannot resolve import \`foo::bar\`` → delete the `import …` line.
fn remove_import(uri: &Uri, text: &str, diag: &Diagnostic) -> Option<CodeAction> {
    let path = diag.message.strip_prefix("cannot resolve import `")?;
    let path = path.split('`').next()?;
    // Base path without selective suffix (`a::{x}` imports `a`).
    let base = path.split("::{").next().unwrap_or(path);
    // Import statements use `::` separators; source lines match verbatim.
    let line_idx = text.lines().position(|l| {
        let t = l.trim();
        t == format!("import {base}") || t.starts_with(&format!("import {base}::")) || t == format!("import {path}")
    })?;
    Some(quickfix(
        uri,
        format!("Remove import `{path}`"),
        diag,
        Range {
            start: Position { line: line_idx as u32, character: 0 },
            end: Position { line: line_idx as u32 + 1, character: 0 },
        },
        String::new(),
    ))
}

/// Bare-`new` leak → bind it: `new Cat()` → `own Cat cat = new Cat()`.
fn assign_own(uri: &Uri, text: &str, diag: &Diagnostic) -> Option<CodeAction> {
    if !diag.message.contains(LEAK_MSG) {
        return None;
    }
    let start = position_to_offset(text, &diag.range.start);
    let end = position_to_offset(text, &diag.range.end).min(text.len());
    let slice = text.get(start..end)?;
    let ty = slice
        .strip_prefix("new")?
        .trim_start()
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
        .next()?;
    let ty = ty.split("::").last().unwrap_or(ty);
    if ty.is_empty() {
        return None;
    }
    let mut var = ty.to_string();
    var[..1].make_ascii_lowercase();
    Some(quickfix(
        uri,
        format!("Assign to new `own` variable `{var}`"),
        diag,
        Range { start: offset_to_position(text, start), end: offset_to_position(text, start) },
        format!("own {ty} {var} = "),
    ))
}

/// `expected \`end\` to close …` → insert `end` at the error point.
fn insert_end(uri: &Uri, text: &str, diag: &Diagnostic) -> Option<CodeAction> {
    if !diag.message.starts_with("expected `end` to close") {
        return None;
    }
    Some(quickfix(
        uri,
        "Insert missing `end`".to_string(),
        diag,
        Range { start: diag.range.start, end: diag.range.start },
        "end\n".to_string(),
    ))
}

/// `undefined …` / `unknown type …` → suggest the import that provides it.
fn auto_import(
    uri: &Uri,
    text: &str,
    diag: &Diagnostic,
    importer: Option<&std::path::Path>,
    workspace_roots: &[std::path::PathBuf],
) -> Option<CodeAction> {
    let name = extract_missing_symbol(&diag.message)?;
    if text.contains(&format!("import {name}")) || text.contains(&format!("::{name}")) {
        // Already imported (selective or qualified) — no fix.
        return None;
    }
    let import = crate::auto_import::find_import_for_symbol(&name, importer, workspace_roots)?;
    if text.lines().any(|l| l.trim() == format!("import {import}") || l.trim().starts_with(&format!("import {import}::")) || l.trim().starts_with(&format!("import {import} "))) {
        return None;
    }
    let insert_at = import_insertion_line(text);
    Some(quickfix(
        uri,
        format!("Add import `{import}` for `{name}`"),
        diag,
        Range {
            start: Position { line: insert_at, character: 0 },
            end: Position { line: insert_at, character: 0 },
        },
        format!("import {import}\n"),
    ))
}

fn extract_missing_symbol(msg: &str) -> Option<String> {
    for prefix in ["unknown type `", "undefined function `", "undefined variable `"] {
        if let Some(rest) = msg.strip_prefix(prefix) {
            let name = rest.split('`').next()?;
            if !name.is_empty() && crate::analysis::Analysis::valid_ident(name) {
                return Some(name.to_string());
            }
        }
    }
    // Fallback: some sema messages use "unknown type `T`" inside longer text.
    None
}

fn import_insertion_line(text: &str) -> u32 {
    let mut last_import: Option<usize> = None;
    for (idx, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("import ") {
            last_import = Some(idx);
        }
    }
    last_import.map(|i| i as u32 + 1).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::span_to_range;
    use hella_compiler::token::Span;

    fn uri() -> Uri {
        "file:///main.hll".parse().unwrap()
    }

    fn diag(text: &str, start: usize, end: usize, message: &str) -> Diagnostic {
        Diagnostic {
            range: span_to_range(text, Span::new(start, end)),
            severity: None,
            code: None,
            code_description: None,
            source: Some("hella-lsp".to_string()),
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn whole(text: &str) -> Range {
        Range {
            start: Position { line: 0, character: 0 },
            end: offset_to_position(text, text.len()),
        }
    }

    #[test]
    fn removes_unresolvable_import() {
        let text = "import nope::missing\n\nvoid main() do\nend\n";
        let d = diag(text, 0, 7, "cannot resolve import `nope::missing` (looked in /tmp)");
        let actions = code_actions(&uri(), text, &whole(text), &[d]);
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(a) = &actions[0] else {
            panic!("expected action");
        };
        assert_eq!(a.title, "Remove import `nope::missing`");
        let edits = &a.edit.as_ref().unwrap().changes.as_ref().unwrap()[&uri()];
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "");
        assert_eq!((edits[0].range.start.line, edits[0].range.end.line), (0, 1));
    }

    #[test]
    fn assigns_bare_new_to_own() {
        let text = "open class Cat has\nend\nvoid main() do\n  new Cat()\nend\n";
        let start = text.find("new Cat()").unwrap();
        let d = diag(
            text,
            start,
            start + 9,
            "unused `new` value is a guaranteed leak — assign it to an `own` slot or delete it",
        );
        let actions = code_actions(&uri(), text, &whole(text), &[d]);
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(a) = &actions[0] else {
            panic!("expected action");
        };
        assert!(a.title.contains("cat"), "title names the var: {}", a.title);
        let edits = &a.edit.as_ref().unwrap().changes.as_ref().unwrap()[&uri()];
        assert_eq!(edits[0].new_text, "own Cat cat = ");
    }

    #[test]
    fn inserts_missing_end() {
        let text = "void main() do\n  int x = 1\n";
        let d = diag(text, text.len(), text.len(), "expected `end` to close block");
        let actions = code_actions(&uri(), text, &whole(text), &[d]);
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(a) = &actions[0] else {
            panic!("expected action");
        };
        assert_eq!(a.title, "Insert missing `end`");
    }

    #[test]
    fn auto_import_for_unknown_type() {
        let root = std::env::temp_dir().join(format!("hella-actions-auto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("mymod.hll"), "int helper(int x) do\n  return x\nend\n").unwrap();
        let text = "void main() do\n  int y = helper(1)\nend\n";
        let d = diag(text, text.find("helper").unwrap(), text.find("helper").unwrap() + 6, "undefined function `helper`");
        let actions = code_actions_with_imports(&uri(), text, &whole(text), &[d], Some(&root.join("main.hll")), &[root.clone()]);
        assert!(actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(c) => c.title.contains("mymod") && c.title.contains("helper"),
            _ => false
        }), "expected auto-import: {actions:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_file_no_actions() {
        let text = "void main() do\n  int x = 1\nend\n";
        assert!(code_actions(&uri(), text, &whole(text), &[]).is_empty());
        // Diagnostics outside the requested range are ignored.
        let d = diag(text, 0, 4, "expected `end` to close block");
        let far = Range {
            start: Position { line: 2, character: 0 },
            end: Position { line: 2, character: 1 },
        };
        assert!(code_actions(&uri(), text, &far, &[d]).is_empty());
    }
}
