//! Document formatting via `hella-fmt`.
//!
//! Whole-file formatting is canonical: `hella_fmt::format_source` on the
//! current buffer text. Broken sources return `None` (client keeps its
//! text) instead of an error — formatting must never crash the server.
//!
//! Range formatting is best-effort: `hella-fmt` is whole-file (import
//! blank-line rules, top-level separation), so we format the whole buffer
//! and narrow the edit to the requested line span when line counts agree,
//! falling back to a full-file edit otherwise.

use lsp_types::{Position, Range, TextEdit};

use crate::document::{offset_to_position, position_to_offset};

/// Format the whole document. Returns `None` when already canonical or
/// when the source does not parse (leave the buffer untouched).
pub fn format_document(text: &str) -> Option<Vec<TextEdit>> {
    let formatted = hella_fmt::format_source(text).ok()?;
    if formatted == normalize(text) {
        return None;
    }
    Some(vec![TextEdit {
        range: full_range(text),
        new_text: formatted,
    }])
}

/// Format the requested range (best-effort, see module docs). Returns
/// `None` when nothing would change or the source does not parse.
pub fn format_range(text: &str, range: &Range) -> Option<Vec<TextEdit>> {
    let formatted = hella_fmt::format_source(text).ok()?;
    if formatted == normalize(text) {
        return None;
    }
    let orig_lines: Vec<&str> = text.lines().collect();
    let fmt_lines: Vec<&str> = formatted.lines().collect();
    // Fast path: same line count — replace exactly the requested lines.
    if orig_lines.len() == fmt_lines.len() {
        let start = (range.start.line as usize).min(orig_lines.len().saturating_sub(1));
        let end = (range.end.line as usize).min(orig_lines.len().saturating_sub(1));
        let (start, end) = (start.min(end), start.max(end));
        let wanted: Vec<&str> = fmt_lines[start..=end].to_vec();
        let current: Vec<&str> = orig_lines[start..=end].to_vec();
        if wanted == current {
            return None;
        }
        return Some(vec![TextEdit {
            range: Range {
                start: Position { line: start as u32, character: 0 },
                end: Position {
                    line: end as u32,
                    character: current.last().map(|l| l.len() as u32).unwrap_or(0),
                },
            },
            new_text: wanted.join("\n"),
        }]);
    }
    // Line counts shifted (block expansion, blank-line rules): fall back
    // to a full-file edit so the result stays canonical.
    Some(vec![TextEdit {
        range: full_range(text),
        new_text: formatted,
    }])
}

fn full_range(text: &str) -> Range {
    Range {
        start: Position { line: 0, character: 0 },
        end: offset_to_position(text, text.len()),
    }
}

/// `hella-fmt` output ends with exactly one `\n`; compare against the
/// normalized buffer so idempotent files report `None`.
fn normalize(text: &str) -> String {
    let mut out = text.replace("\r\n", "\n");
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[allow(dead_code)]
fn _assert_offset_helpers_used() {
    let _ = position_to_offset("", &Position { line: 0, character: 0 });
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANON: &str = "void main() do\n    int x = 1\nend\n";

    #[test]
    fn canonical_source_returns_none() {
        assert_eq!(format_document(CANON), None);
    }

    #[test]
    fn unformatted_source_returns_full_edit() {
        let edits = format_document("void main() do\nint x=1\nend\n").expect("edit");
        assert_eq!(edits.len(), 1);
        assert!(edits[0].new_text.contains("    int x = 1"));
    }

    #[test]
    fn broken_source_returns_none() {
        assert_eq!(format_document("void main( do\n"), None);
        assert_eq!(
            format_range(
                "void main( do\n",
                &Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 5 },
                }
            ),
            None
        );
    }

    #[test]
    fn range_narrows_to_requested_lines() {
        let src = "void main() do\nint x=1\n    int y = 2\nend\n";
        let edits = format_range(
            src,
            &Range {
                start: Position { line: 1, character: 0 },
                end: Position { line: 1, character: 7 },
            },
        )
        .expect("edit");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range.start.line, 1);
        assert_eq!(edits[0].range.end.line, 1);
        assert!(edits[0].new_text.contains("int x = 1"));
    }

    #[test]
    fn range_already_canonical_returns_none() {
        let edits = format_range(
            CANON,
            &Range {
                start: Position { line: 1, character: 0 },
                end: Position { line: 1, character: 5 },
            },
        );
        assert_eq!(edits, None);
    }
}
