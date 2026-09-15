//! Signature help (`textDocument/signatureHelp`).
//!
//! Pure-text call analysis (no parse required, so mid-typing `foo(a, |`
//! works) plus symbol lookup for parameter lists. The server builds the
//! most tolerant `Analysis` it can; this module handles the rest.

use lsp_types::{ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation};

use crate::analysis::Analysis;

/// An unclosed call frame enclosing the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    /// Callee name (`method` in `obj.method(`).
    pub name: String,
    /// Zero-based index of the argument being typed.
    pub active: u32,
}

/// Find the innermost unclosed `(` before `offset` and count its
/// top-level commas. Forward scan with string/comment awareness so
/// `"(",`, `// ,` and `/* ( */` never confuse depth. Returns `None`
/// outside any call.
pub fn enclosing_call(text: &str, offset: usize) -> Option<CallContext> {
    let offset = offset.min(text.len());
    #[derive(Debug)]
    struct Frame {
        name: String,
        commas: u32,
    }
    let bytes = text.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0usize;
    // Lexer states for the scan.
    let mut line_comment = false;
    let mut block_depth = 0u32;
    let mut in_dq = false; // "..." (incl. triple-quote bodies)
    let mut in_sq = false; // '...'
    let mut in_raw = false; // raw strings: backslash escapes nothing
    let mut escape = false;

    // Byte offset of the last `(` or `,` or `)` at call depth — used to
    // detect the empty-args case is unnecessary; commas counted directly.
    while i < offset {
        let b = bytes[i];
        if line_comment {
            if b == b'\n' {
                line_comment = false;
            }
            i += 1;
            continue;
        }
        if block_depth > 0 {
            if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                block_depth += 1;
                i += 2;
                continue;
            }
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block_depth -= 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_dq || in_sq {
            if escape {
                escape = false;
                i += 1;
                continue;
            }
            if !in_raw && b == b'\\' {
                escape = true;
                i += 1;
                continue;
            }
            if in_dq && b == b'"' {
                in_dq = false;
                i += 1;
                continue;
            }
            if in_sq && b == b'\'' {
                in_sq = false;
                i += 1;
                continue;
            }
            // `{...}` interpolation inside strings still runs code — but
            // parens inside string text must not count; interpolation
            // parens are vanishingly rare at the cursor, ignore them.
            i += 1;
            continue;
        }
        match b {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                block_depth = 1;
                i += 2;
            }
            b'"' => {
                in_dq = true;
                in_raw = false;
                i += 1;
            }
            b'\'' => {
                in_sq = true;
                in_raw = false;
                i += 1;
            }
            b'`' => {
                // Raw string Editors: backtick-quoted, no escapes.
                in_dq = true;
                in_raw = true;
                i += 1;
            }
            b'(' => {
                let name = ident_before(bytes, i);
                stack.push(Frame { name, commas: 0 });
                i += 1;
            }
            b',' => {
                if let Some(top) = stack.last_mut() {
                    top.commas += 1;
                }
                i += 1;
            }
            b')' => {
                stack.pop();
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    let frame = stack.pop()?;
    if frame.name.is_empty() {
        return None;
    }
    Some(CallContext { name: frame.name, active: frame.commas })
}

/// Identifier (or `obj.method` tail) immediately before byte `open`.
fn ident_before(bytes: &[u8], open: usize) -> String {
    let mut end = open;
    // Skip whitespace between the name and `(`.
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t' || bytes[end - 1] == b'\n') {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    // `obj.method(` → take `method` (after the last `.`).
    let mut name = &bytes[start..end];
    if let Some(dot) = name.iter().rposition(|&b| b == b'.') {
        name = &name[dot + 1..];
    }
    String::from_utf8_lossy(name).into_owned()
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Build `SignatureHelp` at `offset`: text-scan the call, look up the
/// callee, clamp the active parameter. `None` outside calls or for
/// unknown callees.
pub fn help(analysis: &Analysis, text: &str, offset: usize) -> Option<SignatureHelp> {
    let ctx = enclosing_call(text, offset)?;
    let sym = analysis.lookup_callable(&ctx.name)?;
    let params: Vec<ParameterInformation> = sym
        .params
        .iter()
        .map(|(n, t)| ParameterInformation {
            label: ParameterLabel::Simple(if t.is_empty() || t == "any" {
                n.clone()
            } else {
                format!("{n}: {t}")
            }),
            documentation: None,
        })
        .collect();
    let active = if params.is_empty() {
        0
    } else {
        ctx.active.min(params.len() as u32 - 1)
    };
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label: sym.detail.clone(),
            documentation: None,
            parameters: Some(params),
            active_parameter: Some(active),
        }],
        active_signature: Some(0),
        active_parameter: Some(active),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "int add(int a, int b) do\n    return a + b\nend\n\nvoid main() do\n    int s = add(1, 2)\nend\n";

    fn analysis_of(src: &str) -> Analysis {
        let out = hella_compiler::lexer::lex(src);
        let prog = hella_compiler::parse::parse(out.tokens, src.to_string()).unwrap();
        Analysis::from_program(&prog)
    }

    #[test]
    fn finds_call_and_first_param() {
        let off = SRC.find("add(1").unwrap() + 4; // right after `(`
        let ctx = enclosing_call(SRC, off).expect("call");
        assert_eq!(ctx.name, "add");
        assert_eq!(ctx.active, 0);
    }

    #[test]
    fn comma_advances_active_param() {
        let off = SRC.find("add(1,").unwrap() + 6; // after `,`
        let ctx = enclosing_call(SRC, off).expect("call");
        assert_eq!(ctx.name, "add");
        assert_eq!(ctx.active, 1);
    }

    #[test]
    fn active_clamps_to_last_param() {
        let a = analysis_of(SRC);
        let off = SRC.find("add(1, 2").unwrap() + 8;
        let h = help(&a, SRC, off).expect("help");
        assert_eq!(h.active_parameter, Some(1));
        assert_eq!(h.signatures.len(), 1);
        assert!(h.signatures[0].label.contains("add"));
    }

    #[test]
    fn mid_typing_unclosed_call_works() {
        // No closing paren / no `end` — pure text scan still finds it.
        let src = "int add(int a, int b) do\n    return a + b\nend\n\nvoid main() do\n    int s = add(1, ";
        let a = analysis_of("int add(int a, int b) do\n    return a + b\nend\n\nvoid main() do\nend\n");
        let h = help(&a, src, src.len()).expect("help");
        assert_eq!(h.active_parameter, Some(1));
    }

    #[test]
    fn method_name_after_dot() {
        let src = "struct P has\n    int x\nend\n\nvoid main() do\n    P p = has x = 1 end\n    int v = p.foo(1, ";
        let off = src.len();
        let ctx = enclosing_call(src, off).expect("call");
        assert_eq!(ctx.name, "foo");
        assert_eq!(ctx.active, 1);
    }

    #[test]
    fn parens_in_strings_and_comments_ignored() {
        let src = "void main() do\n    string s = \"(not, a call)\"\n    // foo(bar,\n    int v = add(1, ";
        let ctx = enclosing_call(src, src.len()).expect("call");
        assert_eq!(ctx.name, "add");
        assert_eq!(ctx.active, 1);
    }

    #[test]
    fn unknown_callee_returns_none() {
        let a = analysis_of(SRC);
        let src = "void main() do\n    int v = nope(1, ";
        assert_eq!(help(&a, src, src.len()), None);
    }

    #[test]
    fn outside_call_returns_none() {
        let a = analysis_of(SRC);
        assert_eq!(enclosing_call(SRC, 5), None);
        assert_eq!(help(&a, SRC, 5), None);
    }
}
