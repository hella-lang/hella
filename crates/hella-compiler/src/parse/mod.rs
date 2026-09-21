//! Phase 1 recursive-descent + Pratt parser.
//! Covers EBNF §6-8 §13-21 subset for int/bool/void, functions, blocks, if/while/return/var/expr.

use crate::ast::*;
use crate::token::{Span, SpannedToken, Token};

mod interpolation;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at {}..{}",
            self.message, self.span.start, self.span.end
        )
    }
}

pub struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    source: String,
    /// For decoded interpolation fragments, maps local byte boundaries back to
    /// the original file. Token/AST spans always stay in original-file offsets.
    source_positions: Option<Vec<usize>>,
}

impl Parser {
    pub fn new(tokens: Vec<SpannedToken>, source: String) -> Self {
        Self {
            tokens,
            pos: 0,
            source,
            source_positions: None,
        }
    }

    fn peek(&self) -> Option<&SpannedToken> {
        self.tokens.get(self.pos)
    }
    fn peek_token(&self) -> Option<&Token> {
        self.peek().map(|st| &st.token)
    }
    fn peek_span(&self) -> Span {
        self.peek()
            .map(|st| st.span)
            .unwrap_or(Span::new(self.source_end(), self.source_end()))
    }
    fn is_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn advance(&mut self) -> Option<SpannedToken> {
        if self.is_eof() {
            return None;
        }
        let t = self.tokens[self.pos].clone();
        self.pos += 1;
        Some(t)
    }

    fn expect(
        &mut self,
        expected: Token,
        msg: &str,
    ) -> Result<SpannedToken, ParseError> {
        match self.peek() {
            Some(st) if st.token == expected => Ok(self.advance().unwrap()),
            Some(st) => Err(ParseError {
                message: format!(
                    "{msg}: expected `{expected}`, found `{}`",
                    st.token
                ),
                span: st.span,
            }),
            None => Err(ParseError {
                message: format!("{msg}: expected `{expected}`, found EOF"),
                span: Span::new(self.source_end(), self.source_end()),
            }),
        }
    }

    fn consume_if(&mut self, tok: Token) -> bool {
        if self.peek_token() == Some(&tok) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn consume_newlines(&mut self) -> usize {
        let mut n = 0;
        while matches!(
            self.peek_token(),
            Some(Token::Newline) | Some(Token::Semicolon)
        ) {
            self.advance();
            n += 1;
        }
        n
    }

    fn expect_terminator(&mut self, ctx: &str) -> Result<(), ParseError> {
        // Terminator is newline or ;  — allow multiple, but require at least one unless next is End/else/eof
        let n = self.consume_newlines();
        if n > 0 {
            return Ok(());
        }
        // If next is End or Else or EOF, allow missing terminator (implicit before block end)
        if matches!(
            self.peek_token(),
            Some(Token::End) | Some(Token::Else) | None
        ) {
            return Ok(());
        }
        Err(ParseError {
            message: format!("{ctx}: expected newline or `;`"),
            span: self.peek_span(),
        })
    }

    fn source_offset(&self, offset: usize) -> usize {
        self.source_positions.as_ref().map_or(offset, |positions| positions[offset])
    }

    fn local_offset(&self, offset: usize) -> usize {
        self.source_positions.as_ref().map_or(offset, |positions| {
            positions.partition_point(|&position| position < offset)
        })
    }

    fn source_end(&self) -> usize {
        self.source_offset(self.source.len())
    }

    fn slice(&self, span: Span) -> &str {
        &self.source[self.local_offset(span.start)..self.local_offset(span.end)]
    }

    fn parse_int_lit(&self, span: Span) -> Result<i64, ParseError> {
        let s = self.slice(span).replace('_', "");
        if s.starts_with("0x") || s.starts_with("0X") {
            i64::from_str_radix(&s[2..], 16).map_err(|e| ParseError {
                message: format!("invalid hex int: {e}"),
                span,
            })
        } else if s.starts_with("0b") || s.starts_with("0B") {
            i64::from_str_radix(&s[2..], 2).map_err(|e| ParseError {
                message: format!("invalid bin int: {e}"),
                span,
            })
        } else {
            s.parse::<i64>().map_err(|e| ParseError {
                message: format!("invalid int: {e}"),
                span,
            })
        }
    }

    fn unescape_string(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('b') => out.push('\x08'),
                Some('f') => out.push('\x0c'),
                Some('0') => out.push('\0'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('x') => {
                    let h1 = chars.next().unwrap_or('0');
                    let h2 = chars.next().unwrap_or('0');
                    let hex = format!("{}{}", h1, h2);
                    if let Ok(v) = u8::from_str_radix(&hex, 16) {
                        out.push(v as char);
                    } else {
                        out.push_str(&format!("\\x{}", hex));
                    }
                }
                Some('u') => {
                    let mut hex = String::new();
                    for _ in 0..4 {
                        if let Some(h) = chars.next() {
                            hex.push(h);
                        }
                    }
                    if let Ok(v) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(v) {
                            out.push(ch);
                        } else {
                            out.push_str(&format!("\\u{}", hex));
                        }
                    } else {
                        out.push_str(&format!("\\u{}", hex));
                    }
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        }
        out
    }
    fn unescape_char(s: &str) -> char {
        if s.len() == 1 {
            return s.chars().next().unwrap();
        }
        if s.starts_with('\\') {
            let un = Self::unescape_string(s);
            un.chars().next().unwrap_or('\0')
        } else {
            s.chars().next().unwrap_or('\0')
        }
    }

    fn peek_decl_kind(&self) -> Option<Token> {
        let mut p = self.pos;
        // skip optional visibility/open/sealed for class/struct
        while p < self.tokens.len() {
            match self.tokens[p].token {
                Token::Public | Token::Private | Token::Open | Token::Sealed => p += 1,
                _ => break,
            }
        }
        self.tokens.get(p).map(|t| t.token.clone())
    }

    // ── Entry ─────────────────────────────────────────────────────────
    pub fn parse_program(&mut self) -> Result<Program, ParseError> {
        let start = 0;
        self.consume_newlines();
        let mut items = Vec::new();
        while !self.is_eof() {
            self.consume_newlines();
            if self.is_eof() {
                break;
            }
            // Top-level: import / struct/class/enum/trait decl vs function decl (EBNF §32 §35)
            if self.peek_token() == Some(&Token::Import) {
                let decl = self.parse_import()?;
                items.push(Item::Import(decl));
            } else if self.peek_decl_kind() == Some(Token::Struct) {
                let decl = self.parse_struct_decl()?;
                items.push(Item::Struct(decl));
            } else if self.peek_decl_kind() == Some(Token::Class) {
                let decl = self.parse_class_decl()?;
                items.push(Item::Class(decl));
            } else if self.peek_decl_kind() == Some(Token::Enum) {
                let decl = self.parse_enum_decl()?;
                items.push(Item::Enum(decl));
            } else if self.peek_decl_kind() == Some(Token::Trait) {
                let decl = self.parse_trait_decl()?;
                items.push(Item::Trait(decl));
            } else if self.peek_token() == Some(&Token::At) {
                let attrs = self.parse_attributes()?;
                // After attributes, parse the actual decl
                let inner = if self.peek_decl_kind() == Some(Token::Struct) {
                    Item::Struct(self.parse_struct_decl()?)
                } else if self.peek_decl_kind() == Some(Token::Class) {
                    Item::Class(self.parse_class_decl()?)
                } else if self.peek_decl_kind() == Some(Token::Enum) {
                    Item::Enum(self.parse_enum_decl()?)
                } else if self.peek_decl_kind() == Some(Token::Trait) {
                    Item::Trait(self.parse_trait_decl()?)
                } else if self.peek_token() == Some(&Token::Typedef) {
                    Item::Typedef(self.parse_typedef_decl()?)
                } else if self.peek_token() == Some(&Token::Distinct) {
                    Item::Distinct(self.parse_distinct_decl()?)
                } else if self.peek_token() == Some(&Token::Extend) {
                    Item::Extension(self.parse_extension_decl()?)
                } else if self.peek_token() == Some(&Token::Extern) {
                    Item::Extern(self.parse_extern_decl()?)
                } else if self.peek_token() == Some(&Token::Const)
                    || (matches!(self.peek_token(), Some(Token::Public) | Some(Token::Private))
                        && self.tokens.get(self.pos + 1).map(|t| t.token == Token::Const).unwrap_or(false))
                {
                    Item::Const(self.parse_const_decl()?)
                } else if self.is_var_decl_start() {
                    Item::Var(self.parse_var_decl()?)
                } else {
                    Item::Function(self.parse_function()?)
                };
                items.push(Item::Attributed{attrs, item: Box::new(inner)});
            } else if self.peek_token() == Some(&Token::Typedef) {
                items.push(Item::Typedef(self.parse_typedef_decl()?));
            } else if self.peek_token() == Some(&Token::Distinct) {
                items.push(Item::Distinct(self.parse_distinct_decl()?));
            } else if self.peek_token() == Some(&Token::Extend) {
                items.push(Item::Extension(self.parse_extension_decl()?));
            } else if self.peek_token() == Some(&Token::Extern) {
                items.push(Item::Extern(self.parse_extern_decl()?));
            } else if self.peek_token() == Some(&Token::Init) {
                // init block
                let start = self.expect(Token::Init, "expected `init`")?.span.start;
                let blk = self.parse_block()?;
                let span = Span::new(start, blk.span.end);
                items.push(Item::Init(blk));
            } else if self.peek_token() == Some(&Token::Const)
                || (matches!(self.peek_token(), Some(Token::Public) | Some(Token::Private))
                    && self.tokens.get(self.pos + 1).map(|t| t.token == Token::Const).unwrap_or(false))
            {
                let decl = self.parse_const_decl()?;
                items.push(Item::Const(decl));
            } else if self.peek_token() == Some(&Token::At) {
                // Handle attributes at top-level (already handled above, but for safety)
                let attrs = self.parse_attributes()?;
                items.push(Item::Attributed{attrs, item: Box::new(Item::Function(self.parse_function()?))});
            } else if self.is_var_decl_start() {
                let decl = self.parse_var_decl()?;
                items.push(Item::Var(decl));
            } else {
                let func = self.parse_function()?;
                items.push(Item::Function(func));
            }
            self.consume_newlines();
        }
        Ok(Program {
            items,
            span: Span::new(start, self.source_end()),
        })
    }

    fn parse_struct_decl(&mut self) -> Result<StructDecl, ParseError> {
        // optional visibility
        if matches!(self.peek_token(), Some(Token::Public) | Some(Token::Private)) { self.advance(); }
        let start = self.expect(Token::Struct, "expected `struct`")?.span.start;
        let (name, name_span) = self.parse_ident()?;
        let generics = self.parse_generic_params_opt();
        let where_clause = self.parse_where_clause_opt();
        self.expect(Token::Has, "expected `has` after struct name")?;
        self.consume_newlines();
        let mut fields = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            if matches!(
                self.peek_token(),
                Some(Token::Newline) | Some(Token::Semicolon)
            ) {
                self.advance();
                continue;
            }
            let vis = self.parse_visibility();
            let ty = self.parse_type()?;
            let (fname, fspan) = self.parse_ident()?;
            let fend = fspan.end;
            let default = if self.consume_if(Token::Eq) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect_terminator("struct field")?;
            let span = Span::new(ty.span().start, fend);
            fields.push(StructField {
                ty,
                name: fname,
                name_span: fspan,
                visibility: vis,
                default,
                span,
            });
            self.consume_newlines();
        }
        let end = self
            .expect(Token::End, "expected `end` to close struct")?
            .span
            .end;
        Ok(StructDecl {
            name,
            name_span,
            fields,
            generic_params: generics,
            where_clause,
            span: Span::new(start, end),
        })
    }

    fn is_type_start(&self) -> bool {
        matches!(
            self.peek_token(),
            Some(Token::Int)
                | Some(Token::Bool)
                | Some(Token::Void)
                | Some(Token::StringKw)
                | Some(Token::CharKw)
                | Some(Token::Float)
                | Some(Token::Double)
                | Some(Token::Any)
                | Some(Token::Function)
                | Some(Token::Ident)
                | Some(Token::LParen)
        )
    }

    fn parse_const_decl(&mut self) -> Result<ConstDecl, ParseError> {
        let vis = self.parse_visibility();
        let start = self.expect(Token::Const, "expected `const`")?.span.start;
        // optional type: try to detect `const Type Ident =` vs `const Ident =`
        let mut ty: Option<Type> = None;
        let mut name = String::new();
        let mut name_span = Span::new(start, start);
        // Lookahead for typed form
        // Save pos, try parse type, then check if next is Ident and after that is `=`
        let save = self.pos;
        let mut is_typed = false;
        if self.is_type_start() {
            if let Ok(t) = self.parse_type() {
                if self.peek_token() == Some(&Token::Ident) {
                    // Peek further to see if after Ident we have `=`
                    if self.tokens.get(self.pos + 1).map(|st| st.token == Token::Eq).unwrap_or(false) {
                        ty = Some(t);
                        let (n, ns) = self.parse_ident()?;
                        name = n;
                        name_span = ns;
                        is_typed = true;
                    }
                }
                if !is_typed {
                    // rollback: the parsed type was actually the ident (untyped case)
                    self.pos = save;
                }
            } else {
                self.pos = save;
            }
        }
        if !is_typed {
            let (n, ns) = self.parse_ident()?;
            name = n;
            name_span = ns;
        }
        self.expect(Token::Eq, "expected `=` after const name")?;
        let init = self.parse_expr()?;
        self.expect_terminator("const declaration")?;
        let span = Span::new(start, init.span.end);
        Ok(ConstDecl { visibility: vis, ty, name, name_span, init, span })
    }

    fn is_destructure_start(&self) -> bool {
        // Check for `a, b` or `a, _` then `=` pattern
        // Must start with Ident, then `,` then Ident or `_`, then eventually `=`
        if self.tokens.get(self.pos).map(|t| t.token == Token::Ident).unwrap_or(false) {
            let mut p = self.pos + 1;
            // need at least one `,`
            if p >= self.tokens.len() || self.tokens[p].token != Token::Comma {
                return false;
            }
            // after `,`, must have Ident or `_` (where `_` is Ident with slice "_")
            // Check for `_` as Ident "_" ?
            // In Hella, `_` is wildcard, lexed as Ident with text "_" ?
            // We'll treat "_" as Ident "_" as well, but need to check
            p += 1;
            if p >= self.tokens.len() { return false; }
            let is_ident_or_underscore = self.tokens[p].token == Token::Ident || {
                // Check if slice is "_"
                // For now, "_" is lexed as Ident with text "_", so token is Ident
                false
            };
            if !is_ident_or_underscore && self.tokens[p].token != Token::Ident {
                // Check for "_" as wildcard: it may be lexed as Ident "_" as well
                // We'll just check token is Ident and slice "_"
                return false;
            }
            // Now scan ahead for `=` after possible `, ident/_` repeats, skipping `,` and Ident/`_`
            // We need to find `=` after the destructuring target
            // Destructuring target is `a, b, _, c` etc, then `=`
            // So we can simulate: starting at pos, we have `a , b , _ , c =`
            // We need to ensure after the comma-separated list, next is `=`
            // For simplicity, check if after the first `, Ident` we eventually hit `=` before `;` or newline
            let mut q = p + 1;
            while q < self.tokens.len() {
                match self.tokens[q].token {
                    Token::Comma => {
                        q += 1;
                        if q < self.tokens.len() && self.tokens[q].token == Token::Ident {
                            // Could be Ident or "_" (both Ident)
                            q += 1;
                            continue;
                        } else {
                            return false;
                        }
                    }
                    Token::Eq => return true,
                    Token::Newline | Token::Semicolon => return false,
                    _ => return false,
                }
            }
            return false;
        }
        false
    }

    fn parse_destructure(&mut self) -> Result<DestructureStmt, ParseError> {
        let start = self.peek_span().start;
        let mut targets = Vec::new();
        // first must be Ident
        let (first, fspan) = self.parse_ident()?;
        targets.push(DestructureTarget::Ident(first, fspan));
        while self.consume_if(Token::Comma) {
            // After comma, expect Ident or `_` (wildcard)
            // `_` is lexed as Ident with text "_"
            if self.peek_token() == Some(&Token::Ident) {
                let tok = self.peek().unwrap().clone();
                let slice = self.slice(tok.span);
                if slice == "_" {
                    self.advance();
                    targets.push(DestructureTarget::Wildcard(tok.span));
                } else {
                    let (n, ns) = self.parse_ident()?;
                    targets.push(DestructureTarget::Ident(n, ns));
                }
            } else {
                return Err(ParseError { message: "expected identifier or `_` after `,` in destructuring".into(), span: self.peek_span() });
            }
        }
        self.expect(Token::Eq, "expected `=` after destructuring target")?;
        let expr = self.parse_expr()?;
        self.expect_terminator("destructuring")?;
        let span = Span::new(start, expr.span.end);
        Ok(DestructureStmt { targets, expr, span })
    }

    fn parse_assert(&mut self) -> Result<AssertStmt, ParseError> {
        let is_debug = self.peek_token() == Some(&Token::DebugAssert);
        let start = if is_debug {
            self.expect(Token::DebugAssert, "expected `debug_assert`")?.span.start
        } else {
            self.expect(Token::Assert, "expected `assert`")?.span.start
        };
        let cond = self.parse_expr()?;
        let message = if self.consume_if(Token::Comma) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect_terminator("assert")?;
        let end = message.as_ref().map(|m| m.span.end).unwrap_or(cond.span.end);
        let span = Span::new(start, end);
        Ok(AssertStmt { is_debug, cond, message, span })
    }

