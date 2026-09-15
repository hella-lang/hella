//! Document manager: tracks open `.hll` files and provides byte-offset ↔
//! LSP-position conversion helpers.
//!
//! LSP positions are 0-based (`line`, `character`), and columns are counted
//! in UTF-16 code units. Spans in the `compiler` crate are byte offsets.

use std::collections::HashMap;

use hella_compiler::token::Span;
use lsp_types::{Position, Range, TextDocumentContentChangeEvent, Uri};

/// One open document (the parsed representation lives in `analysis`).
#[derive(Debug, Clone)]
pub struct Document {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
}

/// In-memory store of open documents keyed by URI string.
#[derive(Debug, Default)]
pub struct DocumentManager {
    docs: HashMap<String, Document>,
}

impl DocumentManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self, uri: Uri, version: i32, text: String) {
        self.docs
            .insert(uri.as_str().to_string(), Document { uri, version, text });
    }

    /// Apply a `didChange` payload. Handles both full-document and
    /// incremental (range-based) change events.
    pub fn change(
        &mut self,
        uri: &Uri,
        version: i32,
        changes: &[TextDocumentContentChangeEvent],
    ) {
        let Some(doc) = self.docs.get_mut(uri.as_str()) else {
            return;
        };
        for change in changes {
            match &change.range {
                Some(range) => {
                    let start = position_to_offset(&doc.text, &range.start);
                    let end = position_to_offset(&doc.text, &range.end);
                    let mut next = String::with_capacity(doc.text.len() + change.text.len());
                    next.push_str(&doc.text[..start]);
                    next.push_str(&change.text);
                    next.push_str(&doc.text[end..]);
                    doc.text = next;
                }
                None => {
                    // Whole-document replace.
                    doc.text = change.text.clone();
                }
            }
        }
        doc.version = version;
    }

    pub fn close(&mut self, uri: &Uri) {
        self.docs.remove(uri.as_str());
    }

    pub fn get(&self, uri: &Uri) -> Option<&Document> {
        self.docs.get(uri.as_str())
    }

    /// URIs of all open documents (for `workspace/symbol`).
    pub fn uris(&self) -> Vec<Uri> {
        self.docs.values().map(|d| d.uri.clone()).collect()
    }
}

// ── Position helpers ─────────────────────────────────────────────────

/// Convert a byte offset into a 0-based LSP `Position` (UTF-16 columns).
pub fn offset_to_position(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let mut line = 0usize;
    let mut line_start = 0usize; // byte index of the current line's start
    // Walk lines; find the one containing `offset`.
    for (i, b) in text.bytes().enumerate() {
        if i >= offset {
            break;
        }
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    // Count UTF-16 code units from line_start to offset within the line.
    let fragment = &text[line_start..offset];
    let character = fragment.encode_utf16().count();
    Position {
        line: line as u32,
        character: character as u32,
    }
}

/// Convert a 0-based LSP `Position` back into a byte offset. If the
/// position is past the end of the document, clamps to `text.len()`.
pub fn position_to_offset(text: &str, pos: &Position) -> usize {
    let mut line = 0u32;
    let mut byte: usize = 0;
    for (i, b) in text.bytes().enumerate() {
        if line == pos.line {
            break;
        }
        if b == b'\n' {
            line += 1;
            byte = i + 1;
        }
    }
    // Within the target line, advance `pos.character` UTF-16 code units.
    let rest = &text[byte..];
    let mut code_units = 0u32;
    for (i, c) in rest.char_indices() {
        if code_units >= pos.character {
            return byte + i;
        }
        code_units += c.len_utf16() as u32;
    }
    text.len()
}

/// Convert a compiler byte-span into an LSP `Range`.
pub fn span_to_range(text: &str, span: Span) -> Range {
    Range {
        start: offset_to_position(text, span.start),
        end: offset_to_position(text, span.end),
    }
}

/// Best-effort `file://` URI → filesystem path (percent-decoded) for
/// project-model lookups (import resolution, the `main`-file gate).
/// Returns `None` for non-file schemes (untitled buffers, etc.).
pub fn uri_to_path(uri: &Uri) -> Option<std::path::PathBuf> {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(rest);
    #[cfg(windows)]
    let decoded = decoded.strip_prefix('/').unwrap_or(&decoded).to_string();
    Some(std::path::PathBuf::from(decoded))
}

/// Minimal `%XX` decoding for file URIs (no extra dependency for this).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(h), Some(l)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2)))
            {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: Option<&u8>) -> Option<u8> {
    match b.copied()? {
        v @ b'0'..=b'9' => Some(v - b'0'),
        v @ b'a'..=b'f' => Some(v - b'a' + 10),
        v @ b'A'..=b'F' => Some(v - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_and_position_round_trip() {
        let text = "int x = 1\n  int y = 2\n";
        for offset in [0usize, 3, 10, 11, 20, text.len() - 1] {
            let pos = offset_to_position(text, offset);
            let back = position_to_offset(text, &pos);
            assert_eq!(back, offset, "mismatch for offset {offset}");
        }
    }

    #[test]
    fn two_byte_char_column_width() {
        // "ä" is 2 bytes in UTF-8 but 1 code unit in UTF-16; "𝄞" is 4 bytes / 2 units.
        // Byte layout: a=0, ä=1..3, b=3, 𝄞=4..8, c=8.
        let text = "aäb𝄞c";
        let pos_of_b = offset_to_position(text, 3); // start of 'b'
        assert_eq!(pos_of_b.character, 2); // a + ä
        let pos_of_c = offset_to_position(text, 8); // start of 'c'
        assert_eq!(pos_of_c.character, 5); // a + ä + b + 𝄞 (2 units)
        // Round-trips through position_to_offset.
        assert_eq!(position_to_offset(text, &pos_of_c), 8);
        assert_eq!(position_to_offset(text, &Position { line: 0, character: 3 }), 4);
    }

    #[test]
    fn span_to_range_basic() {
        let text = "line1\nline2\n";
        let r = span_to_range(text, Span::new(6, 11)); // begins line2
        assert_eq!(r.start.line, 1);
        assert_eq!(r.start.character, 0);
        assert_eq!(r.end.character, 5);
    }
}