//! Preserve file byte offsets while parsing decoded string interpolations.
use super::*;

impl Parser {
    /// Decode one escape at a time using the language's existing decoder. The
    /// map includes every decoded byte boundary and the final end boundary.
    /// Composing through source_offset also handles nested interpolations.
    pub(super) fn decoded_with_positions(
        &self,
        inner: &str,
        local_start: usize,
    ) -> (String, Vec<usize>) {
        let mut decoded = String::new();
        let mut positions = Vec::new();
        let mut chars = inner.char_indices().peekable();
        while let Some((start, c)) = chars.next() {
            if c == '\\' {
                if let Some((_, escape)) = chars.next() {
                    let extra = match escape {
                        'x' => 2,
                        'u' => 4,
                        _ => 0,
                    };
                    for _ in 0..extra {
                        chars.next();
                    }
                }
            }
            let end = chars.peek().map_or(inner.len(), |(offset, _)| *offset);
            let chunk = Self::unescape_string(&inner[start..end]);
            for byte in 0..chunk.len() {
                let original = if c == '\\' { start } else { start + byte };
                positions.push(self.source_offset(local_start + original));
            }
            decoded.push_str(&chunk);
        }
        positions.push(self.source_offset(local_start + inner.len()));
        (decoded, positions)
    }

    pub(super) fn interpolation_parser(
        source: String,
        positions: Vec<usize>,
    ) -> Self {
        let mut lexed = crate::lexer::lex(&source);
        for token in &mut lexed.tokens {
            token.span = Span::new(
                positions[token.span.start],
                positions[token.span.end],
            );
        }
        Self {
            tokens: lexed.tokens,
            pos: 0,
            source,
            source_positions: Some(positions),
        }
    }
}