    fn parse_visibility(&mut self) -> Visibility {
        match self.peek_token() {
            Some(Token::Public) => { self.advance(); Visibility::Public },
            Some(Token::Private) => { self.advance(); Visibility::Private },
            _ => Visibility::Default,
        }
    }

    fn parse_class_decl(&mut self) -> Result<ClassDecl, ParseError> {
        // EBNF: [visibility] [open] class ident [generics] [extends type] [implements trait-list] has {class-member} end
        let mut is_open = false;
        let mut is_sealed = false;
        let start_vis = self.peek_span().start;
        let mut class_vis = self.parse_visibility();
        if self.peek_token() == Some(&Token::Open) { is_open = true; self.advance(); }
        if self.peek_token() == Some(&Token::Sealed) { is_sealed = true; self.advance(); }
        // also allow visibility after open? e.g. open public class - handle again
        if class_vis == Visibility::Default { class_vis = self.parse_visibility(); }
        let class_start = self.expect(Token::Class, "expected `class`")?.span.start;
        let start = if start_vis < class_start { start_vis } else { class_start };
        let (name, name_span) = self.parse_ident()?;
        let generics = self.parse_generic_params_opt();
        let where_clause = self.parse_where_clause_opt();
        let mut extends = None;
        if self.peek_token() == Some(&Token::Extends) {
            self.advance();
            extends = Some(self.parse_type()?);
        }
        let mut implements = Vec::new();
        if self.peek_token() == Some(&Token::Implements) {
            self.advance();
            loop {
                implements.push(self.parse_type()?);
                if !self.consume_if(Token::Comma) { break; }
            }
        }
        self.expect(Token::Has, "expected `has` after class name")?;
        self.consume_newlines();
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut constructors = Vec::new();
        let mut destructors = Vec::new();
        let mut properties = Vec::new();
        let mut operators: Vec<OperatorDecl> = Vec::new();
        let mut conversions: Vec<ConversionDecl> = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
            // member modifiers: [visibility] [static] [sealed] [override] — `open` only on class, not members
            let vis = self.parse_visibility();
            let mut is_static = false;
            let mut m_sealed = false;
            let mut is_override = false;
            let m_open = false;
            // loop for flags
            loop {
                match self.peek_token() {
                    Some(Token::Static) => { is_static = true; self.advance(); },
                    Some(Token::Sealed) => { m_sealed = true; self.advance(); },
                    Some(Token::Override) => { is_override = true; self.advance(); },
                    _ => break,
                }
            }
            if self.peek_token() == Some(&Token::Open) {
                return Err(ParseError{message: "method cannot be `open` (only class can be open)".into(), span: self.peek_span()});
            }
            // Check for destructor: ~ ident ( ) block
            if self.peek_token() == Some(&Token::Tilde) {
                let dstart = self.advance().unwrap().span.start;
                let (dname, dspan) = self.parse_ident().map_err(|_| ParseError{message: "expected destructor name after `~`".into(), span: self.peek_span()})?;
                self.expect(Token::LParen, "expected `(` for destructor")?;
                self.expect(Token::RParen, "expected `)` for destructor")?;
                let body = self.parse_block()?;
                let span = Span::new(dstart, body.span.end);
                destructors.push(DestructorDecl{name: dname, name_span: dspan, body, visibility: vis, span});
                self.consume_newlines();
                continue;
            }
            // Check for constructor: ident ( params ) initialize [block]
            let is_constructor = {
                let save = self.pos;
                let res = if self.peek_token() == Some(&Token::Ident) {
                    let _ = self.parse_ident();
                    if self.peek_token() == Some(&Token::LParen) {
                        self.advance();
                        let mut depth = 1;
                        while !self.is_eof() && depth>0 {
                            match self.peek_token() {
                                Some(Token::LParen) => { depth+=1; self.advance(); },
                                Some(Token::RParen) => { depth-=1; self.advance(); },
                                _ => { self.advance(); },
                            }
                        }
                        self.peek_token() == Some(&Token::Initialize)
                    } else { false }
                } else { false };
                self.pos = save;
                res
            };
            if is_constructor {
                let (cname, cspan) = self.parse_ident()?;
                self.expect(Token::LParen, "expected `(` for constructor params")?;
                let mut params = Vec::new();
                if self.peek_token() != Some(&Token::RParen) {
                    loop {
                        let param = self.parse_param()?;
                        params.push(param);
                        if self.consume_if(Token::Comma) { continue; } else { break; }
                    }
                }
                self.expect(Token::RParen, "expected `)` after constructor params")?;
                let istart = self.expect(Token::Initialize, "expected `initialize` for constructor")?.span.start;
                let body = if self.peek_token() == Some(&Token::Do) { Some(self.parse_block()?) } else {
                    self.expect_terminator("constructor")?;
                    None
                };
                let span = Span::new(cspan.start, body.as_ref().map(|b| b.span.end).unwrap_or(istart+10));
                constructors.push(ConstructorDecl{name: cname, name_span: cspan, params, body, visibility: vis, span});
                self.consume_newlines();
                continue;
            }
            // Check for property: [type] ident (get|set) ...
            // Property has forms: [vis] type ident get block [set (param) block]  OR  [vis] type ident set (param) block [get block]  OR simplified [vis] ident set ...
            // Detect: after type+ident, next is Get/Set; or after ident next is Set
            let is_property = {
                let save = self.pos;
                let mut is_prop = false;
                // try type+ident+get/set
                if let Ok(_) = self.parse_type() {
                    if self.peek_token() == Some(&Token::Ident) {
                        let _ = self.parse_ident();
                        if matches!(self.peek_token(), Some(Token::Get) | Some(Token::Set)) { is_prop = true; }
                    }
                }
                if !is_prop {
                    self.pos = save;
                    if self.peek_token() == Some(&Token::Ident) {
                        let _ = self.parse_ident();
                        if self.peek_token() == Some(&Token::Set) { is_prop = true; }
                    }
                }
                self.pos = save;
                is_prop
            };
            if is_property {
                // Parse property: getters/setters are public by default (override class default private)
                let save = self.pos;
                let mut ty_opt = None;
                let mut pname = String::new();
                let mut pspan = Span::new(0,0);
                let mut prop_vis = if vis == Visibility::Default { Visibility::Public } else { vis };
                // try type+ident
                let mut parsed_with_type = false;
                if let Ok(t) = self.parse_type() {
                    if let Ok((n,s)) = self.parse_ident() {
                        if matches!(self.peek_token(), Some(Token::Get) | Some(Token::Set)) {
                            ty_opt = Some(t);
                            pname = n; pspan = s; parsed_with_type = true;
                        }
                    }
                }
                if !parsed_with_type {
                    self.pos = save;
                    let (n,s) = self.parse_ident()?;
                    pname = n; pspan = s;
                    ty_opt = None;
                }
                // now parse getter/setter blocks
                let mut getter = None;
                let mut setter = None;
                // EBNF allows getter then optional setter OR setter then optional getter
                // Parse first accessor
                if self.peek_token() == Some(&Token::Get) {
                    self.advance();
                    getter = Some(self.parse_block()?);
                    self.consume_newlines();
                    if self.peek_token() == Some(&Token::Set) {
                        self.advance();
                        self.expect(Token::LParen, "expected `(` for property setter")?;
                        let pty = self.parse_type()?;
                        let (pn, pn_span) = self.parse_ident()?;
                        let pspan = Span::new(pty.span().start, pn_span.end);
                        let param = Param { is_variadic: false, mode: ParamMode::None, ty: pty, name: pn, name_span: pn_span, default: None, span: pspan};
                        self.expect(Token::RParen, "expected `)` after setter param")?;
                        let body = self.parse_block()?;
                        setter = Some((param, body));
                    }
                } else if self.peek_token() == Some(&Token::Set) {
                    self.advance();
                    self.expect(Token::LParen, "expected `(` for property setter")?;
                    let pty = self.parse_type()?;
                    let (pn, pn_span) = self.parse_ident()?;
                    let pspan = Span::new(pty.span().start, pn_span.end);
                    let param = Param { is_variadic: false, mode: ParamMode::None, ty: pty, name: pn, name_span: pn_span, default: None, span: pspan};
                    self.expect(Token::RParen, "expected `)` after setter param")?;
                    let body = self.parse_block()?;
                    setter = Some((param, body));
                    self.consume_newlines();
                    if self.peek_token() == Some(&Token::Get) {
                        self.advance();
                        getter = Some(self.parse_block()?);
                    }
                } else {
                    return Err(ParseError{message: "expected `get` or `set` for property".into(), span: self.peek_span()});
                }
                let end = setter.as_ref().map(|(_,b)| b.span.end).or(getter.as_ref().map(|b| b.span.end)).unwrap_or(pspan.end);
                properties.push(PropertyDecl{ty: ty_opt, name: pname, name_span: pspan, visibility: prop_vis, getter, setter, span: Span::new(pspan.start, end)});
                self.consume_newlines();
                continue;
            }
            // Check for operator: [vis] [static] operator <symbol> (params) [where] block
            if self.peek_token() == Some(&Token::OperatorKw) {
                let op_start = self.advance().unwrap().span.start;
                // operator symbol
                let op_tok = self.advance().ok_or(ParseError{message: "expected operator symbol".into(), span: self.peek_span()})?;
                let mut op_str = match op_tok.token {
                    Token::Plus => "+".to_string(),
                    Token::Minus => "-".to_string(),
                    Token::Star => "*".to_string(),
                    Token::Slash => "/".to_string(),
                    Token::Percent => "%".to_string(),
                    Token::Lt => "<".to_string(),
                    Token::LtEq => "<=".to_string(),
                    Token::Gt => ">".to_string(),
                    Token::GtEq => ">=".to_string(),
                    Token::Is => {
                        if self.peek_token() == Some(&Token::Not) { self.advance(); "is not".to_string() } else { "is".to_string() }
                    },
                    Token::Ampersand => "&".to_string(),
                    Token::Pipe => "|".to_string(),
                    Token::Caret => "^".to_string(),
                    Token::Tilde => "~".to_string(),
                    Token::LShift => "<<".to_string(),
                    Token::RShift => ">>".to_string(),
                    Token::Eq => "=".to_string(),
                    Token::PlusAssign => "+=".to_string(),
                    Token::MinusAssign => "-=".to_string(),
                    Token::StarAssign => "*=".to_string(),
                    Token::SlashAssign => "/=".to_string(),
                    Token::PercentAssign => "%=".to_string(),
                    Token::AndAssign => "&=".to_string(),
                    Token::OrAssign => "|=".to_string(),
                    Token::XorAssign => "^=".to_string(),
                    Token::LShiftAssign => "<<=".to_string(),
                    Token::RShiftAssign => ">>=".to_string(),
                    Token::PlusPlus => "++".to_string(),
                    Token::MinusMinus => "--".to_string(),
                    Token::LBracket => {
                        // expect ]
                        self.expect(Token::RBracket, "expected `]` for operator `[]`")?;
                        "[]".to_string()
                    },
                    _ => self.slice(op_tok.span).to_string(),
                };
                let op_span = op_tok.span;
                self.expect(Token::LParen, "expected `(` after operator")?;
                let mut params = Vec::new();
                if self.peek_token() != Some(&Token::RParen) {
                    loop {
                        let param = self.parse_param()?;
                        params.push(param);
                        if !self.consume_if(Token::Comma) { break; }
                    }
                }
                self.expect(Token::RParen, "expected `)` after operator params")?;
                let where_clause = if self.peek_token()==Some(&Token::Where) { self.parse_where_clause_opt() } else { None };
                let body = self.parse_block()?;
                let span = Span::new(op_start, body.span.end);
                // need operators vec in scope - will push after loop init? For now create temporary and will handle via mutable outer
                // Use a workaround: store in a temporary vector via closure capture? Instead we need to have operators mutable
                // We have fields, methods, etc. but operators not yet declared in this scope, so we need to declare it above
                // For now, push to a separate vec that we will merge later - we will use a hack: store in methods as operator? Better to handle directly via mutable variable captured from outer scope
                // We will handle by pushing to operators vector that we will declare before loop
                // This code will be replaced via python to correctly reference operators
                operators.push(OperatorDecl{visibility: vis, is_static, op: op_str, op_span, params, body, where_clause, span});
                continue;
            }
            // Check for convert: [vis] [explicit] convert Type to Type block
            if self.peek_token() == Some(&Token::Convert) {
                let cstart = self.advance().unwrap().span.start;
                let from_ty = self.parse_type()?;
                self.expect(Token::To, "expected `to` after convert source type")?;
                let to_ty = self.parse_type()?;
                let body = self.parse_block()?;
                let span = Span::new(cstart, body.span.end);
                conversions.push(ConversionDecl{visibility: vis, is_explicit: false, from_ty, to_ty, body, span});
                continue;
            }
            if vis != crate::ast::Visibility::Default && self.peek_token() == Some(&Token::Explicit) {
                let is_explicit = true;
                self.advance();
                if self.peek_token() == Some(&Token::Convert) {
                    let cstart = self.advance().unwrap().span.start;
                    let from_ty = self.parse_type()?;
                    self.expect(Token::To, "expected `to` after convert source type")?;
                    let to_ty = self.parse_type()?;
                    let body = self.parse_block()?;
                    let span = Span::new(cstart, body.span.end);
                    conversions.push(ConversionDecl{visibility: vis, is_explicit, from_ty, to_ty, body, span});
                    continue;
                }
            }
            // Check for method vs field: lookahead type ident '(' => method
            let is_func = {
                let save = self.pos;
                let ty_ok = self.parse_type().is_ok();
                let after_ty = self.peek_token().cloned();
                let is_ident = after_ty == Some(Token::Ident);
                let mut is_func2 = false;
                if is_ident {
                    if let Some(tok) = self.tokens.get(self.pos+1) {
                        if tok.token == Token::LParen { is_func2 = true; }
                    }
                }
                self.pos = save;
                ty_ok && is_func2
            };
            if is_func {
                // function/method with modifiers
                let ret_ty = self.parse_type()?;
                let (mname, mspan) = self.parse_ident()?;
                let _method_generics = self.parse_generic_params_opt();
                self.expect(Token::LParen, "expected `(` for method params")?;
                let mut params = Vec::new();
                if self.peek_token() != Some(&Token::RParen) {
                    loop {
                        let param = self.parse_param()?;
                        params.push(param);
                        if self.consume_if(Token::Comma) { continue; } else { break; }
                    }
                }
                self.expect(Token::RParen, "expected `)` after params")?;
                let body = self.parse_block()?;
                let span = Span::new(ret_ty.span().start, body.span.end);
                methods.push(Function{ret_ty, name: mname, name_span: mspan, params, body, visibility: vis, is_static, is_sealed: m_sealed, is_override, is_open: m_open, is_async: false, async_span: None, generic_params: Vec::new(), where_clause: None, span});
            } else {
                // field
                // handle const field? EBNF class-member includes constant-declaration: const [type] ident = expr terminator
                if self.peek_token() == Some(&Token::Const) {
                    self.advance();
                    // optional type
                    let _ty_opt = if matches!(self.peek_token(), Some(Token::Int)|Some(Token::Bool)|Some(Token::StringKw)|Some(Token::CharKw)|Some(Token::Ident)|Some(Token::Void)) {
                        let save = self.pos;
                        if self.parse_type().is_ok() {
                            if self.peek_token()==Some(&Token::Ident) { Some(()) } else { self.pos = save; None }
                        } else { None }
                    } else { None };
                    if _ty_opt.is_some() { let _ = self.parse_type()?; } // actually need to capture but ignore const type
                    let (fname, fspan) = self.parse_ident()?;
                    self.expect(Token::Eq, "expected `=` for const")?;
                    let _ = self.parse_expr()?;
                    self.expect_terminator("const declaration")?;
                    // const treated as field with default visibility but skip adding? For now treat as field without ty
                    // Use void type placeholder? Instead skip
                    // We'll add as field with int type placeholder to keep struct layout? Simpler ignore.
                    continue;
                }
                let ty = self.parse_type()?;
                let (fname, fspan) = self.parse_ident()?;
                let fend = fspan.end;
                let default = if self.consume_if(Token::Eq) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                self.expect_terminator("class field")?;
                let span = Span::new(ty.span().start, fend);
                fields.push(StructField{ty, name: fname, name_span: fspan, visibility: vis, default, span});
            }
            self.consume_newlines();
        }
        let end = self.expect(Token::End, "expected `end` to close class")?.span.end;
        Ok(ClassDecl{name, name_span, fields, methods, is_open, is_sealed, generic_params: generics, where_clause, extends, implements, constructors, destructors, properties, operators, conversions, span: Span::new(start, end)})
    }

    fn parse_trait_decl(&mut self) -> Result<TraitDecl, ParseError> {
        if matches!(self.peek_token(), Some(Token::Public) | Some(Token::Private)) { self.advance(); }
        let start = self.expect(Token::Trait, "expected `trait`")?.span.start;
        let (name, name_span) = self.parse_ident()?;
        let generics = self.parse_generic_params_opt();
        let where_clause = self.parse_where_clause_opt();
        self.expect(Token::Has, "expected `has` after trait name")?;
        self.consume_newlines();
        let mut methods = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
            let mut is_sealed = false;
            if self.peek_token() == Some(&Token::Sealed) { is_sealed = true; self.advance(); }
            let ret_ty = self.parse_type()?;
            let (mname, mspan) = self.parse_ident()?;
            let generics = self.parse_generic_params_opt();
            self.expect(Token::LParen, "expected `(` for trait method")?;
            let mut params = Vec::new();
            if self.peek_token() != Some(&Token::RParen) {
                loop {
                    let param = self.parse_param()?;
                    params.push(param);
                    if self.consume_if(Token::Comma) { continue; } else { break; }
                }
            }
            self.expect(Token::RParen, "expected `)` after trait params")?;
            let where_clause = self.parse_where_clause_opt();
            self.expect_terminator("trait method")?;
            let span = Span::new(ret_ty.span().start, mspan.end);
            methods.push(TraitMethod{ret_ty, name: mname, name_span: mspan, params, is_sealed, generic_params: generics, where_clause, span});
            self.consume_newlines();
        }
        let end = self.expect(Token::End, "expected `end` to close trait")?.span.end;
        Ok(TraitDecl{name, name_span, generic_params: generics, where_clause, methods, span: Span::new(start, end)})
    }

    fn parse_attributes(&mut self) -> Result<Vec<Attribute>, ParseError> {
        let mut attrs = Vec::new();
        while self.peek_token() == Some(&Token::At) {
            let start = self.advance().unwrap().span.start;
            let (name, name_span) = self.parse_ident()?;
            let is_cfg = name == "cfg";
            let mut args = Vec::new();
            if self.consume_if(Token::LParen) {
                if self.peek_token() != Some(&Token::RParen) {
                    loop {
                        // Try named arg: ident : expr
                        let save = self.pos;
                        let is_named = if self.peek_token() == Some(&Token::Ident) {
                            let (n, s) = self.parse_ident().unwrap();
                            if self.peek_token() == Some(&Token::Colon) {
                                self.advance(); // :
                                let e = self.parse_expr().unwrap();
                                args.push(AttributeArg::Named(n, s, e));
                                true
                            } else {
                                self.pos = save;
                                false
                            }
                        } else { false };
                        if !is_named {
                            // `name = value` (cfg-style: `os = "macos"`).
                            // The value parses at unary level so `=` never
                            // cascades into an assignment expression and
                            // `or` stays a *separator* between conditions
                            // rather than an operand operator.
                            let save_eq = self.pos;
                            let named_eq = if self.peek_token() == Some(&Token::Ident) {
                                let (n, s) = self.parse_ident().unwrap();
                                if self.consume_if(Token::Eq) {
                                    match self.parse_unary() {
                                        Ok(v) => {
                                            args.push(AttributeArg::Assign(n, s, v));
                                            true
                                        }
                                        Err(e) => return Err(e),
                                    }
                                } else {
                                    self.pos = save_eq;
                                    false
                                }
                            } else { false };
                            if !named_eq {
                                let e = self.parse_expr()?;
                                args.push(AttributeArg::Expr(e));
                            }
                        }
                        // `,` separates attribute arguments; in `@cfg` a
                        // bare `or` separates altative conditions with the
                        // same meaning (any true condition keeps the item).
                        if self.consume_if(Token::Comma) { continue; }
                        if is_cfg && self.consume_if(Token::Or) { continue; }
                        break;
                    }
                }
                self.expect(Token::RParen, "expected `)` after attribute args")?;
            }
            let end = self.peek_span().end;
            attrs.push(Attribute{name, name_span, args, span: Span::new(start, end)});
            self.consume_newlines();
        }
        Ok(attrs)
    }

    fn parse_typedef_decl(&mut self) -> Result<TypedefDecl, ParseError> {
        let vis = self.parse_visibility();
        let start = self.expect(Token::Typedef, "expected `typedef`")?.span.start;
        let generics = self.parse_generic_params_opt();
        let (name, name_span) = self.parse_ident()?;
        self.expect(Token::Eq, "expected `=` after typedef name")?;
        let ty = self.parse_type()?;
        let ty_span = ty.span();
        self.expect_terminator("typedef")?;
        Ok(TypedefDecl{name, name_span, ty, visibility: vis, generic_params: generics, span: Span::new(start, ty_span.end)})
    }

    fn parse_distinct_decl(&mut self) -> Result<DistinctDecl, ParseError> {
        let vis = self.parse_visibility();
        let start = self.expect(Token::Distinct, "expected `distinct`")?.span.start;
        let (name, name_span) = self.parse_ident()?;
        let generics = self.parse_generic_params_opt();
        self.expect(Token::Eq, "expected `=` after distinct name")?;
        let ty = self.parse_type()?;
        let ty_span = ty.span();
        self.expect_terminator("distinct")?;
        Ok(DistinctDecl{name, name_span, ty, visibility: vis, generic_params: generics, span: Span::new(start, ty_span.end)})
    }

    fn parse_extern_decl(&mut self) -> Result<ExternDecl, ParseError> {
        let vis = self.parse_visibility();
        let start = self.expect(Token::Extern, "expected `extern`")?.span.start;
        let lib_tok = self.expect(Token::StringLit, "expected string literal for extern lib")?;
        let lib = self.slice(lib_tok.span)[1..self.slice(lib_tok.span).len()-1].to_string();
        let lib_span = lib_tok.span;
        self.expect(Token::From, "expected `from` after extern lib")?;
        let file_tok = self.expect(Token::StringLit, "expected string literal for extern file")?;
        let file = self.slice(file_tok.span)[1..self.slice(file_tok.span).len()-1].to_string();
        let file_span = file_tok.span;
        self.expect(Token::Do, "expected `do` after extern header")?;
        self.consume_newlines();
        let mut members = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
            // Extern members are always public: an explicit `public` is
            // accepted and ignored, `private` is rejected. (No visibility is
            // stored — nothing downstream gates on extern visibility.)
            let mvis = self.parse_visibility();
            if mvis == crate::ast::Visibility::Private {
                return Err(ParseError{message: "extern members are always public; remove `private`".into(), span: self.peek_span()});
            }
            // Check for extern-struct / extern-enum / extern-const before extern-function
            if self.peek_token() == Some(&Token::Struct) {
                let start = self.advance().unwrap().span.start;
                let (name, name_span) = self.parse_ident()?;
                let mut fields = Vec::new();
                if self.peek_token() == Some(&Token::Has) {
                    self.advance(); // has
                    self.consume_newlines();
                    while !self.is_eof() && self.peek_token() != Some(&Token::End) {
                        if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
                        let fty = self.parse_type()?;
                        let fty_span = fty.span();
                        let (fname, fspan) = self.parse_ident()?;
                        self.expect_terminator("extern field")?;
                        fields.push(ExternField{ty: fty, name: fname, name_span: fspan, span: Span::new(fty_span.start, fspan.end)});
                        self.consume_newlines();
                    }
                    self.expect(Token::End, "expected `end` to close extern struct")?;
                }
                let span = Span::new(start, name_span.end);
                members.push(ExternMember::Struct{name, name_span, fields, span});
                self.consume_newlines();
                continue;
            } else if self.peek_token() == Some(&Token::Enum) {
                let start = self.advance().unwrap().span.start;
                let (name, name_span) = self.parse_ident()?;
                self.expect(Token::Has, "expected `has` after extern enum name")?;
                self.consume_newlines();
                let mut variants = Vec::new();
                while !self.is_eof() && self.peek_token() != Some(&Token::End) {
                    if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
                    let (vname, vspan) = self.parse_ident()?;
                    let mut discriminant = None;
                    let mut payload_params = Vec::new();
                    if self.consume_if(Token::Eq) {
                        let expr = self.parse_expr()?;
                        discriminant = Some(expr);
                    }
                    if self.peek_token() == Some(&Token::LParen) {
                        self.advance(); // (
                        if self.peek_token() != Some(&Token::RParen) {
                            loop {
                                let pty = self.parse_type()?;
                                let (pname, pspan) = if self.peek_token() == Some(&Token::Ident) {
                                    let (n, ns) = self.parse_ident()?;
                                    (n, ns)
                                } else {
                                    (format!("_payload{}", payload_params.len()), pty.span())
                                };
                                let pspan2 = Span::new(pty.span().start, pspan.end);
                                payload_params.push(Param{is_variadic: false, mode: ParamMode::None, ty: pty, name: pname, name_span: pspan, default: None, span: pspan2});
                                if !self.consume_if(Token::Comma) { break; }
                                if self.peek_token() == Some(&Token::RParen) { break; }
                            }
                        }
                        self.expect(Token::RParen, "expected `)` after extern enum payload")?;
                    }
                    self.expect_terminator("extern enum variant")?;
                    variants.push(EnumVariant{name: vname, name_span: vspan, discriminant, payload_params, span: Span::new(vspan.start, vspan.end)});
                    self.consume_newlines();
                }
                let end = self.expect(Token::End, "expected `end` to close extern enum")?.span.end;
                members.push(ExternMember::Enum{name, name_span, variants, span: Span::new(start, end)});
                self.consume_newlines();
                continue;
            } else if self.peek_token() == Some(&Token::Const) {
                let start = self.advance().unwrap().span.start;
                let ty = self.parse_type()?;
                let (name, name_span) = self.parse_ident()?;
                self.expect_terminator("extern const")?;
                members.push(ExternMember::Const{ty, name, name_span, span: Span::new(start, name_span.end)});
                self.consume_newlines();
                continue;
            }
            let ty = self.parse_type()?;
            let (name, name_span) = self.parse_ident()?;
            if self.peek_token() == Some(&Token::LParen) {
                // extern function
                self.advance(); // (
                let mut params = Vec::new();
                if self.peek_token() != Some(&Token::RParen) {
                    loop {
                        if self.peek_token() == Some(&Token::DotDotDot) {
                            let dot_span = self.advance().unwrap().span;
                            // Check if `...` is alone (C varargs) or `...type ident` or `... ident` (derived)
                            if self.peek_token() == Some(&Token::RParen) {
                                // `...` alone like `printf(string fmt, ...)`
                                params.push(ExternParam { is_variadic: true, mode: ParamMode::None, ty: Type::Any(dot_span), name: "".to_string(), name_span: dot_span, span: dot_span });
                            } else if self.peek_token() == Some(&Token::Ident) {
                                // Could be `... vda` (derived) or `...int vda` (type is Ident `int`? but `int` is token Int, not Ident)
                                // For `... vda` derived, next is Ident and following is `,` or `)` or `=`
                                let next_is_delim = self.tokens.get(self.pos + 1).map(|t| matches!(t.token, Token::Comma | Token::RParen | Token::Eq)).unwrap_or(false);
                                if next_is_delim {
                                    let (pn, pn_span) = self.parse_ident()?;
                                    let span = Span::new(dot_span.start, pn_span.end);
                                    params.push(ExternParam { is_variadic: true, mode: ParamMode::None, ty: Type::Named("__derived__".to_string(), pn_span), name: pn, name_span: pn_span, span });
                                } else {
                                    // `...type ident` where type is next
                                    let pty = self.parse_type()?;
                                    let (pn, pn_span) = self.parse_ident()?;
                                    let span = Span::new(dot_span.start, pn_span.end);
                                    params.push(ExternParam { is_variadic: true, mode: ParamMode::None, ty: pty, name: pn, name_span: pn_span, span });
                                }
                            } else {
                                // `...` with type like `...int vda` where `int` is not Ident but token Int
                                let pty = self.parse_type()?;
                                let (pn, pn_span) = self.parse_ident()?;
                                let span = Span::new(dot_span.start, pn_span.end);
                                params.push(ExternParam { is_variadic: true, mode: ParamMode::None, ty: pty, name: pn, name_span: pn_span, span });
                            }
                        } else {
                            // Optional parameter mode: `ref T name` / `out T ident`
                            // (C out-parameters: `rand_s(ref int out)`).
                            let mode = if self.peek_token() == Some(&Token::Ref) {
                                self.advance();
                                ParamMode::Ref
                            } else if self.peek_token() == Some(&Token::Out) {
                                self.advance();
                                ParamMode::Out
                            } else {
                                ParamMode::None
                            };
                            let pty = self.parse_type()?;
                            let pty_span = pty.span();
                            let (pn, pn_span) = self.parse_ident()?;
                            params.push(ExternParam { is_variadic: false, mode, ty: pty, name: pn, name_span: pn_span, span: Span::new(pty_span.start, pn_span.end)});
                        }
                        if !self.consume_if(Token::Comma) { break; }
                        if self.peek_token() == Some(&Token::RParen) { break; }
                    }
                }
                self.expect(Token::RParen, "expected `)` after extern params")?;
                self.expect_terminator("extern function")?;
                members.push(ExternMember::Function{ty, name, name_span, params, span: Span::new(lib_span.start, name_span.end)});
            } else {
                // Unsupported extern member, skip
                self.expect_terminator("extern member")?;
            }
        }
        let end = self.expect(Token::End, "expected `end` to close extern")?.span.end;
        Ok(ExternDecl{lib, lib_span, file, file_span, members, visibility: vis, span: Span::new(start, end)})
    }

    fn parse_extension_decl(&mut self) -> Result<ExtensionDecl, ParseError> {
        let start = self.expect(Token::Extend, "expected `extend`")?.span.start;
        let ty = self.parse_type()?;
        self.expect(Token::Do, "expected `do` after extend type")?;
        self.consume_newlines();
        let mut members = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
            // Try field: [vis] type ident [= expr] terminator
            let save = self.pos;
            // Try operator: [vis] [static] operator <symbol> "(" ... ")" [where] block
            // Try property: [vis] [type] ident get/set ...
            // Try conversion: [vis] [explicit] convert Type to Type block
            // Try field and function with lookahead
            // Use helper to detect field vs function vs operator vs property vs conversion
            let vis = self.parse_visibility();
            let mut is_static = false;
            let mut save_static = self.pos;
            // Check for operator first (needs `operator` keyword)
            if self.peek_token() == Some(&Token::OperatorKw) {
                self.pos = save;
                // Parse operator as in class
                let op_vis = self.parse_visibility();
                let mut op_static = false;
                if self.peek_token() == Some(&Token::Static) { op_static = true; self.advance(); }
                if self.peek_token() == Some(&Token::OperatorKw) {
                    let op_start = self.advance().unwrap().span.start;
                    let op_tok = self.advance().ok_or(ParseError{message: "expected operator symbol".into(), span: self.peek_span()})?;
                    let op_str = match op_tok.token {
                        Token::Plus => "+".to_string(), Token::Minus => "-".to_string(), Token::Star => "*".to_string(),
                        Token::Slash => "/".to_string(), Token::Percent => "%".to_string(), Token::Lt => "<".to_string(),
                        Token::LtEq => "<=".to_string(), Token::Gt => ">".to_string(), Token::GtEq => ">=".to_string(),
                        Token::Is => { if self.peek_token() == Some(&Token::Not) { self.advance(); "is not".to_string() } else { "is".to_string() } },
                        Token::Ampersand => "&".to_string(), Token::Pipe => "|".to_string(), Token::Caret => "^".to_string(),
                        Token::Tilde => "~".to_string(), Token::LShift => "<<".to_string(), Token::RShift => ">>".to_string(),
                        Token::Eq => "=".to_string(), Token::PlusAssign => "+=".to_string(), Token::MinusAssign => "-=".to_string(),
                        Token::StarAssign => "*=".to_string(), Token::SlashAssign => "/=".to_string(), Token::PercentAssign => "%=".to_string(),
                        Token::AndAssign => "&=".to_string(), Token::OrAssign => "|=".to_string(), Token::XorAssign => "^=".to_string(),
                        Token::LShiftAssign => "<<=".to_string(), Token::RShiftAssign => ">>=".to_string(),
                        Token::PlusPlus => "++".to_string(), Token::MinusMinus => "--".to_string(),
                        Token::LBracket => { self.expect(Token::RBracket, "expected `]` for operator `[]`")?; "[]".to_string() },
                        _ => self.slice(op_tok.span).to_string(),
                    };
                    let op_span = op_tok.span;
                    self.expect(Token::LParen, "expected `(` after operator")?;
                    let mut params = Vec::new();
                    if self.peek_token() != Some(&Token::RParen) {
                        loop { let p = self.parse_param()?; params.push(p); if !self.consume_if(Token::Comma) { break; } }
                    }
                    self.expect(Token::RParen, "expected `)` after operator params")?;
                    let where_clause = if self.peek_token()==Some(&Token::Where) { self.parse_where_clause_opt() } else { None };
                    let body = self.parse_block()?;
                    let span = Span::new(op_start, body.span.end);
                    members.push(ExtensionMember::Operator(OperatorDecl{visibility: op_vis, is_static: op_static, op: op_str, op_span, params, body, where_clause, span}));
                    self.consume_newlines();
                    continue;
                }
                self.pos = save;
            }
            // Check for conversion: [vis] [explicit] convert
            if self.peek_token() == Some(&Token::Convert) || (vis != crate::ast::Visibility::Default && self.peek_token() == Some(&Token::Explicit)) {
                self.pos = save;
                let cvis = self.parse_visibility();
                let is_explicit = if self.peek_token() == Some(&Token::Explicit) { self.advance(); true } else { false };
                if self.peek_token() == Some(&Token::Convert) {
                    let cstart = self.advance().unwrap().span.start;
                    let from_ty = self.parse_type()?;
                    self.expect(Token::To, "expected `to` after convert source type")?;
                    let to_ty = self.parse_type()?;
                    let body = self.parse_block()?;
                    let span = Span::new(cstart, body.span.end);
                    members.push(ExtensionMember::Conversion(ConversionDecl{visibility: cvis, is_explicit, from_ty, to_ty, body, span}));
                    self.consume_newlines();
                    continue;
                }
                self.pos = save;
            }
            // Check for property: [type] ident get/set
            // Use save and try to detect property
            self.pos = save;
            let is_property = {
                let save2 = self.pos;
                let mut is_prop = false;
                if let Ok(_) = self.parse_type() {
                    if self.peek_token() == Some(&Token::Ident) {
                        let _ = self.parse_ident();
                        if matches!(self.peek_token(), Some(Token::Get) | Some(Token::Set)) { is_prop = true; }
                    }
                }
                if !is_prop {
                    self.pos = save2;
                    if self.peek_token() == Some(&Token::Ident) {
                        let _ = self.parse_ident();
                        if self.peek_token() == Some(&Token::Set) { is_prop = true; }
                    }
                }
                self.pos = save2;
                is_prop
            };
            if is_property {
                self.pos = save;
                let pvis = self.parse_visibility();
                let prop_vis = if pvis == crate::ast::Visibility::Default { crate::ast::Visibility::Public } else { pvis };
                let save3 = self.pos;
                let mut ty_opt = None;
                let mut pname = String::new();
                let mut pspan = Span::new(0,0);
                let mut parsed_with_type = false;
                if let Ok(t) = self.parse_type() {
                    if let Ok((n,s)) = self.parse_ident() {
                        if matches!(self.peek_token(), Some(Token::Get) | Some(Token::Set)) {
                            ty_opt = Some(t); pname = n; pspan = s; parsed_with_type = true;
                        }
                    }
                }
                if !parsed_with_type {
                    self.pos = save3;
                    let (n,s) = self.parse_ident()?;
                    pname = n; pspan = s; ty_opt = None;
                }
                let mut getter = None;
                let mut setter = None;
                if self.peek_token() == Some(&Token::Get) {
                    self.advance();
                    getter = Some(self.parse_block()?);
                    self.consume_newlines();
                    if self.peek_token() == Some(&Token::Set) {
                        self.advance();
                        self.expect(Token::LParen, "expected `(` for property setter")?;
                        let pty = self.parse_type()?;
                        let (pn, pn_span) = self.parse_ident()?;
                        let pspan = Span::new(pty.span().start, pn_span.end);
                        let param = Param{is_variadic: false, mode: ParamMode::None, ty: pty, name: pn, name_span: pn_span, default: None, span: pspan};
                        self.expect(Token::RParen, "expected `)` after setter param")?;
                        let body = self.parse_block()?;
                        setter = Some((param, body));
                    }
                } else if self.peek_token() == Some(&Token::Set) {
                    self.advance();
                    self.expect(Token::LParen, "expected `(` for property setter")?;
                    let pty = self.parse_type()?;
                    let (pn, pn_span) = self.parse_ident()?;
                    let pspan = Span::new(pty.span().start, pn_span.end);
                    let param = Param{is_variadic: false, mode: ParamMode::None, ty: pty, name: pn, name_span: pn_span, default: None, span: pspan};
                    self.expect(Token::RParen, "expected `)` after setter param")?;
                    let body = self.parse_block()?;
                    setter = Some((param, body));
                    self.consume_newlines();
                    if self.peek_token() == Some(&Token::Get) {
                        self.advance();
                        getter = Some(self.parse_block()?);
                    }
                } else {
                    return Err(ParseError{message: "expected `get` or `set` for property".into(), span: self.peek_span()});
                }
                let end = setter.as_ref().map(|(_,b)| b.span.end).or(getter.as_ref().map(|b| b.span.end)).unwrap_or(pspan.end);
                members.push(ExtensionMember::Property(PropertyDecl{ty: ty_opt, name: pname, name_span: pspan, visibility: prop_vis, getter, setter, span: Span::new(pspan.start, end)}));
                self.consume_newlines();
                continue;
            }
            // Try field: [vis] type ident [= expr] terminator
            self.pos = save;
            let fvis = self.parse_visibility();
            let save4 = self.pos;
            if let Ok(fty) = self.parse_type() {
                if let Ok((fname, fspan)) = self.parse_ident() {
                    let is_func = self.peek_token() == Some(&Token::LParen);
                    if !is_func {
                        let fend = fspan.end;
                        let default = if self.consume_if(Token::Eq) { Some(self.parse_expr()?) } else { None };
                        // Check terminator: must be newline/; or `end` (if not, it's not a field)
                        let next = self.peek_token().cloned();
                        if matches!(next, Some(Token::Newline) | Some(Token::Semicolon) | Some(Token::End) | None) || self.consume_if(Token::Eq) {
                            // Actually we already consumed default if any, so check terminator
                            // For field, we expect terminator
                            let save5 = self.pos;
                            if self.expect_terminator("field").is_ok() {
                                let span = Span::new(fty.span().start, fend);
                                members.push(ExtensionMember::Field(StructField{ty: fty, name: fname, name_span: fspan, visibility: fvis, default, span}));
                                self.consume_newlines();
                                continue;
                            } else {
                                self.pos = save5;
                            }
                        }
                    }
                }
            }
            self.pos = save;
            // Try function
            match self.parse_function() {
                Ok(func) => {
                    members.push(ExtensionMember::Function(func));
                    self.consume_newlines();
                    continue;
                }
                Err(_) => {
                    self.pos = save;
                    self.advance();
                }
            }
        }
        let end = self.expect(Token::End, "expected `end` to close extend")?.span.end;
        Ok(ExtensionDecl{ty, members, span: Span::new(start, end)})
    }

    fn parse_enum_decl(&mut self) -> Result<EnumDecl, ParseError> {
        if matches!(self.peek_token(), Some(Token::Public) | Some(Token::Private)) { self.advance(); }
        let start = self.expect(Token::Enum, "expected `enum`")?.span.start;
        let (name, name_span) = self.parse_ident()?;
        let generics = self.parse_generic_params_opt();
        let where_clause = self.parse_where_clause_opt();
        self.expect(Token::Has, "expected `has` after enum name")?;
        self.consume_newlines();
        let mut variants = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
            let (vname, vspan) = self.parse_ident()?;
            let mut discriminant = None;
            let mut payload_params = Vec::new();
            if self.consume_if(Token::Eq) {
                let expr = self.parse_expr()?;
                discriminant = Some(expr);
            }
            if self.peek_token() == Some(&Token::LParen) {
                self.advance(); // (
                if self.peek_token() != Some(&Token::RParen) {
                    loop {
                        // EBNF: parameter = [parameter-mode] type ident ["=" expression]
                        // For enum payload, we allow `type ident` with optional name; mode is always None for enum
                        let mode = if self.peek_token() == Some(&Token::Ref) {
                            self.advance();
                            ParamMode::Ref
                        } else if self.peek_token() == Some(&Token::Out) {
                            self.advance();
                            ParamMode::Out
                        } else {
                            ParamMode::None
                        };
                        let ty = self.parse_type()?;
                        let (pname, pspan) = if self.peek_token() == Some(&Token::Ident) {
                            let (n, ns) = self.parse_ident()?;
                            (n, ns)
                        } else {
                            // If no ident, use placeholder like `_payload0`
                            (format!("_payload{}", payload_params.len()), ty.span())
                        };
                        let pspan2 = Span::new(ty.span().start, pspan.end);
                        let mut default_expr = None;
                        if self.consume_if(Token::Eq) {
                            default_expr = Some(self.parse_expr()?);
                        }
                        payload_params.push(Param { is_variadic: false, mode, ty, name: pname, name_span: pspan, default: default_expr, span: pspan2 });
                        if !self.consume_if(Token::Comma) { break; }
                        if self.peek_token() == Some(&Token::RParen) { break; }
                    }
                }
                self.expect(Token::RParen, "expected `)` after enum payload")?;
            }
            self.expect_terminator("enum variant")?;
            let span = Span::new(vspan.start, vspan.end);
            variants.push(EnumVariant{name: vname, name_span: vspan, discriminant, payload_params, span});
            self.consume_newlines();
        }
        let end = self.expect(Token::End, "expected `end` to close enum")?.span.end;
        Ok(EnumDecl{name, name_span, generic_params: generics, where_clause, variants, span: Span::new(start, end)})
    }

    fn parse_import(&mut self) -> Result<ImportDecl, ParseError> {
        // EBNF §32: import qualified-name [:: { import-list }] terminator
        let start = self.expect(Token::Import, "expected `import`")?.span.start;
        let (first, fspan) = self.parse_ident()?;
        let mut path = vec![first];
        let mut path_end = fspan.end;
        while self.peek_token() == Some(&Token::ColonColon) {
            // Stop if this `::` introduces the `{` import list (e.g. `std::io::{a}`)
            if self.tokens.get(self.pos + 1).map(|t| t.token == Token::LBrace).unwrap_or(false) {
                break;
            }
            self.advance(); // ::
            let (seg, sspan) = self.parse_ident()?;
            path.push(seg);
            path_end = sspan.end;
        }
        // Optional :: { import-list }
        let mut symbols: Option<Vec<(String, Span)>> = None;
        if self.peek_token() == Some(&Token::ColonColon) {
            // Lookahead :: {
            if self.tokens.get(self.pos + 1).map(|t| t.token == Token::LBrace).unwrap_or(false) {
                self.advance(); // ::
                self.advance(); // {
                let mut list = Vec::new();
                // handle empty? EBNF requires at least one, but allow empty gracefully
                while !self.is_eof() && self.peek_token() != Some(&Token::RBrace) {
                    if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
                    let (nm, ns) = self.parse_ident()?;
                    list.push((nm, ns));
                    if self.consume_if(Token::Comma) { continue; } else { // allow trailing
                    }
                    // consume newlines between symbols
                    self.consume_newlines();
                }
                self.expect(Token::RBrace, "expected `}` to close import list")?;
                path_end = self.tokens[self.pos - 1].span.end;
                symbols = Some(list);
            }
        }
        // Also handle direct `::` already consumed? Support `import std::io::{a,b}` where the `::` before `{` is part of above.
        // Alternative form `import std::io :: {a}` already handled; handle plain `{` without leading `::`? EBNF requires `::`, so ignore.
        self.expect_terminator("import")?;
        let span = Span::new(start, path_end);
        let path_span = Span::new(start, path_end);
        Ok(ImportDecl{path, path_span, symbols, span})
    }

    fn parse_type(&mut self) -> Result<Type, ParseError> {
        self.parse_type_impl(true)
    }

    /// Type parser with optional map interpretation. `where T: Trait`
    /// constraints use `allow_map = false` so the constraint colon is not
    /// consumed as a `KEY:VALUE` map operator.
    fn parse_type_impl(&mut self, allow_map: bool) -> Result<Type, ParseError> {
        // Owning heap pointer: `own T` prefix (validated in sema).
        if self.peek_token() == Some(&Token::Own) {
            let own_tok = self.advance().unwrap();
            let inner = self.parse_type_impl(allow_map)?;
            let span = Span::new(own_tok.span.start, inner.span().end);
            return Ok(Type::Own(Box::new(inner), span));
        }
        // primary-type
        let mut ty: Type = {
            let st = self.peek().cloned().ok_or(ParseError {
                message: "expected type".into(),
                span: Span::new(self.source_end(), self.source_end()),
            })?;
            match st.token {
                Token::Int => {
                    self.advance();
                    Type::Int(st.span)
                }
                Token::Bool => {
                    self.advance();
                    Type::Bool(st.span)
                }
                Token::Void => {
                    self.advance();
                    Type::Void(st.span)
                }
                Token::StringKw => {
                    self.advance();
                    Type::String(st.span)
                }
                Token::CharKw => {
                    self.advance();
                    Type::Char(st.span)
                }
                Token::Float => {
                    self.advance();
                    Type::Float(st.span)
                }
                Token::Double => {
                    self.advance();
                    Type::Double(st.span)
                }
                Token::Any => {
                    self.advance();
                    Type::Any(st.span)
                }
                Token::Function => {
                    self.advance();
                    self.expect(Token::Lt, "expected `<` after `function`")?;
                    let ret_ty = if self.peek_token() == Some(&Token::LParen) {
                        Type::Void(st.span)
                    } else {
                        self.parse_type()?
                    };
                    self.expect(Token::LParen, "expected `(` after function return type")?;
                    let mut args = Vec::new();
                    if self.peek_token() != Some(&Token::RParen) {
                        loop {
                            let aty = self.parse_type()?;
                            args.push(aty);
                            if !self.consume_if(Token::Comma) { break; }
                        }
                    }
                    self.expect(Token::RParen, "expected `)` after function params")?;
                    self.expect(Token::Gt, "expected `>` after function type")?;
                    let span = Span::new(st.span.start, self.tokens[self.pos-1].span.end);
                    Type::FunctionType(Box::new(ret_ty), args, span)
                }
                Token::LParen => {
                    self.advance();
                    if self.peek_token() == Some(&Token::RParen) {
                        self.advance();
                        let span = Span::new(st.span.start, self.tokens[self.pos-1].span.end);
                        Type::Tuple(vec![], span)
                    } else {
                        let first = self.parse_type()?;
                        if self.consume_if(Token::Comma) {
                            let mut tys = vec![first];
                            if self.peek_token() != Some(&Token::RParen) {
                                loop {
                                    if self.peek_token() == Some(&Token::RParen) { break; }
                                    let aty = self.parse_type()?;
                                    tys.push(aty);
                                    if !self.consume_if(Token::Comma) { break; }
                                    if self.peek_token() == Some(&Token::RParen) { break; }
                                }
                            }
                            self.expect(Token::RParen, "expected `)` after tuple types")?;
                            let span = Span::new(st.span.start, self.tokens[self.pos-1].span.end);
                            Type::Tuple(tys, span)
                        } else {
                            self.expect(Token::RParen, "expected `)` after type")?;
                            first
                        }
                    }
                }
                Token::Ident => {
                    self.advance();
                    let mut name = self.slice(st.span).to_string();
                    let mut end = st.span.end;
                    while self.peek_token() == Some(&Token::ColonColon) {
                        self.advance();
                        let (seg, sspan) = self.parse_ident()?;
                        name.push_str("::");
                        name.push_str(&seg);
                        end = sspan.end;
                    }
                    Type::Named(name, Span::new(st.span.start, end))
                }
                _ => {
                    return Err(ParseError {
                        message: format!("expected type, found `{}`", st.token),
                        span: st.span,
                    });
                }
            }
        };
        // Handle generic-type: Named<type,...> — including `task<T>`
        // (Async-1: `task<T>` is a pending computation handle).
        if self.peek_token() == Some(&Token::Lt) {
            // Try parse generic args - only if ty is Named
            if let Type::Named(n, s) = ty.clone() {
                let save = self.pos;
                self.advance(); // <
                let mut args = Vec::new();
                let mut ok = true;
                if self.peek_token() != Some(&Token::Gt) {
                    loop {
                        match self.parse_type() {
                            Ok(arg) => args.push(arg),
                            Err(_) => { ok = false; break; }
                        }
                        if !self.consume_if(Token::Comma) { break; }
                    }
                }
                if ok && self.consume_if(Token::Gt) {
                    let span = Span::new(s.start, self.tokens[self.pos-1].span.end);
                    if n == "task" {
                        if args.len() != 1 {
                            return Err(ParseError {
                                message: format!("`task` takes exactly one type argument, found {}", args.len()),
                                span,
                            });
                        }
                        ty = Type::Task(Box::new(args.into_iter().next().unwrap()), span);
                    } else {
                        ty = Type::Generic(n, args, span);
                    }
                } else {
                    // rollback not needed for now
                    self.pos = save;
                }
            }
        }
        // Fixed-size array suffix (types skill §7-8): `TYPE arr` (inferred)
        // or `TYPE arr[SIZE]` (explicit). `arr` is a compiler keyword.
        if self.peek_token() == Some(&Token::Arr) {
            let arr_tok = self.advance().unwrap();
            let mut size: Option<u64> = None;
            let mut end = arr_tok.span.end;
            if self.peek_token() == Some(&Token::LBracket) {
                self.advance(); // [
                if self.peek_token() == Some(&Token::RBracket) {
                    self.advance(); // `arr[]` — treat as inferred
                    end = self.tokens[self.pos - 1].span.end;
                } else {
                    let size_tok = self.peek().cloned().ok_or(ParseError {
                        message: "expected array size after `arr[`".into(),
                        span: Span::new(self.source_end(), self.source_end()),
                    })?;
                    match size_tok.token {
                        Token::IntLit | Token::HexInt | Token::BinInt => {
                            self.advance();
                            let n = self.parse_int_lit(size_tok.span)?;
                            if n < 0 {
                                return Err(ParseError {
                                    message: "array size must be non-negative".into(),
                                    span: size_tok.span,
                                });
                            }
                            size = Some(n as u64);
                            let rb = self.expect(Token::RBracket, "expected `]` after array size")?;
                            end = rb.span.end;
                        }
                        _ => {
                            return Err(ParseError {
                                message: format!("expected array size, found `{}`", size_tok.token),
                                span: size_tok.span,
                            });
                        }
                    }
                }
            }
            let span = Span::new(ty.span().start, end);
            ty = Type::FixedArray { elem: Box::new(ty), size, span };
        }
        // Dynamic vector suffix (types skill §9): `TYPE vec`. Unlike `arr`,
        // vectors take no size — they own their storage and may grow.
        // (`vec[]` empty construction is an *expression*, parsed in primary.)
        if self.peek_token() == Some(&Token::Vec) {
            let vec_tok = self.advance().unwrap();
            let span = Span::new(ty.span().start, vec_tok.span.end);
            ty = Type::Vec { elem: Box::new(ty), span };
        }
        // Map type (types skill §12): `KEY_TYPE:VALUE_TYPE` (no `map`
        // keyword). `:` here is distinct from `::` (qualified names tokenize
        // as one `ColonColon`). Value is a full type, so `string:int[]` is a
        // map to an array; `(string:int)[]` is an array of maps.
        // Disabled inside `where` constraints (`where T: Trait`).
        if allow_map && self.peek_token() == Some(&Token::Colon) {
            self.advance(); // :
            let value = self.parse_type()?;
            let span = Span::new(ty.span().start, value.span().end);
            ty = Type::Map { key: Box::new(ty), value: Box::new(value), span };
        }
        // { type-modifier } : "?" | "*" | "[]"
        loop {
            if self.peek_token() == Some(&Token::Question) {
                let q = self.advance().unwrap();
                let span = Span::new(ty.span().start, q.span.end);
                ty = Type::Optional(Box::new(ty), span);
                continue;
            }
            if self.peek_token() == Some(&Token::Star) {
                let star = self.advance().unwrap();
                let span = Span::new(ty.span().start, star.span.end);
                ty = Type::Pointer(Box::new(ty), span);
                continue;
            }
            if self.peek_token() == Some(&Token::LBracket) {
                let rb = self.tokens.get(self.pos + 1);
                if rb.map(|t| t.token == Token::RBracket).unwrap_or(false) {
                    self.advance();
                    self.advance();
                    let end = self.tokens[self.pos - 1].span.end;
                    let span = Span::new(ty.span().start, end);
                    ty = Type::Array(Box::new(ty), span);
                    continue;
                }
            }
            break;
        }
        Ok(ty)
    }

    fn parse_param(&mut self) -> Result<Param, ParseError> {
        let is_variadic = self.consume_if(Token::DotDotDot);
        let mode = if self.peek_token() == Some(&Token::Ref) {
            self.advance();
            ParamMode::Ref
        } else if self.peek_token() == Some(&Token::Out) {
            self.advance();
            ParamMode::Out
        } else {
            ParamMode::None
        };
        // For variadic with derived type `... vda` (no explicit type), type is derived from previous param
        // Detect `... vda` where after `...` (and optional mode) we have Ident and next is `=` or `,` or `)`
        let mut ty_opt: Option<Type> = None;
        let mut name_opt: Option<(String, Span)> = None;
        if is_variadic {
            // Check if next is Ident and following is `=` or `,` or `)` => derived `... vda`
            if self.peek_token() == Some(&Token::Ident) {
                let next_is_eq_or_delim = self.tokens.get(self.pos + 1).map(|t| matches!(t.token, Token::Eq | Token::Comma | Token::RParen)).unwrap_or(false);
                // Also need to check if next after Ident is `=` etc, and not another Ident (which would be `...int vda` where first Ident is type)
                // For `...int vda`, after `...` we have `int` (type keyword) not Ident, so not this case
                // For `...T vda` where T is Ident type, after `...` we have `T` (Ident) and then `vda` (Ident) then `=`/`,`/`)`
                // So for `... vda` derived, we have single Ident after `...` then delimiter
                if next_is_eq_or_delim {
                    // derived: `... vda`
                    let (n, ns) = self.parse_ident()?;
                    let pspan = Span::new(ns.start, ns.end);
                    let default = if self.consume_if(Token::Eq) {
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    return Ok(Param { is_variadic: true, mode, ty: Type::Named("__derived__".to_string(), ns), name: n, name_span: ns, default, span: pspan });
                }
            }
        }
        let ty = self.parse_type()?;
        let (pn, pn_span) = self.parse_ident()?;
        let pspan = Span::new(ty.span().start, pn_span.end);
        let default = if self.consume_if(Token::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Param { is_variadic, mode, ty, name: pn, name_span: pn_span, default, span: pspan })
    }

    fn parse_call_arg(&mut self) -> Result<CallArg, ParseError> {
        // out [type] ident
        if self.peek_token() == Some(&Token::Out) {
            let start = self.advance().unwrap().span.start;
            // Lookahead to distinguish `out Type ident` vs `out ident`
            // If next token is primitive type keyword, treat as typed
            // If next is Ident, look at following token to decide
            let is_typed = match self.peek_token() {
                Some(Token::Int) | Some(Token::Bool) | Some(Token::Void) | Some(Token::StringKw)
                | Some(Token::CharKw) | Some(Token::Float) | Some(Token::Double) | Some(Token::Any) | Some(Token::Function) => true,
                Some(Token::Ident) => {
                    // Need to see if there is a second Ident after potential type
                    // Type can be `Ident` or `Ident::Ident` or generic, but for simple check, if we have Ident + Ident before comma/paren, it's typed
                    if let Some(next2) = self.tokens.get(self.pos + 1) {
                        matches!(next2.token, Token::Ident) || matches!(next2.token, Token::ColonColon) || matches!(next2.token, Token::Lt) || matches!(next2.token, Token::Question) || matches!(next2.token, Token::Star) || matches!(next2.token, Token::LBracket)
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if is_typed {
                // parse type then ident
                let ty = self.parse_type()?;
                let (name, nspan) = self.parse_ident()?;
                let span = Span::new(start, nspan.end);
                return Ok(CallArg::Out { ty: Some(ty), name, name_span: nspan, span });
            } else {
                // untyped: `out ident`
                // For `out x` where x was peeked as Ident, it will be here
                // Also handle case where we incorrectly thought typed but it's actually single Ident typed case where type == ident and next is , or )
                // The above is_typed logic already handles single Ident as untyped, so safe
                let (name, nspan) = self.parse_ident().map_err(|_| ParseError { message: "expected identifier after `out`".into(), span: self.peek_span() })?;
                let span = Span::new(start, nspan.end);
                return Ok(CallArg::Out { ty: None, name, name_span: nspan, span });
            }
        }
        if self.peek_token() == Some(&Token::Ref) {
            let start = self.advance().unwrap().span.start;
            let expr = self.parse_expr()?;
            let span = Span::new(start, expr.span.end);
            return Ok(CallArg::Ref { expr: Box::new(expr), span });
        }
        // named-argument: ident ':' expr  (but not '::' )
        if self.peek_token() == Some(&Token::Ident) {
            if self.tokens.get(self.pos + 1).map(|t| t.token == Token::Colon).unwrap_or(false) {
                let (name, nspan) = self.parse_ident()?;
                self.advance(); // :
                let value = self.parse_expr()?;
                let span = Span::new(nspan.start, value.span.end);
                return Ok(CallArg::Named { name, name_span: nspan, value, span });
            }
        }
        let expr = self.parse_expr()?;
        Ok(CallArg::Expr(expr))
    }

    fn parse_call_args(&mut self) -> Result<Vec<CallArg>, ParseError> {
        let mut args = Vec::new();
        if self.peek_token() == Some(&Token::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.parse_call_arg()?);
            if !self.consume_if(Token::Comma) {
                break;
            }
            // allow trailing comma before )
            if self.peek_token() == Some(&Token::RParen) {
                break;
            }
        }
        Ok(args)
    }

    fn parse_generic_params_opt(&mut self) -> Vec<GenericParam> {
        if self.peek_token() != Some(&Token::Lt) { return Vec::new(); }
        self.advance(); // <
        let mut params = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::Gt) {
            if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
            let (name, span) = match self.parse_ident() {
                Ok(v) => v,
                Err(_) => break,
            };
            let mut bounds = Vec::new();
            if self.consume_if(Token::Colon) {
                loop {
                    bounds.push(self.parse_type().unwrap_or(Type::Named("any".into(), span)));
                    if !self.consume_if(Token::Comma) { break; }
                }
            }
            params.push(GenericParam {name, name_span: span, bounds, span});
            if !self.consume_if(Token::Comma) { break; }
        }
        let _ = self.expect(Token::Gt, "expected `>` after generic params");
        params
    }

    fn parse_where_clause_opt(&mut self) -> Option<WhereClause> {
        if self.peek_token() != Some(&Token::Where) { return None; }
        let start = self.advance().unwrap().span.start;
        let mut constraints = Vec::new();
        while !self.is_eof() && !matches!(self.peek_token(), Some(Token::Has) | Some(Token::Do) | Some(Token::End) | Some(Token::Newline) | Some(Token::Semicolon)) {
            // `where T: Trait` — the constraint colon is not a map operator.
            let ty = match self.parse_type_impl(false) { Ok(t) => t, Err(_) => break };
            if !self.consume_if(Token::Colon) { break; }
            let mut bounds = Vec::new();
            loop {
                match self.parse_type_impl(false) {
                    Ok(t) => bounds.push(t),
                    Err(_) => break,
                }
                if !self.consume_if(Token::Comma) { break; }
            }
            let span = Span::new(ty.span().start, bounds.last().map(|b| b.span().end).unwrap_or(ty.span().end));
            constraints.push(WhereConstraint{ty, bounds, span});
            if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.consume_newlines(); }
            else { break; }
        }
        Some(WhereClause{constraints, span: Span::new(start, self.peek_span().end)})
    }

    fn parse_ident(&mut self) -> Result<(String, Span), ParseError> {
        let st = self.peek().cloned().ok_or(ParseError {
            message: "expected identifier".into(),
            span: Span::new(self.source_end(), self.source_end()),
        })?;
        if st.token == Token::Ident {
            self.advance();
            Ok((self.slice(st.span).to_string(), st.span))
        } else {
            Err(ParseError {
                message: format!("expected identifier, found `{}`", st.token),
                span: st.span,
            })
        }
    }

    fn parse_function(&mut self) -> Result<Function, ParseError> {
        let vis = self.parse_visibility();
        let mut is_static = false;
        let mut is_sealed = false;
        let mut is_override = false;
        let mut is_open = false;
        loop {
            match self.peek_token() {
                Some(Token::Static) => { is_static = true; self.advance(); },
                Some(Token::Sealed) => { is_sealed = true; self.advance(); },
                Some(Token::Override) => { is_override = true; self.advance(); },
                Some(Token::Open) => { is_open = true; self.advance(); },
                _ => break,
            }
        }
        let start_span = self.peek_span();
        let ret_ty = self.parse_type()?;
        let (name, name_span) = self.parse_ident()?;
        // Phase 5: generic params <T, U>
        let generic_params = self.parse_generic_params_opt();
        self.expect(Token::LParen, "function params")?;
        let mut params = Vec::new();
        if self.peek_token() != Some(&Token::RParen) {
            loop {
                let param = self.parse_param()?;
                params.push(param);
                if self.consume_if(Token::Comma) {
                    continue;
                } else {
                    break;
                }
            }
        }
        self.expect(Token::RParen, "closing `)`")?;
        let where_clause = self.parse_where_clause_opt();
        // `Ret name(params) [where ...] async do ... end` (Async-1): the
        // optional `async` marker sits between the signature and the body.
        // Methods parse their own body below; async methods are added in
        // Async-2 with sema/codegen support.
        let is_async = self.consume_if(Token::Async);
        let async_span = if is_async {
            Some(self.tokens[self.pos - 1].span)
        } else {
            None
        };
        // function body: block (do ... end) - `initialize` is constructor-only, not free fns
        let body = self.parse_block()?;
        let span = Span::new(start_span.start, body.span.end);
        Ok(Function {
            ret_ty,
            name,
            name_span,
            params,
            body,
            visibility: vis,
            is_static,
            is_sealed,
            is_override,
            is_open,
            is_async,
            async_span,
            generic_params,
            where_clause,
            span,
        })
    }

    fn parse_block(&mut self) -> Result<Block, ParseError> {
        let start = self
            .expect(Token::Do, "expected `do` to start block")?
            .span
            .start;
        self.consume_newlines();
        let mut stmts = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            // allow blank lines inside block
            if matches!(
                self.peek_token(),
                Some(Token::Newline) | Some(Token::Semicolon)
            ) {
                self.advance();
                continue;
            }
            let stmt = self.parse_stmt()?;
            stmts.push(stmt);
            // terminators already handled inside parse_stmt; consume extra newlines
            self.consume_newlines();
        }
        let end = self
            .expect(Token::End, "expected `end` to close block")?
            .span
            .end;
        Ok(Block {
            stmts,
            span: Span::new(start, end),
        })
    }

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        // Check for labeled loop/for:  ident ":" (loop|for)
        if self.peek_token() == Some(&Token::Ident) && self.tokens.get(self.pos+1).map(|t| t.token == Token::Colon).unwrap_or(false) {
            let label_tok = self.tokens[self.pos].clone();
            // Look ahead after colon
            let after_colon = self.tokens.get(self.pos+2).map(|t| &t.token);
            if matches!(after_colon, Some(Token::Loop)) {
                let label = self.slice(label_tok.span).to_string();
                let label_span = label_tok.span;
                self.advance(); // ident
                self.advance(); // :
                let l = self.parse_loop(Some((label, label_span)))?;
                return Ok(Stmt::Loop(l));
            } else if matches!(after_colon, Some(Token::For)) {
                let label = self.slice(label_tok.span).to_string();
                let label_span = label_tok.span;
                self.advance(); // ident
                self.advance(); // :
                let f = self.parse_for(Some((label, label_span)))?;
                return Ok(Stmt::For(f));
            }
        }
        match self.peek_token() {
            Some(Token::If) => {
                let s = self.parse_if()?;
                Ok(Stmt::If(s))
            }
            Some(Token::While) => {
                let s = self.parse_while()?;
                Ok(Stmt::While(s))
            }
            Some(Token::Loop) => {
                let l = self.parse_loop(None)?;
                Ok(Stmt::Loop(l))
            }
            Some(Token::For) => {
                let f = self.parse_for(None)?;
                Ok(Stmt::For(f))
            }
            Some(Token::Defer) => {
                let d = self.parse_defer()?;
                Ok(Stmt::Defer(d))
            }
            Some(Token::Delete) => {
                let start = self.advance().unwrap().span.start;
                let target = self.parse_expr()?;
                let end = target.span.end;
                self.expect_terminator("delete statement")?;
                Ok(Stmt::Delete(DeleteStmt {
                    target,
                    span: Span::new(start, end),
                }))
            }
            Some(Token::Return) => {
                let s = self.parse_return()?;
                Ok(Stmt::Return(s))
            }
            Some(Token::Break) => {
                let s = self.parse_break()?;
                Ok(Stmt::Break(s))
            }
            Some(Token::Continue) => {
                let s = self.parse_continue()?;
                Ok(Stmt::Continue(s))
            }
            Some(Token::Assert) | Some(Token::DebugAssert) => {
                let a = self.parse_assert()?;
                Ok(Stmt::Assert(a))
            }
            Some(Token::Do) => {
                let b = self.parse_block()?;
                Ok(Stmt::Block(b))
            }
            Some(Token::Const) => {
                let c = self.parse_const_decl()?;
                Ok(Stmt::Const(c))
            }
            Some(Token::Scope) => {
                // `scope do ... end` (Async-1): structured-concurrency scope.
                self.advance(); // scope
                let b = self.parse_block()?;
                Ok(Stmt::Scope(b))
            }
            Some(Token::Yield) => {
                // `yield` with optional terminator (Async-1).
                let start = self.advance().unwrap().span.start;
                let end = self.tokens.get(self.pos).map(|t| t.span.end).unwrap_or(start);
                let _ = self.consume_newlines();
                Ok(Stmt::Yield(Span::new(start, end)))
            }
            Some(Token::Public) | Some(Token::Private) => {
                // Could be `public const` or `private const` or typed var decl with visibility
                let is_const = self.tokens.get(self.pos + 1).map(|t| t.token == Token::Const).unwrap_or(false);
                if is_const {
                    let c = self.parse_const_decl()?;
                    Ok(Stmt::Const(c))
                } else if self.is_var_decl_start() {
                    let d = self.parse_var_decl()?;
                    Ok(Stmt::VarDecl(d))
                } else {
                    let e = self.parse_expr_stmt()?;
                    Ok(Stmt::Expr(e))
                }
            }
            _ => {
                if self.is_destructure_start() {
                    let d = self.parse_destructure()?;
                    Ok(Stmt::Destructure(d))
                } else if self.is_var_decl_start() {
                    let d = self.parse_var_decl()?;
                    Ok(Stmt::VarDecl(d))
                } else {
                    let e = self.parse_expr_stmt()?;
                    Ok(Stmt::Expr(e))
                }
            }
        }
    }

    fn is_var_decl_start(&mut self) -> bool {
        let save = self.pos;
        // Handle optional visibility `public`/`private`
        let _vis = self.parse_visibility();
        // Try parse a type; if fails, not a decl
        let ty = match self.parse_type() {
            Ok(t) => t,
            Err(_) => {
                self.pos = save;
                return false;
            }
        };
        // After type, next token must be Ident (var name)
        if !matches!(self.peek_token(), Some(Token::Ident)) {
            self.pos = save;
            return false;
        }
        // Consume ident for lookahead
        let _ = self.parse_ident();
        let next = self.peek_token().cloned();
        self.pos = save;
        let _ = ty;
        // Variable decl if next is `=` or terminator (newline/; / End / EOF), not `(` (which is function)
        matches!(next, Some(Token::Eq) | Some(Token::Newline) | Some(Token::Semicolon) | Some(Token::End) | None)
    }

    fn parse_var_decl(&mut self) -> Result<VarDecl, ParseError> {
        let visibility = self.parse_visibility();
        let ty = self.parse_type()?;
        let (name, name_span) = self.parse_ident()?;
        let mut init = None;
        let mut end = name_span.end;
        if self.consume_if(Token::Eq) {
            // Support omitted Type in struct/class literal: `User u2 = has ... end`
            // When RHS starts with `has` without preceding Type, infer type from LHS
            self.consume_newlines();
            if self.peek_token() == Some(&Token::Has) {
                let has_tok = self.advance().unwrap();
                let _ = has_tok;
                self.consume_newlines();
                // Bare `has ... end` with a map declaration type is a map
                // literal (`string:int m = has "k": v end`); with `any` try
                // map entries first, falling back to struct fields.
                let want_map = matches!(ty, Type::Map { .. });
                let try_map_first = want_map || matches!(ty, Type::Any(_));
                if try_map_first {
                    let save_entries = self.pos;
                    let mut entries = Vec::new();
                    let mut is_map = true;
                    while !self.is_eof() && self.peek_token() != Some(&Token::End) {
                        if matches!(
                            self.peek_token(),
                            Some(Token::Newline) | Some(Token::Semicolon) | Some(Token::Comma)
                        ) {
                            self.advance();
                            continue;
                        }
                        // A map entry starts with a key expression followed by `:`.
                        // Struct fields start with `Ident =`; probe without committing.
                        let save_entry = self.pos;
                        let key = match self.parse_expr() {
                            Ok(k) => k,
                            Err(_) => {
                                is_map = false;
                                break;
                            }
                        };
                        if self.peek_token() != Some(&Token::Colon) {
                            is_map = false;
                            self.pos = save_entry;
                            break;
                        }
                        self.advance(); // :
                        let value = match self.parse_expr() {
                            Ok(v) => v,
                            Err(_) => {
                                is_map = false;
                                self.pos = save_entry;
                                break;
                            }
                        };
                        entries.push((key, value));
                        if self.consume_if(Token::Comma) {
                            self.consume_newlines();
                            continue;
                        }
                        self.consume_newlines();
                        if matches!(self.peek_token(), Some(Token::Semicolon)) {
                            self.advance();
                        }
                    }
                    if is_map {
                        // For `any`, an empty `has end` is still a (empty) map.
                        let end_tok = self.expect(Token::End, "expected `end` to close map literal")?;
                        let span = Span::new(ty.span().start, end_tok.span.end);
                        let lit = Expr{kind: ExprKind::MapLit{ty: ty.clone(), entries}, span};
                        end = lit.span.end;
                        init = Some(lit);
                        let term_start = end;
                        self.expect_terminator("variable declaration")?;
                        let span = Span::new(ty.span().start, term_start);
                        return Ok(VarDecl {
                            visibility,
                            ty,
                            name,
                            name_span,
                            init,
                            span,
                        });
                    }
                    // Not map entries after all: rewind (unless the type
                    // demands a map) and parse struct fields.
                    if want_map {
                        // `string:int m = has <non-map entries>` — surface the
                        // entry error rather than a struct-field error.
                        return Err(ParseError {
                            message: "expected `key: value` entries in map literal".into(),
                            span: self.peek_span(),
                        });
                    }
                    self.pos = save_entries;
                }
                let mut fields = Vec::new();
                while !self.is_eof() && self.peek_token() != Some(&Token::End) {
                    if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
                    let (fname, fspan) = self.parse_ident()?;
                    self.expect(Token::Eq, "expected `=` in struct literal")?;
                    let fexpr = self.parse_expr()?;
                    self.expect_terminator("struct field initializer")?;
                    fields.push((fname, fspan, fexpr));
                    self.consume_newlines();
                }
                let end_tok = self.expect(Token::End, "expected `end` to close struct literal")?;
                let span = Span::new(ty.span().start, end_tok.span.end);
                let lit = Expr{kind: ExprKind::StructLit{ty: ty.clone(), fields}, span};
                end = lit.span.end;
                init = Some(lit);
            } else {
                let expr = self.parse_expr()?;
                end = expr.span.end;
                init = Some(expr);
            }
        }
        let term_start = end;
        self.expect_terminator("variable declaration")?;
        // span from ty to term
        let span = Span::new(ty.span().start, term_start);
        // Note: we consumed terminators already; span ends at init/end.
        Ok(VarDecl {
            visibility,
            ty,
            name,
            name_span,
            init,
            span,
        })
    }

    fn parse_if(&mut self) -> Result<IfStmt, ParseError> {
        let start = self.expect(Token::If, "if")?.span.start;
        self.consume_if(Token::LParen);
        let cond = self.parse_expr()?;
        self.consume_if(Token::RParen);
        let then_block = self.parse_block()?;
        // handle else if / else chain
        let else_block = if self.consume_if(Token::Else) {
            if self.peek_token() == Some(&Token::If) {
                // else if -> recursive if parsed as block containing single if stmt?
                // EBNF expands to else if block, we model as else { if }
                let nested_if = self.parse_if()?;
                // Wrap nested_if in a block for uniform else_block
                let span = nested_if.span;
                Some(Block {
                    stmts: vec![Stmt::If(nested_if)],
                    span,
                })
            } else {
                Some(self.parse_block()?)
            }
        } else {
            None
        };
        let end = else_block
            .as_ref()
            .map(|b| b.span.end)
            .unwrap_or(then_block.span.end);
        Ok(IfStmt {
            cond,
            then_block,
            else_block,
            span: Span::new(start, end),
        })
    }

    fn parse_while(&mut self) -> Result<WhileStmt, ParseError> {
        let start = self.expect(Token::While, "while")?.span.start;
        self.consume_if(Token::LParen);
        let cond = self.parse_expr()?;
        self.consume_if(Token::RParen);
        let body = self.parse_block()?;
        let span = Span::new(start, body.span.end);
        Ok(WhileStmt { cond, body, span })
    }

    fn parse_return(&mut self) -> Result<ReturnStmt, ParseError> {
        let start = self.expect(Token::Return, "return")?.span.start;
        // return may have expr or not; peek terminator/end/else
        let needs_semi = matches!(
            self.peek_token(),
            Some(Token::Newline)
                | Some(Token::Semicolon)
                | Some(Token::End)
                | None
        );
        let value = if needs_semi {
            None
        } else {
            Some(self.parse_expr()?)
        };
        let end = value.as_ref().map(|e| e.span.end).unwrap_or(start + 6);
        self.expect_terminator("return")?;
        let span = Span::new(start, end);
        Ok(ReturnStmt { value, span })
    }

    fn parse_break(&mut self) -> Result<BreakStmt, ParseError> {
        let start = self.expect(Token::Break, "break")?.span.start;
        let mut end = start + 5;
        let mut label = None;
        if self.peek_token() == Some(&Token::Ident) {
            let (name, span) = self.parse_ident()?;
            label = Some(name);
            end = span.end;
        }
        self.expect_terminator("break")?;
        Ok(BreakStmt { label, span: Span::new(start, end) })
    }

    fn parse_continue(&mut self) -> Result<ContinueStmt, ParseError> {
        let start = self.expect(Token::Continue, "continue")?.span.start;
        let mut end = start + 8;
        let mut label = None;
        if self.peek_token() == Some(&Token::Ident) {
            let (name, span) = self.parse_ident()?;
            label = Some(name);
            end = span.end;
        }
        self.expect_terminator("continue")?;
        Ok(ContinueStmt { label, span: Span::new(start, end) })
    }

    fn parse_loop(&mut self, label_opt: Option<(String, Span)>) -> Result<LoopStmt, ParseError> {
        let start = if let Some((_, ls)) = &label_opt { ls.start } else { self.peek_span().start };
        self.expect(Token::Loop, "loop")?;
        let body = self.parse_block()?;
        let end = body.span.end;
        let (label, label_span) = match label_opt { Some((n,s)) => (Some(n), Some(s)), None => (None, None) };
        let span = Span::new(start, end);
        Ok(LoopStmt { label, label_span, body, span })
    }

    fn parse_for(&mut self, label_opt: Option<(String, Span)>) -> Result<ForStmt, ParseError> {
        let start = if let Some((_, ls)) = &label_opt { ls.start } else { self.peek_span().start };
        self.expect(Token::For, "for")?;
        let (var, var_span) = self.parse_ident()?;
        // Optional second variable: `for x, i in ...` (index) or
        // `for k, v in ...` over maps (value).
        let var2 = if self.consume_if(Token::Comma) {
            let (v2, s2) = self.parse_ident()?;
            Some((v2, s2))
        } else {
            None
        };
        self.expect(Token::In, "expected `in` after for variable")?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;
        let end = body.span.end;
        let (label, label_span) = match label_opt { Some((n,s)) => (Some(n), Some(s)), None => (None, None) };
        Ok(ForStmt { label, label_span, var, var_span, var2, iter, body, span: Span::new(start, end) })
    }

    fn parse_defer(&mut self) -> Result<DeferStmt, ParseError> {
        let start = self.expect(Token::Defer, "defer")?.span.start;
        if self.peek_token() == Some(&Token::Do) {
            let blk = self.parse_block()?;
            let end = blk.span.end;
            return Ok(DeferStmt { inner: DeferInner::Block(blk), span: Span::new(start, end) });
        }
        let expr = self.parse_expr()?;
        let end = expr.span.end;
        self.expect_terminator("defer")?;
        Ok(DeferStmt { inner: DeferInner::Expr(Box::new(expr)), span: Span::new(start, end) })
    }

    fn parse_expr_stmt(&mut self) -> Result<ExprStmt, ParseError> {
        let expr = self.parse_expr()?;
        let span = expr.span;
        self.expect_terminator("expression statement")?;
        Ok(ExprStmt { expr, span })
    }

    // ── Expressions (Pratt) ────────────────────────────────────────────

    pub fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_assignment()
    }

    fn parse_assignment(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_conditional()?;
        let is_assign = matches!(
            self.peek_token(),
            Some(Token::Eq)
                | Some(Token::PlusAssign)
                | Some(Token::MinusAssign)
                | Some(Token::StarAssign)
                | Some(Token::SlashAssign)
                | Some(Token::PercentAssign)
                | Some(Token::AndAssign)
                | Some(Token::OrAssign)
                | Some(Token::XorAssign)
                | Some(Token::LShiftAssign)
                | Some(Token::RShiftAssign)
        );
        if !is_assign {
            return Ok(lhs);
        }
        // Allow Ident, MemberAccess, Index, NullableMemberAccess as lvalue
        let is_lvalue = matches!(
            lhs.kind,
            ExprKind::Ident(_)
                | ExprKind::MemberAccess { .. }
                | ExprKind::NullableMemberAccess { .. }
                | ExprKind::Index { .. }
                | ExprKind::Paren(_)
        );
        if !is_lvalue {
            return Err(ParseError {
                message: "assignment target must be identifier or field access".into(),
                span: lhs.span,
            });
        }
        if let ExprKind::Paren(_) = lhs.kind {
            return Err(ParseError {
                message: "cannot assign to parenthesized expression".into(),
                span: lhs.span,
            });
        }
        let op_tok = self.advance().unwrap();
        let lhs_span = lhs.span;
        let rhs = self.parse_assignment()?;
        let span = Span::new(lhs_span.start, rhs.span.end);
        if op_tok.token == Token::Eq {
            return Ok(Expr {
                kind: ExprKind::Assign {
                    lhs: Box::new(lhs),
                    value: Box::new(rhs),
                },
                span,
            });
        } else {
            let op = match op_tok.token {
                Token::PlusAssign => BinOp::CompoundAdd,
                Token::MinusAssign => BinOp::CompoundSub,
                Token::StarAssign => BinOp::CompoundMul,
                Token::SlashAssign => BinOp::CompoundDiv,
                Token::PercentAssign => BinOp::CompoundMod,
                Token::AndAssign => BinOp::CompoundBitAnd,
                Token::OrAssign => BinOp::CompoundBitOr,
                Token::XorAssign => BinOp::CompoundBitXor,
                Token::LShiftAssign => BinOp::CompoundShl,
                Token::RShiftAssign => BinOp::CompoundShr,
                _ => unreachable!(),
            };
            return Ok(Expr {
                kind: ExprKind::CompoundAssign {
                    op,
                    lhs: Box::new(lhs),
                    value: Box::new(rhs),
                },
                span,
            });
        }
    }

    fn parse_conditional(&mut self) -> Result<Expr, ParseError> {
        let cond = self.parse_null_coalesce()?;
        if self.peek_token() == Some(&Token::Question) {
            self.advance(); // ?
            let then_branch = self.parse_expr()?;
            self.expect(Token::Colon, "expected `:` after `?` in conditional")?;
            let else_branch = self.parse_conditional()?;
            let span = Span::new(cond.span.start, else_branch.span.end);
            return Ok(Expr{kind: ExprKind::Conditional{cond: Box::new(cond), then_branch: Box::new(then_branch), else_branch: Box::new(else_branch)}, span});
        }
        Ok(cond)
    }

    fn parse_null_coalesce(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_or()?;
        while self.peek_token() == Some(&Token::QuestionQuestion) {
            self.advance();
            let rhs = self.parse_or()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr{kind: ExprKind::Binary{op: BinOp::NullCoalesce, lhs: Box::new(lhs), rhs: Box::new(rhs)}, span};
        }
        Ok(lhs)
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while self.peek_token() == Some(&Token::Or) {
            self.advance();
            let rhs = self.parse_and()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::Or,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitwise_or()?;
        while self.peek_token() == Some(&Token::And) {
            self.advance();
            let rhs = self.parse_equality()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::And,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_bitwise_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitwise_xor()?;
        while self.peek_token() == Some(&Token::Pipe) {
            // Need to distinguish `|` vs `||`? But we already handle `or` as keyword, and `|` as bitwise, and `||` is not tokenized? Actually `||` would be two `|`? But we treat `|` as single
            // Check that next is not `|` for `||`? For now, handle single `|`
            // Avoid consuming `||` which is not a token; we have `Pipe` for single `|`
            self.advance();
            let rhs = self.parse_bitwise_xor()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr{kind: ExprKind::Binary{op: BinOp::BitOr, lhs: Box::new(lhs), rhs: Box::new(rhs)}, span};
        }
        Ok(lhs)
    }

    fn parse_bitwise_xor(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitwise_and()?;
        while self.peek_token() == Some(&Token::Caret) {
            self.advance();
            let rhs = self.parse_bitwise_and()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr{kind: ExprKind::Binary{op: BinOp::BitXor, lhs: Box::new(lhs), rhs: Box::new(rhs)}, span};
        }
        Ok(lhs)
    }

    fn parse_bitwise_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_equality()?;
        while self.peek_token() == Some(&Token::Ampersand) {
            self.advance();
            let rhs = self.parse_equality()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr{kind: ExprKind::Binary{op: BinOp::BitAnd, lhs: Box::new(lhs), rhs: Box::new(rhs)}, span};
        }
        Ok(lhs)
    }

    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_relational()?;
        loop {
            let op = match self.peek_token() {
                Some(Token::Is) => {
                    // check for "is not"
                    let is_span = self.peek().unwrap().span;
                    // lookahead
                    let next_is_not = self
                        .tokens
                        .get(self.pos + 1)
                        .map(|st| st.token == Token::Not)
                        .unwrap_or(false)
                        && {
                            // need to ensure slice gap is whitespace? For now treat any adjacent Is Not as IsNot
                            true
                        };
                    if next_is_not {
                        // consume Is + Not
                        self.advance();
                        self.advance();
                        BinOp::IsNot
                    } else {
                        self.advance();
                        BinOp::Is
                    }
                }
                _ => break,
            };
            let rhs = self.parse_relational()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_range()?;
        loop {
            let op = match self.peek_token() {
                Some(Token::LShift) => BinOp::Shl,
                Some(Token::RShift) => BinOp::Shr,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_range()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr{kind: ExprKind::Binary{op, lhs: Box::new(lhs), rhs: Box::new(rhs)}, span};
        }
        Ok(lhs)
    }

    fn parse_range(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_add()?;
        loop {
            let (op, inclusive) = match self.peek_token() {
                Some(Token::DotDot) => (BinOp::Range, false),
                Some(Token::DotDotEq) => (BinOp::RangeInclusive, true),
                _ => break,
            };
            self.advance();
            // Right-hand side may be omitted for `a..` or `..b`? EBNF allows [expr] .. [expr]
            // For now, require rhs expr if present, else create range with None
            let rhs = if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon) | Some(Token::Comma) | Some(Token::RParen) | Some(Token::RBracket) | Some(Token::End) | None) {
                None
            } else {
                Some(Box::new(self.parse_add()?))
            };
            let end = rhs.as_ref().map(|e| e.span.end).unwrap_or(lhs.span.end);
            let span = Span::new(lhs.span.start, end);
            // Represent range as Binary with Range op, or as Range struct
            // Use Range variant for start/end optional
            lhs = Expr{kind: ExprKind::Range{start: Some(Box::new(lhs.clone())), end: rhs, inclusive}, span};
            // For simple .. as binary, also handle as Binary for codegen fallback
            // Break after one range to avoid chaining `a..b..c`
            break;
        }
        Ok(lhs)
    }

    fn parse_relational(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_shift()?;
        loop {
            let op = match self.peek_token() {
                Some(Token::Lt) => BinOp::Lt,
                Some(Token::LtEq) => BinOp::Le,
                Some(Token::Gt) => BinOp::Gt,
                Some(Token::GtEq) => BinOp::Ge,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_add()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    // add/sub
    fn parse_add(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek_token() {
                Some(Token::Plus) => BinOp::Add,
                Some(Token::Minus) => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek_token() {
                Some(Token::Star) => BinOp::Mul,
                Some(Token::Slash) => BinOp::Div,
                Some(Token::Percent) => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            let span = Span::new(lhs.span.start, rhs.span.end);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.peek_token() == Some(&Token::Not) {
            let start = self.advance().unwrap().span.start;
            let expr = self.parse_unary()?;
            let span = Span::new(start, expr.span.end);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(expr),
                },
                span,
            });
        }
        if self.peek_token() == Some(&Token::Minus) {
            let start = self.advance().unwrap().span.start;
            let expr = self.parse_unary()?;
            let span = Span::new(start, expr.span.end);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(expr),
                },
                span,
            });
        }
        if self.peek_token() == Some(&Token::Plus) {
            let start = self.advance().unwrap().span.start;
            let expr = self.parse_unary()?;
            let span = Span::new(start, expr.span.end);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Pos,
                    expr: Box::new(expr),
                },
                span,
            });
        }
        if self.peek_token() == Some(&Token::Tilde) {
            let start = self.advance().unwrap().span.start;
            let expr = self.parse_unary()?;
            let span = Span::new(start, expr.span.end);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::BitNot,
                    expr: Box::new(expr),
                },
                span,
            });
        }
        if self.peek_token() == Some(&Token::PlusPlus) {
            let start = self.advance().unwrap().span.start;
            let expr = self.parse_unary()?;
            let span = Span::new(start, expr.span.end);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Inc,
                    expr: Box::new(expr),
                },
                span,
            });
        }
        if self.peek_token() == Some(&Token::MinusMinus) {
            let start = self.advance().unwrap().span.start;
            let expr = self.parse_unary()?;
            let span = Span::new(start, expr.span.end);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Dec,
                    expr: Box::new(expr),
                },
                span,
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_primary()?;
        loop {
            // Handle generic call foo<T>(args) before normal call
            if let ExprKind::Ident(s) = &expr.kind {
                if self.peek_token() == Some(&Token::Lt) {
                    let save = self.pos;
                    self.advance(); // <
                    let is_type_start = matches!(self.peek_token(), Some(Token::Int) | Some(Token::Bool) | Some(Token::StringKw) | Some(Token::CharKw) | Some(Token::Float) | Some(Token::Double) | Some(Token::Any) | Some(Token::Ident));
                    if is_type_start {
                        // Try to parse type args and check for '>' then '('
                        let save2 = self.pos;
                        let mut ok = false;
                        if let Ok(_) = self.parse_type() {
                            // Check for ',' or '>' and then '('
                            let mut temp_pos = self.pos;
                            // Look ahead for '>' then '('
                            let mut depth = 0;
                            // Simple check: if next is ',' or '>' then assume generic
                            if matches!(self.peek_token(), Some(Token::Comma) | Some(Token::Gt)) {
                                // Check if after '>' we have '('
                                let save3 = self.pos;
                                // Try to consume up to '>' and then check '('
                                while !self.is_eof() && self.peek_token() != Some(&Token::Gt) {
                                    if self.peek_token() == Some(&Token::Lt) { depth += 1; }
                                    self.advance();
                                }
                                if self.peek_token() == Some(&Token::Gt) {
                                    self.advance();
                                    if self.peek_token() == Some(&Token::LParen) {
                                        ok = true
                                    }
                                }
                                self.pos = save2;
                                if ok {
                                    // It's generic call
                                    self.pos = save;
                                    self.advance(); // <
                                    let mut type_args = Vec::new();
                                    if self.peek_token() != Some(&Token::Gt) {
                                        loop {
                                            type_args.push(self.parse_type().unwrap());
                                            if !self.consume_if(Token::Comma) { break; }
                                        }
                                    }
                                    self.expect(Token::Gt, "expected `>` after type arguments").unwrap();
                                    self.expect(Token::LParen, "expected `(` after type arguments").unwrap();
                                    let args = self.parse_call_args()?;
                                    self.expect(Token::RParen, "expected `)` after call args").unwrap();
                                    let end = self.tokens[self.pos-1].span.end;
                                    let callee_name = s.clone();
                                    let callee_span = expr.span;
                                    let span2 = Span::new(callee_span.start, end);
                                    expr = Expr{kind: ExprKind::Call{callee: callee_name, callee_span, args, type_args}, span: span2};
                                    continue;
                                }
                            }
                        }
                    }
                    self.pos = save;
                }
            }
            // call: '(' [args] ')' — handles both free fn `foo()` and method `obj.meth()`
            if self.peek_token() == Some(&Token::LParen) {
                match &expr.kind {
                    ExprKind::Ident(s) => {
                        let callee_name = s.clone();
                        let callee_span = expr.span;
                        self.advance(); // (
                        let args = self.parse_call_args()?;
                        let end = self.expect(Token::RParen, "expected `)` after call args")?.span.end;
                        let span = Span::new(callee_span.start, end);
                        expr = Expr{kind: ExprKind::Call{callee: callee_name, callee_span, args, type_args: Vec::new()}, span};
                        continue;
                    }
                    ExprKind::MemberAccess{object, field, field_span} => {
                        // method call `obj.method(args)` -> MethodCall
                        let obj = object.clone();
                        let meth = field.clone();
                        let meth_span = *field_span;
                        let outer_span_start = expr.span.start;
                        self.advance(); // (
                        let args = self.parse_call_args()?;
                        let end = self.expect(Token::RParen, "expected `)` after call args")?.span.end;
                        let span = Span::new(outer_span_start, end);
                        expr = Expr{kind: ExprKind::MethodCall{object: obj, method: meth, method_span: meth_span, args}, span};
                        continue;
                    }
                    _ => break,
                }
            }
            // member access: '.' ident (EBNF §8 postfix member-access)
            if self.peek_token() == Some(&Token::Dot) {
                self.advance(); // consume .
                let (field, fspan) = self.parse_ident()?;
                let span = Span::new(expr.span.start, fspan.end);
                expr = Expr {
                    kind: ExprKind::MemberAccess {
                        object: Box::new(expr),
                        field,
                        field_span: fspan,
                    },
                    span,
                };
                continue;
            }
            // nullable member access: '?.' ident (EBNF §8)
            if self.peek_token() == Some(&Token::QuestionDot) {
                self.advance(); // consume ?.
                let (field, fspan) = self.parse_ident()?;
                let span = Span::new(expr.span.start, fspan.end);
                expr = Expr {
                    kind: ExprKind::NullableMemberAccess {
                        object: Box::new(expr),
                        field,
                        field_span: fspan,
                    },
                    span,
                };
                continue;
            }
            // postfix increment/decrement: `++` / `--` (EBNF §8)
            if self.peek_token() == Some(&Token::PlusPlus) {
                let end = self.advance().unwrap().span.end;
                let span = Span::new(expr.span.start, end);
                expr = Expr{kind: ExprKind::Postfix{op: UnaryOp::Inc, expr: Box::new(expr)}, span};
                continue;
            }
            if self.peek_token() == Some(&Token::MinusMinus) {
                let end = self.advance().unwrap().span.end;
                let span = Span::new(expr.span.start, end);
                expr = Expr{kind: ExprKind::Postfix{op: UnaryOp::Dec, expr: Box::new(expr)}, span};
                continue;
            }
            // index: '[' index-or-range ']'  EBNF §8  index-or-range = expression | range-expression | [expr] range-operator [expr]
            if self.peek_token() == Some(&Token::LBracket) {
                self.advance(); // [
                // Handle leading range `..` / `..=`  (start omitted)
                if matches!(self.peek_token(), Some(Token::DotDot) | Some(Token::DotDotEq)) {
                    let op = self.advance().unwrap();
                    let inclusive = op.token == Token::DotDotEq;
                    let end = if self.peek_token() != Some(&Token::RBracket) {
                        Some(Box::new(self.parse_expr()?))
                    } else { None };
                    let rb = self.expect(Token::RBracket, "expected `]` after index")?;
                    let span = Span::new(expr.span.start, rb.span.end);
                    expr = Expr{kind: ExprKind::Slice{object: Box::new(expr), start: None, end, inclusive}, span};
                    continue;
                }
                let start = self.parse_expr()?;
                // If start is already a Range (e.g. `1..2`, `1..`, `1..=5`), convert to Slice
                if let ExprKind::Range { start: rs, end: re, inclusive } = &start.kind {
                    let rb = self.expect(Token::RBracket, "expected `]` after index")?;
                    let span = Span::new(expr.span.start, rb.span.end);
                    expr = Expr{kind: ExprKind::Slice{object: Box::new(expr), start: rs.clone(), end: re.clone(), inclusive: *inclusive}, span};
                    continue;
                }
                // Check for trailing `..` / `..=` after single expr (e.g. `a[1..]`, `a[1..5]` where `1..5` was not parsed as Range because we parsed only `1`? But `parse_expr` already consumes `..`, so this branch only for `1` where `..` not consumed? Actually `1..` as start would have been Range already, so this handles `1` followed by `..` not yet consumed if `1` was parsed as `parse_add` only? But `parse_expr` includes Range, so `1..` would be Range, not here. This handles case where start was single and next is `..` (should not happen if Range consumed, but handle for safety)
                if matches!(self.peek_token(), Some(Token::DotDot) | Some(Token::DotDotEq)) {
                    let op = self.advance().unwrap();
                    let inclusive = op.token == Token::DotDotEq;
                    let end = if self.peek_token() != Some(&Token::RBracket) {
                        Some(Box::new(self.parse_expr()?))
                    } else { None };
                    let rb = self.expect(Token::RBracket, "expected `]` after index")?;
                    let span = Span::new(expr.span.start, rb.span.end);
                    expr = Expr{kind: ExprKind::Slice{object: Box::new(expr), start: Some(Box::new(start)), end, inclusive}, span};
                    continue;
                }
                let rb = self.expect(Token::RBracket, "expected `]` after index")?;
                let span = Span::new(expr.span.start, rb.span.end);
                expr = Expr{kind: ExprKind::Index{object: Box::new(expr), index: Box::new(start)}, span};
                continue;
            }
            break;
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        // `await task_expr` (Async-1): prefix wait on a `task<T>` handle.
        // Binds at unary level (same as `not`/`-`): parses one unary operand
        // so `await t` works but `await a + b` needs parens.
        if self.peek_token() == Some(&Token::Await) {
            let start = self.advance().unwrap().span.start;
            let task = self.parse_unary()?;
            let span = Span::new(start, task.span.end);
            return Ok(Expr {
                kind: ExprKind::Await { task: Box::new(task), span },
                span,
            });
        }
        // `spawn callee(args)` / `spawn expr` (Async-1): concurrent child.
        if self.peek_token() == Some(&Token::Spawn) {
            let start = self.advance().unwrap().span.start;
            let task = self.parse_unary()?;
            let span = Span::new(start, task.span.end);
            return Ok(Expr {
                kind: ExprKind::Spawn { task: Box::new(task), span },
                span,
            });
        }
        // closure: | [params] | => expr | block
        if self.peek_token() == Some(&Token::Pipe) {
            let start = self.peek_span().start;
            self.advance(); // |
            let mut params = Vec::new();
            if self.peek_token() != Some(&Token::Pipe) {
                loop {
                    if matches!(self.peek_token(), Some(Token::Newline) | Some(Token::Semicolon)) { self.advance(); continue; }
                    // Variadic `...` support in closures
                    let is_variadic = self.consume_if(Token::DotDotDot);
                    if is_variadic {
                        // Check derived `... b` where next Ident then delimiter
                        if self.peek_token() == Some(&Token::Ident) {
                            let next_is_delim = self.tokens.get(self.pos + 1).map(|t| matches!(t.token, Token::Comma | Token::Pipe)).unwrap_or(false);
                            if next_is_delim {
                                let (pn, pn_span) = self.parse_ident()?;
                                let ty = Type::Named("__derived__".to_string(), pn_span);
                                params.push(Param { is_variadic: true, mode: ParamMode::None, ty, name: pn, name_span: pn_span, default: None, span: pn_span });
                                if !self.consume_if(Token::Comma) { break; }
                                if self.peek_token() == Some(&Token::Pipe) { break; }
                                continue;
                            }
                        }
                        // Explicit `...T vda`
                        let save = self.pos;
                        let mut parsed_explicit = false;
                        if let Ok(ty) = self.parse_type() {
                            if let Ok((pn, pn_span)) = self.parse_ident() {
                                let pspan = Span::new(ty.span().start, pn_span.end);
                                params.push(Param { is_variadic: true, mode: ParamMode::None, ty, name: pn, name_span: pn_span, default: None, span: pspan});
                                parsed_explicit = true;
                            } else {
                                self.pos = save;
                            }
                        } else {
                            self.pos = save;
                        }
                        if !parsed_explicit {
                            let (pn, pn_span) = self.parse_ident()?;
                            let any_ty = Type::Any(pn_span);
                            params.push(Param { is_variadic: true, mode: ParamMode::None, ty: any_ty, name: pn, name_span: pn_span, default: None, span: pn_span});
                        }
                        if !self.consume_if(Token::Comma) { break; }
                        if self.peek_token() == Some(&Token::Pipe) { break; }
                        continue;
                    }
                    // Try type + ident, fallback to ident only
                    let save = self.pos;
                    let mut parsed = false;
                    if let Ok(ty) = self.parse_type() {
                        if let Ok((pn, pn_span)) = self.parse_ident() {
                            let pspan = Span::new(ty.span().start, pn_span.end);
                            params.push(Param { is_variadic: false, mode: ParamMode::None, ty, name: pn, name_span: pn_span, default: None, span: pspan});
                            parsed = true;
                        } else {
                            self.pos = save;
                        }
                    } else {
                        self.pos = save;
                    }
                    if !parsed {
                        let (pn, pn_span) = self.parse_ident()?;
                        let any_ty = Type::Any(pn_span);
                        let pspan = Span::new(pn_span.start, pn_span.end);
                        params.push(Param { is_variadic: false, mode: ParamMode::None, ty: any_ty, name: pn, name_span: pn_span, default: None, span: pspan});
                    }
                    if !self.consume_if(Token::Comma) { break; }
                    if self.peek_token() == Some(&Token::Pipe) { break; }
                }
            }
            self.expect(Token::Pipe, "expected `|` to close closure params")?;
            let body = if self.consume_if(Token::FatArrow) {
                let expr = self.parse_expr()?;
                ClosureBody::Expr(Box::new(expr))
            } else if self.peek_token() == Some(&Token::Do) {
                let blk = self.parse_block()?;
                ClosureBody::Block(blk)
            } else {
                return Err(ParseError{message: "expected `=>` or `do` after closure params".into(), span: self.peek_span()});
            };
            let end = match &body {
                ClosureBody::Expr(e) => e.span.end,
                ClosureBody::Block(b) => b.span.end,
            };
            let span = Span::new(start, end);
            return Ok(Expr{kind: ExprKind::Closure{params, body: Box::new(body), span: span.clone()}, span});
        }
        // match expression is also primary (EBNF primary-expression includes match-expression)
        if self.peek_token() == Some(&Token::Match) {
            return self.parse_match_expr();
        }
        // Attempt struct literal first: Type has ... end
        if let Some(lit) = self.try_parse_struct_literal()? {
            return Ok(lit);
        }
        let st = self.peek().cloned().ok_or(ParseError {
            message: "expected expression".into(),
            span: Span::new(self.source_end(), self.source_end()),
        })?;
        match st.token {
            Token::FloatLit => {
                self.advance();
                let raw = self.slice(st.span).replace("_", "");
                Ok(Expr {
                    kind: ExprKind::FloatLit(raw),
                    span: st.span,
                })
            }
            Token::IntLit | Token::HexInt | Token::BinInt => {
                self.advance();
                let val = self.parse_int_lit(st.span)?;
                Ok(Expr {
                    kind: ExprKind::IntLit(val),
                    span: st.span,
                })
            }
            Token::True => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::BoolLit(true),
                    span: st.span,
                })
            }
            Token::False => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::BoolLit(false),
                    span: st.span,
                })
            }
            Token::StringLit => {
                self.advance();
                let raw = self.slice(st.span);
                // raw is either "\"...\"" or "\"\"\"...\"\"\""
                let inner = if raw.starts_with("\"\"\"") && raw.ends_with("\"\"\"") && raw.len() >= 6 {
                    &raw[3..raw.len() - 3]
                } else if raw.len() >= 2 {
                    &raw[1..raw.len() - 1]
                } else {
                    ""
                };
                let quote_len = if raw.starts_with("\"\"\"") { 3 } else { 1 };
                let (decoded, positions) = self.decoded_with_positions(
                    inner, self.local_offset(st.span.start) + quote_len,
                );
                // Check for interpolation {expr} per EBNF §4
                if decoded.contains('{') && decoded.contains('}') {
                    let mut parts: Vec<InterpolatedPart> = Vec::new();
                    let mut literal = String::new();
                    let mut chars = decoded.char_indices().peekable();
                    let mut in_expr = false;
                    while let Some((offset, c)) = chars.next() {
                        if !in_expr && c == '{' {
                            if chars.peek().is_some_and(|(_, c)| *c == '{') {
                                // escaped {{
                                literal.push('{');
                                chars.next();
                                continue;
                            }
                            if !literal.is_empty() {
                                parts.push(InterpolatedPart::Literal(literal.clone()));
                                literal.clear();
                            }
                            // collect expr until matching }
                            let mut expr_str = String::new();
                            let mut depth = 1;
                            while let Some((_, c2)) = chars.next() {
                                if c2 == '{' { depth += 1; expr_str.push(c2); }
                                else if c2 == '}' {
                                    depth -= 1;
                                    if depth == 0 { break; }
                                    else { expr_str.push(c2); }
                                } else { expr_str.push(c2); }
                            }
                            let expr_trim = expr_str.trim().to_string();
                            if !expr_trim.is_empty() {
                                let start = offset + 1 + expr_str.len() - expr_str.trim_start().len();
                                let end = start + expr_trim.len();
                                let mut p = Self::interpolation_parser(expr_trim, positions[start..=end].to_vec());
                                if let Ok(expr) = p.parse_expr() {
                                    parts.push(InterpolatedPart::Expr(Box::new(expr)));
                                } else {
                                    // fallback as literal
                                    literal.push('{');
                                    literal.push_str(&expr_str);
                                    literal.push('}');
                                }
                            }
                        } else if !in_expr && c == '}' && chars.peek().is_some_and(|(_, c)| *c == '}') {
                            literal.push('}');
                            chars.next();
                        } else {
                            literal.push(c);
                        }
                    }
                    if !literal.is_empty() {
                        parts.push(InterpolatedPart::Literal(literal));
                    }
                    if parts.iter().any(|p| matches!(p, InterpolatedPart::Expr(_))) {
                        return Ok(Expr{kind: ExprKind::InterpolatedString(parts, st.span), span: st.span});
                    }
                }
                Ok(Expr {
                    kind: ExprKind::StringLit(decoded),
                    span: st.span,
                })
            }
            Token::RawStringLit => {
                self.advance();
                let raw = self.slice(st.span);
                // r"..."
                let inner = if raw.len() >= 3 {
                    &raw[2..raw.len() - 1]
                } else {
                    ""
                };
                Ok(Expr {
                    kind: ExprKind::StringLit(inner.to_string()),
                    span: st.span,
                })
            }
            Token::CharLit => {
                self.advance();
                let raw = self.slice(st.span);
                let inner = if raw.len() >= 2 {
                    &raw[1..raw.len() - 1]
                } else {
                    ""
                };
                let ch = Self::unescape_char(inner);
                Ok(Expr {
                    kind: ExprKind::CharLit(ch),
                    span: st.span,
                })
            }
            Token::This => {
                self.advance();
                Ok(Expr{kind: ExprKind::This, span: st.span})
            }
            Token::Super => {
                self.advance();
                Ok(Expr{kind: ExprKind::Super, span: st.span})
            }
            Token::SelfType => {
                self.advance();
                // Expression-position `Self` is the current instance
                // (EBNF §38 special-name), exactly like `this`.
                Ok(Expr{kind: ExprKind::This, span: st.span})
            }
            Token::Null => {
                self.advance();
                Ok(Expr{kind: ExprKind::Null, span: st.span})
            }
            Token::New => {
                // Heap construction: `new Type(args)` — constructor picked
                // by arity in sema, like a `Type(...)` call.
                self.advance();
                let ty = self.parse_type()?;
                self.expect(Token::LParen, "expected `(` after `new Type`")?;
                let args = self.parse_call_args()?;
                let rp = self.expect(Token::RParen, "expected `)` after `new` arguments")?;
                let span = Span::new(st.span.start, rp.span.end);
                Ok(Expr{kind: ExprKind::New{ty, args, span}, span})
            }
            Token::Dot => {
                // Enum variant `.Variant` or `.Variant(args)` (Phase 4) — args use argument-list (named/out/ref)
                let dot_span = st.span;
                self.advance(); // '.'
                let (vname, vspan) = self.parse_ident()?;
                let mut args = Vec::new();
                let mut end = vspan.end;
                if self.peek_token() == Some(&Token::LParen) {
                    self.advance(); // '('
                    args = self.parse_call_args()?;
                    let rp = self.expect(Token::RParen, "expected `)` after enum variant args")?;
                    end = rp.span.end;
                }
                let span = Span::new(dot_span.start, end);
                Ok(Expr{kind: ExprKind::EnumVariant{enum_name: None, variant: vname, variant_span: vspan, args}, span})
            }
            Token::Ident => {
                self.advance();
                let mut name = self.slice(st.span).to_string();
                let mut end = st.span.end;
                // qualified-expression: `a::b::c`
                while self.peek_token() == Some(&Token::ColonColon) {
                    self.advance(); // ::
                    let (seg, sspan) = self.parse_ident()?;
                    name.push_str("::");
                    name.push_str(&seg);
                    end = sspan.end;
                }
                // Check for qualified enum variant `Enum::Variant` or `a::b::Variant(args)` is handled via Ident + :: + Ident, but if next is `.Variant`? Actually `a::b::c` as qualified expr is just Ident with ::; enum variant with qualified prefix `Option::Some(1)` would be `a::b` + `.Variant`? That is handled via later MemberAccess? For now, treat `a::b` as Ident.
                // If the qualified name is followed by `.Variant` (enum), it will be handled as MemberAccess in postfix, but we can also handle `a::b::Variant(args)` as EnumVariant with prefix
                // For `a::b::c` where `c` is variant and next is `(`, we could detect but keep as Ident for now.
                let span = Span::new(st.span.start, end);
                // If next tokens are `::` already handled, but if after qualified we have `.` variant, postfix will turn it into MemberAccess/EnumVariant. For now return qualified Ident.
                // Special: if name contains `::` and next is `::` already consumed, we already have full qualified.
                // Check for `::Variant` with `::`? Already handled.
                Ok(Expr {
                    kind: ExprKind::Ident(name),
                    span,
                })
            }
            Token::LParen => {
                self.advance();
                // Check for empty tuple `()`
                if self.peek_token() == Some(&Token::RParen) {
                    let end = self.advance().unwrap().span.end;
                    let span = Span::new(st.span.start, end);
                    return Ok(Expr{kind: ExprKind::Tuple(vec![]), span});
                }
                let first = self.parse_expr()?;
                if self.consume_if(Token::Comma) {
                    // Tuple: (first, ...)
                    let mut exprs = vec![first];
                    // Handle trailing comma and remaining exprs
                    while self.peek_token() != Some(&Token::RParen) {
                        // Allow trailing comma: if next is `)` break
                        if self.peek_token() == Some(&Token::RParen) { break; }
                        let e = self.parse_expr()?;
                        exprs.push(e);
                        if !self.consume_if(Token::Comma) { break; }
                    }
                    let end = self.expect(Token::RParen, "expected `)` after tuple")?.span.end;
                    let span = Span::new(st.span.start, end);
                    return Ok(Expr{kind: ExprKind::Tuple(exprs), span});
                }
                let end = self.expect(Token::RParen, "expected `)`")?.span.end;
                let span = Span::new(st.span.start, end);
                Ok(Expr {
                    kind: ExprKind::Paren(Box::new(first)),
                    span,
                })
            }
            Token::LBracket => {
                // Array literal (types skill §8): `[1, 2, 3, 4]` (or `[]`).
                // Unambiguous in primary position: `[...]` postfix indexing
                // only occurs *after* a primary.
                self.advance(); // [
                let mut elems = Vec::new();
                if self.peek_token() == Some(&Token::RBracket) {
                    let end = self.advance().unwrap().span.end;
                    let span = Span::new(st.span.start, end);
                    return Ok(Expr { kind: ExprKind::ArrayLit(elems), span });
                }
                loop {
                    // Allow newlines inside literals
                    self.consume_newlines();
                    if self.peek_token() == Some(&Token::RBracket) {
                        break;
                    }
                    let e = self.parse_expr()?;
                    elems.push(e);
                    self.consume_newlines();
                    if !self.consume_if(Token::Comma) {
                        break;
                    }
                }
                let end = self.expect(Token::RBracket, "expected `]` after array literal")?.span.end;
                let span = Span::new(st.span.start, end);
                Ok(Expr { kind: ExprKind::ArrayLit(elems), span })
            }
            Token::Vec => {
                // Empty vector (types skill §10): `vec[]`.
                self.advance();
                self.expect(Token::LBracket, "expected `[` after `vec` for empty vector `vec[]`")?;
                let end = self.expect(Token::RBracket, "expected `]` for empty vector `vec[]`")?.span.end;
                let span = Span::new(st.span.start, end);
                Ok(Expr { kind: ExprKind::VecEmpty(span), span })
            }
            _ => Err(ParseError {
                message: format!("expected expression, found `{}`", st.token),
                span: st.span,
            }),
        }
    }

    fn parse_match_expr(&mut self) -> Result<Expr, ParseError> {
        let start = self.expect(Token::Match, "expected `match`")?.span.start;
        let scrutinee = self.parse_expr()?;
        self.expect(Token::Do, "expected `do` after `match` scrutinee")?;
        self.consume_newlines();
        let mut arms = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            if matches!(
                self.peek_token(),
                Some(Token::Newline) | Some(Token::Semicolon)
            ) {
                self.advance();
                continue;
            }
            let pat_start = self.peek_span().start;
            let pattern = self.parse_pattern()?;
            // optional guard `if expr`
            let guard = if self.peek_token() == Some(&Token::If) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(Token::Arrow, "expected `->` after match pattern")?;
            // body: block or expr
            let body = if self.peek_token() == Some(&Token::Do) {
                let blk = self.parse_block()?;
                MatchArmBody::Block(blk)
            } else {
                // expression body — consume without requiring terminator; newlines will separate arms
                let e = self.parse_expr()?;
                MatchArmBody::Expr(Box::new(e))
            };
            let arm_span = Span::new(
                pat_start,
                match &body {
                    MatchArmBody::Expr(e) => e.span.end,
                    MatchArmBody::Block(b) => b.span.end,
                },
            );
            arms.push(MatchArm {
                pattern,
                guard,
                body,
                span: arm_span,
            });
            self.consume_newlines();
        }
        let end = self
            .expect(Token::End, "expected `end` to close `match`")?
            .span
            .end;
        let span = Span::new(start, end);
        Ok(Expr {
            kind: ExprKind::Match(MatchExpr {
                scrutinee: Box::new(scrutinee),
                arms,
                span,
            }),
            span,
        })
    }

    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        // pattern-alternative: pattern-primary { ("|" | "or") pattern-primary }
        let first = self.parse_pattern_primary()?;
        let mut alts = vec![first];
        let mut span_start = alts[0].span().start;
        let mut span_end = alts[0].span().end;
        while matches!(self.peek_token(), Some(Token::Pipe) | Some(Token::Or)) {
            self.advance(); // consume `|` or `or`
            let next = self.parse_pattern_primary()?;
            span_end = next.span().end;
            alts.push(next);
        }
        if alts.len() == 1 {
            Ok(alts.into_iter().next().unwrap())
        } else {
            let span = Span::new(span_start, span_end);
            Ok(Pattern::Alternative(alts, span))
        }
    }

    fn parse_pattern_primary(&mut self) -> Result<Pattern, ParseError> {
        let st = self.peek().cloned().ok_or(ParseError {
            message: "expected pattern".into(),
            span: Span::new(self.source_end(), self.source_end()),
        })?;
        match st.token {
            Token::Dot => {
                self.advance(); // '.'
                let (vname, vspan) = self.parse_ident()?;
                let payload = if self.peek_token() == Some(&Token::LParen) {
                    self.advance(); // '('
                    let mut pats = Vec::new();
                    if self.peek_token() != Some(&Token::RParen) {
                        loop {
                            pats.push(self.parse_pattern()?);
                            if !self.consume_if(Token::Comma) { break; }
                            if self.peek_token() == Some(&Token::RParen) { break; }
                        }
                    }
                    self.expect(Token::RParen, "expected `)` after enum payload pattern")?;
                    if pats.is_empty() { None } else { Some(pats) }
                } else { None };
                Ok(Pattern::Enum{variant: vname, variant_span: vspan, payload})
            }
            Token::Ident => {
                let s = self.slice(st.span).to_string();
                if s == "_" {
                    self.advance();
                    Ok(Pattern::Wildcard(st.span))
                } else {
                    // Check for qualified enum pattern `Option.Some` or `a::b::Variant`? For now treat as Var,
                    // but if next is `.` Variant, handle as enum-pattern with qualified prefix
                    // Lookahead for qualified `::` or `.` enum pattern
                    let save = self.pos;
                    // Try to parse qualified-name "." identifier "(" pattern-list ")"
                    // We already consumed first ident as potential var, but we can check if next is `::` or `.`
                    // For simplicity, if next is `::`, treat as qualified var (for now just consume and return Var with qualified name)
                    // If next is `.` and after that is ident, it's an enum pattern with qualified prefix like `Option.Some`
                    // We'll handle that here
                    self.advance();
                    let mut name = s.clone();
                    let mut end = st.span.end;
                    // Handle `::` qualified continuation (e.g., `std::io::Var`)
                    while self.peek_token() == Some(&Token::ColonColon) {
                        self.advance(); // ::
                        let (seg, sspan) = self.parse_ident()?;
                        name.push_str("::");
                        name.push_str(&seg);
                        end = sspan.end;
                    }
                    // Check for enum pattern `.Variant` with qualified prefix
                    if self.peek_token() == Some(&Token::Dot) {
                        self.advance(); // .
                        let (vname, vspan) = self.parse_ident()?;
                        let payload = if self.peek_token() == Some(&Token::LParen) {
                            self.advance();
                            let mut pats = Vec::new();
                            if self.peek_token() != Some(&Token::RParen) {
                                loop {
                                    pats.push(self.parse_pattern()?);
                                    if !self.consume_if(Token::Comma) { break; }
                                    if self.peek_token() == Some(&Token::RParen) { break; }
                                }
                            }
                            self.expect(Token::RParen, "expected `)` after enum payload pattern")?;
                            if pats.is_empty() { None } else { Some(pats) }
                        } else { None };
                        // This is an enum pattern like `Option::Some` or `MyEnum.Variant`; treat as Enum with qualified variant
                        // For now, store variant as `name.variant`? But Pattern::Enum only has variant, not enum_name.
                        // We'll store the full qualified variant name as variant and keep original span
                        let full_variant = format!("{}::{}", name, vname);
                        // Use the variant's span for now, but keep payload
                        return Ok(Pattern::Enum{variant: full_variant, variant_span: vspan, payload});
                    }
                    // If we consumed `::` qualifiers, return Var with qualified name
                    if name != s {
                        let span = Span::new(st.span.start, end);
                        return Ok(Pattern::Var(name, span));
                    }
                    Ok(Pattern::Var(s, st.span))
                }
            }
            Token::IntLit | Token::HexInt | Token::BinInt => {
                self.advance();
                let v = self.parse_int_lit(st.span)?;
                Ok(Pattern::LitInt(v, st.span))
            }
            Token::True => {
                self.advance();
                Ok(Pattern::LitBool(true, st.span))
            }
            Token::False => {
                self.advance();
                Ok(Pattern::LitBool(false, st.span))
            }
            Token::LParen => {
                // tuple-pattern: "(" pattern { "," pattern } [","] ")"
                let start = st.span.start;
                self.advance(); // (
                // Handle empty tuple `()` as Tuple with 0 elements? But EBNF requires at least one pattern for tuple-pattern with parens? For match, `()` could be unit.
                // If next is `)`, treat as empty tuple
                if self.peek_token() == Some(&Token::RParen) {
                    let end = self.advance().unwrap().span.end;
                    return Ok(Pattern::Tuple(vec![], Span::new(start, end)));
                }
                let first = self.parse_pattern()?;
                let mut pats = vec![first];
                // Check if this is a tuple (has `,`) or just parenthesized single pattern
                let mut is_tuple = false;
                while self.peek_token() == Some(&Token::Comma) {
                    self.advance(); // ,
                    is_tuple = true;
                    if self.peek_token() == Some(&Token::RParen) {
                        break; // trailing comma
                    }
                    pats.push(self.parse_pattern()?);
                }
                let end = self.expect(Token::RParen, "expected `)` after tuple pattern")?.span.end;
                let span = Span::new(start, end);
                if is_tuple || pats.len() > 1 {
                    Ok(Pattern::Tuple(pats, span))
                } else {
                    // Single pattern in parens without comma: treat as just the inner pattern (parenthesized)
                    // But to preserve EBNF tuple-pattern with one element and no comma would be `(a)` which is not a tuple, just `a`
                    // Return the inner pattern directly
                    Ok(pats.into_iter().next().unwrap())
                }
            }
            _ => Err(ParseError {
                message: format!("expected pattern, found `{}`", st.token),
                span: st.span,
            }),
        }
    }

    fn try_parse_struct_literal(&mut self) -> Result<Option<Expr>, ParseError> {
        // Save position to backtrack if not a struct literal
        let save = self.pos;
        // Try parse Type (int/bool/void/named/...) — struct/map literals expect
        // a type followed by `has`. Cover every type start (incl. `string`,
        // sized-int idents like `i32`, `any`, tuples, `function<...>`).
        let is_type_start = matches!(
            self.peek_token(),
            Some(Token::Int)
                | Some(Token::Bool)
                | Some(Token::Void)
                | Some(Token::StringKw)
                | Some(Token::CharKw)
                | Some(Token::Float)
                | Some(Token::Double)
                | Some(Token::Any)
                | Some(Token::Function)
                | Some(Token::LParen)
                | Some(Token::Ident)
        );
        if !is_type_start {
            return Ok(None);
        }
        // Lookahead: need Type then `has`
        // We attempt to parse type, then check for Has; if not Has, revert
        let ty = match self.parse_type() {
            Ok(t) => t,
            Err(_) => {
                self.pos = save;
                return Ok(None);
            }
        };
        if self.peek_token() != Some(&Token::Has) {
            self.pos = save;
            return Ok(None);
        }
        // It is a struct literal
        let has_tok = self.advance().unwrap(); // consume has
        self.consume_newlines();
        // Map literal (types skill §13): `TYPE has k: v, ... end` when the
        // type is a `KEY:VALUE` map. Entries are `key: value` separated by
        // commas, newlines, or semicolons (both styles below are valid).
        if matches!(ty, Type::Map { .. }) {
            let mut entries = Vec::new();
            while !self.is_eof() && self.peek_token() != Some(&Token::End) {
                if matches!(
                    self.peek_token(),
                    Some(Token::Newline) | Some(Token::Semicolon) | Some(Token::Comma)
                ) {
                    self.advance();
                    continue;
                }
                let key = self.parse_expr()?;
                self.expect(Token::Colon, "expected `:` between map key and value")?;
                let value = self.parse_expr()?;
                entries.push((key, value));
                // Separator: `,` or terminator; `end` closes.
                if self.consume_if(Token::Comma) {
                    self.consume_newlines();
                    continue;
                }
                self.consume_newlines();
                if matches!(self.peek_token(), Some(Token::Semicolon)) {
                    self.advance();
                }
            }
            let end = self
                .expect(Token::End, "expected `end` to close map literal")?
                .span
                .end;
            let span = Span::new(ty.span().start, end);
            return Ok(Some(Expr {
                kind: ExprKind::MapLit { ty, entries },
                span,
            }));
        }
        let mut fields = Vec::new();
        while !self.is_eof() && self.peek_token() != Some(&Token::End) {
            if matches!(
                self.peek_token(),
                Some(Token::Newline) | Some(Token::Semicolon)
            ) {
                self.advance();
                continue;
            }
            let (fname, fspan) = self.parse_ident()?;
            self.expect(Token::Eq, "expected `=` in struct literal")?;
            let expr = self.parse_expr()?;
            self.expect_terminator("struct field initializer")?;
            fields.push((fname, fspan, expr));
            self.consume_newlines();
        }
        let end = self
            .expect(Token::End, "expected `end` to close struct literal")?
            .span
            .end;
        let span = Span::new(ty.span().start, end);
        Ok(Some(Expr {
            kind: ExprKind::StructLit { ty, fields },
            span,
        }))
    }
}

pub fn parse(
    tokens: Vec<SpannedToken>,
    source: String,
) -> Result<Program, ParseError> {
    let mut p = Parser::new(tokens, source);
    p.parse_program()
}
