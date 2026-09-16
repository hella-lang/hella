//! Doc comments (`///`) for Hella.
//!
//! `///` lines directly above an item form its documentation. Ordinary
//! `//` comments (and `////...` lines) are never docs and also break a
//! doc block, so internal notes stay invisible to the LSP.
//!
//! The lexer skips all comments as trivia, so docs are recovered here by
//! scanning source lines — the same approach `hella-fmt` uses to preserve
//! comments without touching the grammar.

/// Extract the doc block immediately preceding the byte offset `item_start`.
///
/// Walks upwards over consecutive `///` own-lines. The block must end on the
/// line directly above the item's line (no blank line in between — a blank
/// line means the comment documents nothing). Returns the joined text with
/// the `///` markers stripped (one optional leading space removed per line),
/// or `None` when there is no doc block.
pub fn extract_doc_comment(source: &str, item_start: usize) -> Option<String> {
    let item_start = item_start.min(source.len());
    // Line index containing `item_start`.
    let item_line = source[..item_start].matches('\n').count();
    let lines: Vec<&str> = source.lines().collect();
    if item_line >= lines.len() {
        return None;
    }
    let mut docs: Vec<String> = Vec::new();
    let mut idx = item_line as isize - 1;
    let mut first = true;
    while idx >= 0 {
        let line = lines[idx as usize];
        let trimmed = line.trim_start();
        if first && trimmed.is_empty() {
            // Blank line between comment and item: not attached.
            return None;
        }
        first = false;
        if trimmed.is_empty() {
            break;
        }
        match doc_text_of_line(trimmed) {
            Some(text) => {
                docs.push(text);
                idx -= 1;
            }
            None => break,
        }
    }
    if docs.is_empty() {
        return None;
    }
    docs.reverse();
    let joined = docs.join("\n");
    if joined.trim().is_empty() {
        // A bare `///` block with no text documents nothing.
        return None;
    }
    Some(joined)
}

/// Doc text of a single trimmed line, or `None` when the line is not a doc
/// comment. `//` (ordinary) and `////...` (four or more slashes) are not docs.
fn doc_text_of_line(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("///")?;
    // `//// foo` is a normal comment, not a doc.
    if rest.starts_with('/') {
        return None;
    }
    // Strip one optional leading space: `/// foo` -> `foo`.
    let text = rest.strip_prefix(' ').unwrap_or(rest);
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_above_item() {
        let src = "/// Prints hello.\nvoid print(string s) do\nend\n";
        let off = src.find("void print").unwrap();
        assert_eq!(
            extract_doc_comment(src, off).as_deref(),
            Some("Prints hello.")
        );
    }

    #[test]
    fn normal_comment_not_doc() {
        let src = "// Prints hello.\nvoid print(string s) do\nend\n";
        let off = src.find("void print").unwrap();
        assert_eq!(extract_doc_comment(src, off), None);
    }

    #[test]
    fn four_slashes_not_doc() {
        let src = "//// Prints hello.\nvoid print(string s) do\nend\n";
        let off = src.find("void print").unwrap();
        assert_eq!(extract_doc_comment(src, off), None);
    }

    #[test]
    fn multiline_doc_joins() {
        let src = "/// Line one.\n/// Line two.\nvoid f() do\nend\n";
        let off = src.find("void f").unwrap();
        assert_eq!(
            extract_doc_comment(src, off).as_deref(),
            Some("Line one.\nLine two.")
        );
    }

    #[test]
    fn blank_line_breaks_attachment() {
        let src = "/// Orphaned.\n\nvoid f() do\nend\n";
        let off = src.find("void f").unwrap();
        assert_eq!(extract_doc_comment(src, off), None);
    }

    #[test]
    fn normal_comment_breaks_block() {
        // Only the `///` lines directly above the item attach; the `//`
        // line cuts off the upper doc line.
        let src = "/// Upper.\n// note\n/// Lower.\nvoid f() do\nend\n";
        let off = src.find("void f").unwrap();
        assert_eq!(
            extract_doc_comment(src, off).as_deref(),
            Some("Lower.")
        );
    }

    #[test]
    fn indented_doc() {
        let src = "struct S has\n  /// The x field.\n  int x\nend\n";
        let off = src.find("int x").unwrap();
        assert_eq!(
            extract_doc_comment(src, off).as_deref(),
            Some("The x field.")
        );
    }
}
