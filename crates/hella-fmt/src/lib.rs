//! Hella canonical formatter (`hella fmt`).
//!
//! AST-driven pretty-printer implementing `.opencode/skills/formatter/SKILL.md`:
//! 4-space indent, 100-col structural wrapping, expanded blocks, whitespace
//! normalization, blank-line rules, variable/field alignment, class member
//! categorization, imports section, structural call/decl/collection/map
//! wrapping, comment preservation, deterministic idempotent output.

use hella_compiler::ast::*;
use hella_compiler::token::Span;

// ── Public API ─────────────────────────────────────────────────────────────

/// Formatter failure (lex or parse error).
#[derive(Debug, Clone)]
pub struct FormatError {
    pub message: String,
    pub span: Option<Span>,
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FormatError {}

/// Canonical options. The style is intentionally not user-configurable.
#[derive(Debug, Clone, Copy)]
pub struct FormatOptions {
    pub max_width: usize,
    pub indent_width: usize,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            max_width: 100,
            indent_width: 4,
        }
    }
}

/// Format Hella source text into canonical form.
pub fn format_source(source: &str) -> Result<String, FormatError> {
    format_source_with_options(source, FormatOptions::default())
}

pub fn format_source_with_options(
    source: &str,
    options: FormatOptions,
) -> Result<String, FormatError> {
    let lexed = hella_compiler::lexer::lex(source);
    if let Some(first) = lexed.errors.into_iter().next() {
        return Err(FormatError {
            message: format!("unexpected token `{}`", first.slice),
            span: Some(first.span),
        });
    }
    let program = hella_compiler::parse::parse(lexed.tokens, source.to_string()).map_err(|e| {
        FormatError {
            message: e.message,
            span: Some(e.span),
        }
    })?;
    let comments = collect_comments(source);
    let mut f = Formatter::new(source, comments, options);
    f.format_program(&program);
    Ok(f.finish())
}

/// Returns true when `source` is already canonically formatted.
pub fn is_formatted(source: &str) -> Result<bool, FormatError> {
    Ok(format_source(source)? == normalize_trailing_newline(source))
}

fn normalize_trailing_newline(s: &str) -> String {
    let mut out = s.replace("\r\n", "\n");
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

// ── Comments ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Comment {
    span: Span,
    text: String,
    /// True when nothing but whitespace precedes the comment on its line.
    own_line: bool,
    line: usize,
}

/// Scan source for `//` and `/* */` comments, respecting string/char literals.
fn collect_comments(source: &str) -> Vec<Comment> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    // line start offsets for line numbers / own-line detection
    let mut line_starts = vec![0usize];
    for (idx, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            line_starts.push(idx + 1);
        }
    }
    let line_of = |off: usize| -> usize {
        match line_starts.binary_search(&off) {
            Ok(l) => l,
            Err(l) => l.saturating_sub(1),
        }
    };
    while i < bytes.len() {
        let c = bytes[i];
        // strings
        if c == b'"' {
            if source[i..].starts_with("\"\"\"") {
                i += 3;
                while i < bytes.len() && !source[i..].starts_with("\"\"\"") {
                    if bytes[i] == b'\\' {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i += 3.min(bytes.len().saturating_sub(i));
                continue;
            }
            // raw string r"..." — the `r` was consumed as ident char; handle `r"` case
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                if bytes[i] == b'\n' {
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b'\'' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'\n' {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\'' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                let start = i;
                let mut end = i + 2;
                while end < bytes.len() && bytes[end] != b'\n' {
                    end += 1;
                }
                let text = source[start..end].to_string();
                let line = line_of(start);
                let ls = line_starts[line];
                let own_line = source[ls..start].trim().is_empty();
                out.push(Comment {
                    span: Span::new(start, end),
                    text,
                    own_line,
                    line,
                });
                i = end;
                continue;
            }
            if bytes[i + 1] == b'*' {
                let start = i;
                let mut end = i + 2;
                while end + 1 < bytes.len() && !(bytes[end] == b'*' && bytes[end + 1] == b'/') {
                    end += 1;
                }
                end = (end + 2).min(bytes.len());
                let text = source[start..end].to_string();
                let line = line_of(start);
                let ls = line_starts[line];
                let own_line = source[ls..start].trim().is_empty();
                out.push(Comment {
                    span: Span::new(start, end),
                    text,
                    own_line,
                    line,
                });
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

// ── Formatter core ─────────────────────────────────────────────────────────

struct Formatter<'a> {
    source: &'a str,
    comments: Vec<Comment>,
    comment_idx: usize,
    line_starts: Vec<usize>,
    out: String,
    indent: usize,
    opts: FormatOptions,
}

impl<'a> Formatter<'a> {
    fn new(source: &'a str, comments: Vec<Comment>, opts: FormatOptions) -> Self {
        let mut line_starts = vec![0usize];
        for (idx, b) in source.as_bytes().iter().enumerate() {
            if *b == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self {
            source,
            comments,
            comment_idx: 0,
            line_starts,
            out: String::new(),
            indent: 0,
            opts,
        }
    }

    fn finish(mut self) -> String {
        // trailing comments at EOF
        self.flush_leading(usize::MAX);
        let mut s = self.out;
        // Never emit 3+ consecutive blank lines, but preserve the canonical
        // two blank lines after the import section (`\n\n\n`).
        while s.contains("\n\n\n\n") {
            s = s.replace("\n\n\n\n", "\n\n\n");
        }
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    fn line_of(&self, off: usize) -> usize {
        let off = off.min(self.source.len());
        match self.line_starts.binary_search(&off) {
            Ok(l) => l,
            Err(l) => l.saturating_sub(1),
        }
    }

    fn pad(&self) -> String {
        " ".repeat(self.indent * self.opts.indent_width)
    }

    fn push_line(&mut self, text: &str) {
        if text.is_empty() {
            self.out.push('\n');
        } else {
            self.out.push_str(&self.pad());
            self.out.push_str(text);
            self.out.push('\n');
        }
    }

    fn push_blank(&mut self) {
        if !self.out.ends_with("\n\n") && !self.out.is_empty() {
            self.out.push('\n');
        }
    }

    /// Emit own-line comments positioned before `pos`.
    fn flush_leading(&mut self, pos: usize) {
        while self.comment_idx < self.comments.len()
            && self.comments[self.comment_idx].span.start < pos
        {
            let c = self.comments[self.comment_idx].clone();
            // trailing (inline) comments are drained by `drain_trailing`; if we
            // reach one here it means its owner was already emitted — emit own-line.
            if c.own_line {
                // block comments spanning lines: emit raw lines indented
                for (k, line) in c.text.lines().enumerate() {
                    let _ = k;
                    let t = line.trim_end();
                    if t.trim().is_empty() {
                        self.out.push('\n');
                    } else {
                        self.out.push_str(&self.pad());
                        self.out.push_str(t.trim_start());
                        self.out.push('\n');
                    }
                }
                if c.text.ends_with('\n') {
                    self.out.push('\n');
                }
            } else {
                // inline comment without an owner on this pass: put on own line
                self.out.push_str(&self.pad());
                self.out.push_str(c.text.trim());
                self.out.push('\n');
            }
            self.comment_idx += 1;
        }
    }

    /// Emit trailing `//`/`/* */` comments on the same source line as `end`.
    /// Must be called right after emitting the line content (before newline).
    fn drain_trailing(&mut self, end: usize) {
        let line = self.line_of(end);
        while self.comment_idx < self.comments.len()
            && self.comments[self.comment_idx].span.start < self.source.len()
            && self.line_of(self.comments[self.comment_idx].span.start) == line
            && !self.comments[self.comment_idx].own_line
        {
            let c = self.comments[self.comment_idx].clone();
            self.out.push_str("  ");
            self.out.push_str(c.text.trim());
            self.comment_idx += 1;
        }
    }

    /// Drain (without emitting) all comments positioned before `pos`.
    fn take_leading(&mut self, pos: usize) -> Vec<Comment> {
        let mut v = Vec::new();
        while self.comment_idx < self.comments.len()
            && self.comments[self.comment_idx].span.start < pos
        {
            v.push(self.comments[self.comment_idx].clone());
            self.comment_idx += 1;
        }
        v
    }

    /// Render drained comments at an explicit indent (member level).
    fn emit_comments_at(&self, comments: &[Comment], indent: usize) -> String {
        let pad = " ".repeat(indent * self.opts.indent_width);
        let mut s = String::new();
        for c in comments {
            // Inline (`!own_line`) comments drained as leading fall back to
            // their own line — text is preserved and output stays idempotent.
            for line in c.text.lines() {
                if line.trim().is_empty() {
                    s.push('\n');
                } else {
                    s.push_str(&pad);
                    s.push_str(line.trim_start());
                    s.push('\n');
                }
            }
        }
        s
    }

    /// Emit any queued comments positioned before `pos` (e.g. comments
    /// between the last statement and a closing `end`) into `s` at the
    /// current indent, so they never leak outside their construct.
    fn drain_until_into(&mut self, pos: usize, s: &mut String) {
        let pad = " ".repeat(self.indent * self.opts.indent_width);
        while self.comment_idx < self.comments.len()
            && self.comments[self.comment_idx].span.start < pos
        {
            let c = self.comments[self.comment_idx].clone();
            self.comment_idx += 1;
            let mut first = true;
            for line in c.text.lines() {
                let _ = first;
                first = false;
                if line.trim().is_empty() {
                    s.push('\n');
                } else {
                    s.push_str(&pad);
                    s.push_str(line.trim_start());
                    s.push('\n');
                }
            }
        }
    }

    /// True when a variable initializer used the omitted-type form
    /// (`User u = has ... end`): the source between `=` and the literal is
    /// just whitespace/comments followed by `has`. Preserving the omitted
    /// form avoids inventing an explicit type (no semantic rewrites).
    fn init_omits_type(&self, var_span: Span) -> bool {
        let end = var_span.end.min(self.source.len());
        let mut src = &self.source[var_span.start.min(end)..end];
        let eq = match src.find('=') {
            Some(i) => i + 1,
            None => return false,
        };
        src = &src[eq..];
        // skip whitespace and comments
        let bytes = src.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' | b'\n' | b'\r' => i += 1,
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i += 2;
                }
                _ => break,
            }
        }
        src[i..].starts_with("has")
            && src[i + 3..].chars().next().map(|c| !c.is_alphanumeric() && c != '_').unwrap_or(true)
    }

    /// Drain consecutive trailing (`!own_line`) comments on the same source
    /// line as `end`, combined with two-space separators.
    fn drain_same_line_trailing(&mut self, end: usize) -> Option<String> {
        let line = self.line_of(end);
        let mut parts: Vec<String> = Vec::new();
        while self.comment_idx < self.comments.len() {
            let c = &self.comments[self.comment_idx];
            if c.span.start < end || self.line_of(c.span.start) != line || c.own_line {
                break;
            }
            parts.push(c.text.trim().to_string());
            self.comment_idx += 1;
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("  "))
        }
    }

    /// True when the source has a blank line between `a_end` and `b_start`.
    fn has_blank_between(&self, a_end: usize, b_start: usize) -> bool {
        if a_end >= b_start || b_start > self.source.len() {
            return false;
        }
        let la = self.line_of(a_end.min(self.source.len().saturating_sub(1)));
        let lb = self.line_of(b_start.min(self.source.len().saturating_sub(1)));
        lb > la + 1
    }

    // ── Program / items ──

    fn format_program(&mut self, prog: &Program) {
        // Partition: imports first (stable), then the rest (stable).
        let mut imports: Vec<&Item> = Vec::new();
        let mut rest: Vec<&Item> = Vec::new();
        for item in &prog.items {
            match item {
                Item::Import(_) => imports.push(item),
                _ => rest.push(item),
            }
        }
        let mut first = true;
        for item in &imports {
            self.flush_leading(item_span(item).start);
            let s = self.fmt_item(item);
            for (i, line) in s.lines().enumerate() {
                let _ = i;
                self.push_line(line);
            }
            // trailing comment on the import line
            // (fmt_item returns lines; trailing handled via span below)
            let _ = first;
            first = false;
        }
        if !imports.is_empty() && !rest.is_empty() {
            // exactly two blank lines between final import and rest
            self.out.push('\n');
            self.out.push('\n');
        }
        for (idx, item) in rest.iter().enumerate() {
            if idx > 0 {
                self.push_blank();
            }
            self.flush_leading(item_span(item).start);
            let s = self.fmt_item(item);
            // attach trailing comment for single-line items (var/const/typedef/...)
            let trailing_line = self.line_of(item_span(item).end);
            for line in s.lines() {
                self.push_line(line);
            }
            // drain same-line trailing comments after the item
            // (only when the item ends on one line; multi-line items drain
            // their inner trailing comments during statement emission)
            let _ = trailing_line;
            self.drain_trailing(item_span(item).end);
            // if drain_trailing appended to last line, we already pushed newline —
            // fix by appending to output directly before the final newline
            // (handled: drain appends after newline, so move it up)
            self.fix_trailing_append();
        }
        // comments between items are flushed via flush_leading on next item;
        // remaining EOF comments handled in finish().
    }

    /// `drain_trailing` appends after the newline that `push_line` emitted;
    /// move `  // comment` back onto the previous line.
    fn fix_trailing_append(&mut self) {
        // output ends with "...\n  // c"; we want "...  // c\n"
        if let Some(pos) = self.out.rfind('\n') {
            let tail = self.out[pos + 1..].to_string();
            if tail.starts_with("  //") || tail.starts_with("  /*") {
                self.out.truncate(pos);
                // remove the newline, append inline, re-add newline
                self.out.push_str(&tail);
                self.out.push('\n');
            }
        }
    }

    fn fmt_item(&mut self, item: &Item) -> String {
        match item {
            Item::Import(d) => self.fmt_import(d),
            Item::Function(f) => self.fmt_function(f),
            Item::Struct(d) => self.fmt_struct(d),
            Item::Class(d) => self.fmt_class(d),
            Item::Enum(d) => self.fmt_enum(d),
            Item::Trait(d) => self.fmt_trait(d),
            Item::Typedef(d) => self.fmt_typedef(d),
            Item::Distinct(d) => self.fmt_distinct(d),
            Item::Extension(d) => self.fmt_extension(d),
            Item::Extern(d) => self.fmt_extern(d),
            Item::Init(b) => {
                let mut s = String::new();
                s.push_str("init do\n");
                s.push_str(&self.fmt_block_inner(b, 1));
                s.push_str(&format!("{}end", self.pad_for(0)));
                s
            }
            Item::Const(d) => self.fmt_const_decl(d, 0),
            Item::Var(d) => self.fmt_var_decl_noalign(d),
            Item::Attributed { attrs, item } => {
                let mut s = String::new();
                for a in attrs {
                    s.push_str(&self.fmt_attribute(a));
                    s.push('\n');
                }
                s.push_str(&self.fmt_item(item));
                s
            }
        }
    }

    fn pad_for(&self, extra: usize) -> String {
        " ".repeat((self.indent + extra) * self.opts.indent_width)
    }

    // ── Imports / attributes / typedefs ──

    fn fmt_import(&self, d: &ImportDecl) -> String {
        let mut s = format!("import {}", d.path.join("::"));
        if let Some(syms) = &d.symbols {
            let names: Vec<_> = syms.iter().map(|(n, _)| n.clone()).collect();
            s.push_str(&format!("::{{{}}}", names.join(", ")));
        }
        s
    }

    fn fmt_attribute(&self, a: &Attribute) -> String {
        if a.args.is_empty() {
            return format!("@{}", a.name);
        }
        let args: Vec<String> = a
            .args
            .iter()
            .map(|arg| match arg {
                AttributeArg::Expr(e) => self.fmt_expr_compact(e),
                AttributeArg::Named(n, _, v) => format!("{}: {}", n, self.fmt_expr_compact(v)),
            })
            .collect();
        format!("@{}({})", a.name, args.join(", "))
    }

    fn fmt_typedef(&self, d: &TypedefDecl) -> String {
        format!(
            "{}typedef {}{} = {}",
            vis_prefix(d.visibility),
            self.fmt_generic_params(&d.generic_params),
            d.name,
            self.fmt_type(&d.ty)
        )
    }

    fn fmt_distinct(&self, d: &DistinctDecl) -> String {
        format!(
            "{}distinct {}{} = {}",
            vis_prefix(d.visibility),
            d.name,
            self.fmt_generic_params(&d.generic_params),
            self.fmt_type(&d.ty)
        )
    }

    // ── Types ──

    fn fmt_type(&self, ty: &Type) -> String {
        match ty {
            Type::Int(_) => "int".into(),
            Type::Bool(_) => "bool".into(),
            Type::Void(_) => "void".into(),
            Type::String(_) => "string".into(),
            Type::Char(_) => "char".into(),
            Type::Float(_) => "float".into(),
            Type::Double(_) => "double".into(),
            Type::Any(_) => "any".into(),
            Type::Own(el, _) => format!("own {}", self.fmt_type(el)),
            Type::Named(n, _) => n.clone(),
            Type::Generic(n, args, _) => {
                format!("{}<{}>", n, args.iter().map(|a| self.fmt_type(a)).collect::<Vec<_>>().join(", "))
            }
            Type::FunctionType(ret, args, _) => {
                format!(
                    "function<{}({})>",
                    self.fmt_type(ret),
                    args.iter().map(|a| self.fmt_type(a)).collect::<Vec<_>>().join(", ")
                )
            }
            Type::Tuple(tys, _) => {
                format!("({})", tys.iter().map(|t| self.fmt_type(t)).collect::<Vec<_>>().join(", "))
            }
            Type::Array(el, _) => format!("{}[]", self.fmt_type(el)),
            Type::FixedArray { elem, size, .. } => match size {
                Some(n) => format!("{} arr[{}]", self.fmt_type(elem), n),
                None => format!("{} arr", self.fmt_type(elem)),
            },
            Type::Vec { elem, .. } => format!("{} vec", self.fmt_type(elem)),
            Type::Map { key, value, .. } => {
                format!("{}:{}", self.fmt_type(key), self.fmt_type(value))
            }
            Type::Pointer(el, _) => format!("{}*", self.fmt_type(el)),
            Type::Optional(el, _) => format!("{}?", self.fmt_type(el)),
        }
    }

    fn fmt_generic_params(&self, params: &[GenericParam]) -> String {
        if params.is_empty() {
            return String::new();
        }
        let parts: Vec<String> = params
            .iter()
            .map(|p| {
                if p.bounds.is_empty() {
                    p.name.clone()
                } else {
                    format!(
                        "{}: {}",
                        p.name,
                        p.bounds.iter().map(|b| self.fmt_type(b)).collect::<Vec<_>>().join(", ")
                    )
                }
            })
            .collect();
        format!("<{}>", parts.join(", "))
    }

    fn fmt_where_clause(&self, w: &Option<WhereClause>) -> String {
        match w {
            None => String::new(),
            Some(wc) => {
                let parts: Vec<String> = wc
                    .constraints
                    .iter()
                    .map(|c| {
                        format!(
                            "{}: {}",
                            self.fmt_type(&c.ty),
                            c.bounds.iter().map(|b| self.fmt_type(b)).collect::<Vec<_>>().join(", ")
                        )
                    })
                    .collect();
                format!(" where {}", parts.join(", "))
            }
        }
    }

    fn fmt_param(&self, p: &Param) -> String {
        let mut s = String::new();
        if p.is_variadic {
            s.push_str("...");
        }
        match p.mode {
            ParamMode::Ref => s.push_str("ref "),
            ParamMode::Out => s.push_str("out "),
            ParamMode::None => {}
        }
        // `... vda` derived form stores `__derived__` as the type placeholder.
        if let Type::Named(n, _) = &p.ty {
            if n == "__derived__" {
                s.push_str(&p.name);
                return s;
            }
        }
        s.push_str(&format!("{} {}", self.fmt_type(&p.ty), p.name));
        s
    }

    // ── Functions ──

    fn fmt_function(&mut self, f: &Function) -> String {
        let mut head = String::new();
        head.push_str(vis_prefix(f.visibility));
        if f.is_static {
            head.push_str("static ");
        }
        if f.is_sealed {
            head.push_str("sealed ");
        }
        if f.is_override {
            head.push_str("override ");
        }
        head.push_str(&format!(
            "{}{}{} {}(",
            self.fmt_type(&f.ret_ty),
            if f.name.is_empty() { "" } else { " " },
            f.name,
            ""
        ));
        // head currently "ret name(" — rebuild cleanly:
        let mut h = format!(
            "{}{}{}{}{} {}(",
            vis_prefix(f.visibility),
            if f.is_static { "static " } else { "" },
            if f.is_sealed { "sealed " } else { "" },
            if f.is_override { "override " } else { "" },
            self.fmt_type(&f.ret_ty),
            f.name,
        );
        let _ = head;
        let params: Vec<String> = f.params.iter().map(|p| self.fmt_param(p)).collect();
        let generics = self.fmt_generic_params(&f.generic_params);
        if !generics.is_empty() {
            // insert generics before paren: "name<T>("
            h = h.replacen(&format!("{}(", f.name), &format!("{}{}(", f.name, generics), 1);
        }
        let where_s = self.fmt_where_clause(&f.where_clause);
        let compact = format!("{}{}{}){} do", h, params.join(", "), "", where_s);
        // The skill's wrapped declaration example wraps generic params AND params.
        if fits(self.indent, &compact, self.opts.max_width) {
            let mut s = compact;
            s.push('\n');
            s.push_str(&self.fmt_block_inner(&f.body, 1));
            s.push_str(&format!("{}end", self.pad_for(0)));
            return s;
        }
        // structural wrap: one param per line
        let mut s = format!("{}{}", h.trim_end_matches('('), "");
        // rebuild wrapped head
        let hname = format!(
            "{}{}{}{}{} {}{}(",
            vis_prefix(f.visibility),
            if f.is_static { "static " } else { "" },
            if f.is_sealed { "sealed " } else { "" },
            if f.is_override { "override " } else { "" },
            self.fmt_type(&f.ret_ty),
            f.name,
            generics,
        );
        s = hname;
        s.push('\n');
        let ip = self.pad_for(1);
        for (i, p) in params.iter().enumerate() {
            s.push_str(&ip);
            s.push_str(p);
            if i + 1 < params.len() {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str(&format!("{}){} do\n", self.pad_for(0), where_s));
        s.push_str(&self.fmt_block_inner(&f.body, 1));
        s.push_str(&format!("{}end", self.pad_for(0)));
        s
    }

    // ── Structs / classes / enums / traits ──

    fn fmt_struct(&mut self, d: &StructDecl) -> String {
        let mut s = format!(
            "{}struct {}{}{} has\n",
            vis_prefix(Visibility::Default),
            d.name,
            self.fmt_generic_params(&d.generic_params),
            self.fmt_where_clause(&d.where_clause),
        );
        let comments = self.attach_field_comments(&d.fields);
        s.push_str(&self.fmt_fields_aligned(&d.fields, comments, 1));
        // comments between the last field and `end` stay in the struct
        let saved = self.indent;
        self.indent += 1;
        let mut tail = String::new();
        self.drain_until_into(d.span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", self.pad_for(0)));
        s
    }

    /// Drain leading/trailing comments for each field in source order.
    fn attach_field_comments(
        &mut self,
        fields: &[StructField],
    ) -> Vec<(Vec<Comment>, Option<String>)> {
        let mut out = Vec::with_capacity(fields.len());
        for f in fields {
            let leads = self.take_leading(f.span.start);
            let trail = self.drain_same_line_trailing(f.span.end);
            out.push((leads, trail));
        }
        out
    }

    /// Field alignment: visibility creates separate groups; blank lines also
    /// terminate groups. Within a group align type, name and `=` columns.
    fn fmt_fields_aligned(
        &mut self,
        fields: &[StructField],
        comments: Vec<(Vec<Comment>, Option<String>)>,
        base_indent: usize,
    ) -> String {
        if fields.is_empty() {
            return String::new();
        }
        // split into groups by visibility + source blank lines (indices)
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut cur_vis: Option<Visibility> = None;
        let mut prev_end: Option<usize> = None;
        for (idx, f) in fields.iter().enumerate() {
            let vis = f.visibility;
            let breaks = match (cur_vis, prev_end) {
                (Some(v), Some(pe)) if v == vis => self.has_blank_between(pe, f.span.start),
                (Some(v), _) if v != vis => true,
                _ => false,
            };
            if breaks && !cur.is_empty() {
                groups.push(std::mem::take(&mut cur));
            }
            cur_vis = Some(vis);
            prev_end = Some(f.span.end);
            cur.push(idx);
        }
        if !cur.is_empty() {
            groups.push(cur);
        }
        let field_pad = " ".repeat((self.indent + base_indent) * self.opts.indent_width);
        let mut s = String::new();
        for (gi, g) in groups.iter().enumerate() {
            if gi > 0 {
                s.push('\n'); // one blank line between visibility groups
            }
            // compute widths (only pad when group has 2+ members)
            let pad_group = g.len() >= 2;
            let type_strs: Vec<String> = g
                .iter()
                .map(|idx| {
                    let f = &fields[*idx];
                    format!("{}{}", vis_prefix(f.visibility), self.fmt_type(&f.ty))
                })
                .collect();
            let max_ty = type_strs.iter().map(|t| t.len()).max().unwrap_or(0);
            let max_name = g.iter().map(|idx| fields[*idx].name.len()).max().unwrap_or(0);
            for (k, idx) in g.iter().enumerate() {
                let f = &fields[*idx];
                // leading comments attached to this field
                s.push_str(&self.emit_comments_at(&comments[*idx].0, self.indent + base_indent));
                let ty = &type_strs[k];
                let init = f.default.as_ref().map(|e| self.fmt_expr_wrapped(e, base_indent));
                let line = if pad_group {
                    let ty_pad = format!("{:width$}", ty, width = max_ty);
                    let nm_pad = format!("{:width$}", f.name, width = max_name);
                    match init {
                        Some(v) => format!("{} {} = {}", ty_pad, nm_pad, v),
                        None => format!("{} {}", ty_pad, f.name),
                    }
                } else {
                    match init {
                        Some(v) => format!("{} {} = {}", ty, f.name, v),
                        None => format!("{} {}", ty, f.name),
                    }
                };
                s.push_str(&field_pad);
                s.push_str(&line);
                if let Some(c) = &comments[*idx].1 {
                    s.push_str("  ");
                    s.push_str(c);
                }
                s.push('\n');
            }
        }
        s
    }

    fn fmt_class(&mut self, d: &ClassDecl) -> String {
        let mut head = String::new();
        if d.is_open {
            head.push_str("open ");
        }
        if d.is_sealed {
            head.push_str("sealed ");
        }
        head.push_str(&format!(
            "class {}{}{}",
            d.name,
            self.fmt_generic_params(&d.generic_params),
            self.fmt_where_clause(&d.where_clause),
        ));
        if let Some(e) = &d.extends {
            head.push_str(&format!(" extends {}", self.fmt_type(e)));
        }
        if !d.implements.is_empty() {
            let im: Vec<String> = d.implements.iter().map(|t| self.fmt_type(t)).collect();
            head.push_str(&format!(" implements {}", im.join(", ")));
        }
        head.push_str(" has\n");
        let mut sections: Vec<String> = Vec::new();
        // Phase 1: attach comments in source order so the canonical
        // reordering below (fields → ctors → dtor → props → ops →
        // conversions → methods) carries each member's comments with it.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum MK {
            Field,
            Ctor,
            Dtor,
            Prop,
            Op,
            Conv,
            Method,
        }
        let mut events: Vec<(usize, MK, usize)> = Vec::new();
        for (i, m) in d.fields.iter().enumerate() {
            events.push((m.span.start, MK::Field, i));
        }
        for (i, m) in d.constructors.iter().enumerate() {
            events.push((m.span.start, MK::Ctor, i));
        }
        for (i, m) in d.destructors.iter().enumerate() {
            events.push((m.span.start, MK::Dtor, i));
        }
        for (i, m) in d.properties.iter().enumerate() {
            events.push((m.span.start, MK::Prop, i));
        }
        for (i, m) in d.operators.iter().enumerate() {
            events.push((m.span.start, MK::Op, i));
        }
        for (i, m) in d.conversions.iter().enumerate() {
            events.push((m.span.start, MK::Conv, i));
        }
        for (i, m) in d.methods.iter().enumerate() {
            events.push((m.span.start, MK::Method, i));
        }
        events.sort();
        // Per-member attached comments: leading lines + same-line trailing.
        let mut field_c: Vec<(Vec<Comment>, Option<String>)> =
            (0..d.fields.len()).map(|_| (Vec::new(), None)).collect();
        let mut other: std::collections::HashMap<(u8, usize), (Vec<Comment>, String, Option<String>)> =
            std::collections::HashMap::new();
        let tag = |k: MK| match k {
            MK::Field => 0,
            MK::Ctor => 1,
            MK::Dtor => 2,
            MK::Prop => 3,
            MK::Op => 4,
            MK::Conv => 5,
            MK::Method => 6,
        };
        for (_, kind, idx) in events {
            let (start, end) = match kind {
                MK::Field => (d.fields[idx].span.start, d.fields[idx].span.end),
                MK::Ctor => (d.constructors[idx].span.start, d.constructors[idx].span.end),
                MK::Dtor => (d.destructors[idx].span.start, d.destructors[idx].span.end),
                MK::Prop => (d.properties[idx].span.start, d.properties[idx].span.end),
                MK::Op => (d.operators[idx].span.start, d.operators[idx].span.end),
                MK::Conv => (d.conversions[idx].span.start, d.conversions[idx].span.end),
                MK::Method => (d.methods[idx].span.start, d.methods[idx].span.end),
            };
            let leads = self.take_leading(start);
            // Format the member body now (source order) so inner comment
            // drains happen in position order; store for canonical emission.
            match kind {
                MK::Field => {
                    field_c[idx].0 = leads;
                    field_c[idx].1 = self.drain_same_line_trailing(end);
                }
                _ => {
                    let body = match kind {
                        MK::Ctor => self.fmt_constructor(&d.constructors[idx], 1),
                        MK::Dtor => self.fmt_destructor(&d.destructors[idx], 1),
                        MK::Prop => self.fmt_property(&d.properties[idx], 1),
                        MK::Op => self.fmt_operator(&d.operators[idx], 1),
                        MK::Conv => self.fmt_conversion(&d.conversions[idx], 1),
                        MK::Method => self.fmt_function_indented(&d.methods[idx], 1),
                        MK::Field => unreachable!(),
                    };
                    let trail = self.drain_same_line_trailing(end);
                    other.insert((tag(kind), idx), (leads, body, trail));
                }
            }
        }
        // Phase 2: canonical emission.
        let member = |store: &std::collections::HashMap<(u8, usize), (Vec<Comment>, String, Option<String>)>,
                      this: &Formatter, kind: MK, idx: usize|
         -> String {
            let (leads, body, trail) = store.get(&(tag(kind), idx)).cloned().unwrap_or_default();
            let mut s = this.emit_comments_at(&leads, this.indent + 1);
            s.push_str(&body);
            if let Some(t) = trail {
                s.push_str("  ");
                s.push_str(&t);
            }
            s
        };
        let mut sections: Vec<String> = Vec::new();
        if !d.fields.is_empty() {
            let mut s = String::new();
            // leading comments before the first field (attached above)
            s.push_str(&self.fmt_fields_aligned(&d.fields, field_c, 1));
            sections.push(s);
        }
        for (i, _) in d.constructors.iter().enumerate() {
            sections.push(member(&other, self, MK::Ctor, i));
        }
        for (i, _) in d.destructors.iter().enumerate() {
            sections.push(member(&other, self, MK::Dtor, i));
        }
        for (i, _) in d.properties.iter().enumerate() {
            sections.push(member(&other, self, MK::Prop, i));
        }
        for (i, _) in d.operators.iter().enumerate() {
            sections.push(member(&other, self, MK::Op, i));
        }
        for (i, _) in d.conversions.iter().enumerate() {
            sections.push(member(&other, self, MK::Conv, i));
        }
        for (i, _) in d.methods.iter().enumerate() {
            sections.push(member(&other, self, MK::Method, i));
        }
        let mut s = head;
        for (i, sec) in sections.iter().enumerate() {
            if i > 0 {
                s.push('\n'); // one blank line between logical sections/members
            }
            // sections may contain multiple lines; methods each end with `end`
            // without trailing newline — normalize
            let t = sec.trim_end_matches('\n');
            for line in t.lines() {
                s.push_str(line);
                s.push('\n');
            }
        }
        // comments between the last member and `end` stay in the class
        let saved = self.indent;
        self.indent += 1;
        let mut tail = String::new();
        self.drain_until_into(d.span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", self.pad_for(0)));
        s
    }

    fn fmt_function_indented(&mut self, f: &Function, base: usize) -> String {
        let saved = self.indent;
        self.indent += base;
        let inner = self.fmt_function(f);
        self.indent = saved;
        // `fmt_function` emits absolute-indented lines except the header,
        // which carries no leading pad — pad only the first line.
        let pad = " ".repeat((saved + base) * self.opts.indent_width);
        inner
            .lines()
            .enumerate()
            .map(|(k, l)| {
                if l.is_empty() || k > 0 {
                    l.to_string()
                } else {
                    format!("{}{}", pad, l)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fmt_constructor(&mut self, c: &ConstructorDecl, base: usize) -> String {
        let pad = " ".repeat((self.indent + base) * self.opts.indent_width);
        let params: Vec<String> = c.params.iter().map(|p| self.fmt_param(p)).collect();
        let mut head = format!("{}{}{}({}) initialize", pad, vis_prefix(c.visibility), c.name, params.join(", "));
        // wrap long param lists
        if head.len() > self.opts.max_width && params.len() > 1 {
            let mut s = format!("{}{}{}(\n", pad, vis_prefix(c.visibility), c.name);
            let ip = " ".repeat((self.indent + base + 1) * self.opts.indent_width);
            for (i, p) in params.iter().enumerate() {
                s.push_str(&ip);
                s.push_str(p);
                if i + 1 < params.len() {
                    s.push(',');
                }
                s.push('\n');
            }
            s.push_str(&format!("{}) initialize", pad));
            head = s;
        }
        match &c.body {
            None => head,
            Some(b) => {
                let mut s = format!("{} do\n", head);
                let saved = self.indent;
                self.indent += base;
                s.push_str(&self.fmt_block_inner(b, 1));
                self.indent = saved;
                s.push_str(&format!("{}end", pad));
                s
            }
        }
    }

    fn fmt_destructor(&mut self, d: &DestructorDecl, base: usize) -> String {
        let pad = " ".repeat((self.indent + base) * self.opts.indent_width);
        let mut s = format!("{}{}~{}() do\n", pad, vis_prefix(d.visibility), d.name);
        let saved = self.indent;
        self.indent += base;
        s.push_str(&self.fmt_block_inner(&d.body, 1));
        self.indent = saved;
        s.push_str(&format!("{}end", pad));
        s
    }

    fn fmt_property(&mut self, p: &PropertyDecl, base: usize) -> String {
        let pad = " ".repeat((self.indent + base) * self.opts.indent_width);
        let ipad = " ".repeat((self.indent + base + 1) * self.opts.indent_width);
        let ty = p.ty.as_ref().map(|t| format!("{} ", self.fmt_type(t))).unwrap_or_default();
        let mut s = String::new();
        if let Some(g) = &p.getter {
            s.push_str(&format!("{}{}{}{} get do\n", pad, vis_prefix(p.visibility), ty, p.name));
            let saved = self.indent;
            self.indent += base;
            s.push_str(&self.fmt_block_inner(g, 1));
            self.indent = saved;
            s.push_str(&format!("{}end", pad));
        }
        if let Some((param, body)) = &p.setter {
            if !s.is_empty() {
                s.push('\n');
            }
            // setter header: `set(param)` — when getter present, short form
            let has_getter = p.getter.is_some();
            let header = if has_getter {
                format!("{}set({}) do", pad, self.fmt_param(param))
            } else {
                format!("{}{}{}{} set({}) do", pad, vis_prefix(p.visibility), ty, p.name, self.fmt_param(param))
            };
            s.push_str(&header);
            s.push('\n');
            let saved = self.indent;
            self.indent += base;
            s.push_str(&self.fmt_block_inner(body, 1));
            self.indent = saved;
            s.push_str(&format!("{}end", pad));
        }
        let _ = ipad;
        s
    }

    fn fmt_operator(&mut self, o: &OperatorDecl, base: usize) -> String {
        let pad = " ".repeat((self.indent + base) * self.opts.indent_width);
        let params: Vec<String> = o.params.iter().map(|p| self.fmt_param(p)).collect();
        let mut s = format!(
            "{}{}{}operator {}({}){} do\n",
            pad,
            vis_prefix(o.visibility),
            if o.is_static { "static " } else { "" },
            o.op,
            params.join(", "),
            self.fmt_where_clause(&o.where_clause),
        );
        let saved = self.indent;
        self.indent += base;
        s.push_str(&self.fmt_block_inner(&o.body, 1));
        self.indent = saved;
        s.push_str(&format!("{}end", pad));
        s
    }

    fn fmt_conversion(&mut self, c: &ConversionDecl, base: usize) -> String {
        let pad = " ".repeat((self.indent + base) * self.opts.indent_width);
        let mut s = format!(
            "{}{}{}convert {} to {} do\n",
            pad,
            vis_prefix(c.visibility),
            if c.is_explicit { "explicit " } else { "" },
            self.fmt_type(&c.from_ty),
            self.fmt_type(&c.to_ty),
        );
        let saved = self.indent;
        self.indent += base;
        s.push_str(&self.fmt_block_inner(&c.body, 1));
        self.indent = saved;
        s.push_str(&format!("{}end", pad));
        s
    }

    fn fmt_enum(&mut self, d: &EnumDecl) -> String {
        let mut s = format!(
            "enum {}{}{} has\n",
            d.name,
            self.fmt_generic_params(&d.generic_params),
            self.fmt_where_clause(&d.where_clause),
        );
        for v in &d.variants {
            let __leads = self.take_leading(v.span.start);
;s.push_str(&self.emit_comments_at(&__leads, self.indent + 1));
            let pad = " ".repeat((self.indent + 1) * self.opts.indent_width);
            let mut line = v.name.clone();
            if !v.payload_params.is_empty() {
                let ps: Vec<String> = v.payload_params.iter().map(|p| self.fmt_param(p)).collect();
                line.push_str(&format!("({})", ps.join(", ")));
            }
            if let Some(disc) = &v.discriminant {
                line.push_str(&format!(" = {}", self.fmt_expr_compact(disc)));
            }
            s.push_str(&pad);
            s.push_str(&line);
            if let Some(t) = self.drain_same_line_trailing(v.span.end) {
                s.push_str("  ");
                s.push_str(&t);
            }
            s.push('\n');
        }
        let saved = self.indent;
        self.indent += 1;
        let mut tail = String::new();
        self.drain_until_into(d.span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", self.pad_for(0)));
        s
    }

    fn fmt_trait(&mut self, d: &TraitDecl) -> String {
        let mut s = format!(
            "trait {}{}{} has\n",
            d.name,
            self.fmt_generic_params(&d.generic_params),
            self.fmt_where_clause(&d.where_clause),
        );
        for (i, m) in d.methods.iter().enumerate() {
            if i > 0 {
                s.push('\n');
            }
            let __leads = self.take_leading(m.span.start);
;s.push_str(&self.emit_comments_at(&__leads, self.indent + 1));
            let pad = " ".repeat((self.indent + 1) * self.opts.indent_width);
            let params: Vec<String> = m.params.iter().map(|p| self.fmt_param(p)).collect();
            s.push_str(&format!(
                "{}{}{} {}({}){}",
                pad,
                if m.is_sealed { "sealed " } else { "" },
                self.fmt_type(&m.ret_ty),
                m.name,
                params.join(", "),
                self.fmt_where_clause(&m.where_clause),
            ));
            if let Some(t) = self.drain_same_line_trailing(m.span.end) {
                s.push_str("  ");
                s.push_str(&t);
            }
            s.push('\n');
        }
        let saved = self.indent;
        self.indent += 1;
        let mut tail = String::new();
        self.drain_until_into(d.span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", self.pad_for(0)));
        s
    }

    fn fmt_extension(&mut self, d: &ExtensionDecl) -> String {
        let mut s = format!("extend {} do\n", self.fmt_type(&d.ty));
        let mut first = true;
        for m in &d.members {
            if !first {
                s.push('\n');
            }
            first = false;
            let mspan = ext_member_span(m);
            let __leads = self.take_leading(mspan.start);
;s.push_str(&self.emit_comments_at(&__leads, self.indent + 1));
            let mut body = match m {
                ExtensionMember::Field(f) => {
                    let pad = " ".repeat((self.indent + 1) * self.opts.indent_width);
                    let mut b = format!("{}{} {}", pad, self.fmt_type(&f.ty), f.name);
                    if let Some(init) = &f.default {
                        b.push_str(&format!(" = {}", self.fmt_expr_compact(init)));
                    }
                    b
                }
                ExtensionMember::Function(f) => self.fmt_function_indented(f, 1),
                ExtensionMember::Operator(o) => self.fmt_operator(o, 1),
                ExtensionMember::Property(p) => self.fmt_property(p, 1),
                ExtensionMember::Conversion(c) => self.fmt_conversion(c, 1),
            };
            if let Some(t) = self.drain_same_line_trailing(mspan.end) {
                body.push_str("  ");
                body.push_str(&t);
            }
            s.push_str(&body);
            s.push('\n');
        }
        let saved = self.indent;
        self.indent += 1;
        let mut tail = String::new();
        self.drain_until_into(d.span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", self.pad_for(0)));
        s
    }

    fn fmt_extern(&mut self, d: &ExternDecl) -> String {
        let mut s = format!("extern \"{}\" from \"{}\" do\n", d.lib, d.file);
        for m in &d.members {
            let mspan = extern_member_span(m);
            let __leads = self.take_leading(mspan.start);
;s.push_str(&self.emit_comments_at(&__leads, self.indent + 1));
            let pad = " ".repeat((self.indent + 1) * self.opts.indent_width);
            match m {
                ExternMember::Function { ty, name, params, .. } => {
                    let ps: Vec<String> = params
                        .iter()
                        .map(|p| {
                            if p.name.is_empty() {
                                // bare C varargs `...`
                                return "...".to_string();
                            }
                            if p.is_variadic {
                                if let Type::Named(n, _) = &p.ty {
                                    if n == "__derived__" {
                                        return format!("...{}", p.name);
                                    }
                                }
                                return format!("...{} {}", self.fmt_type(&p.ty), p.name);
                            }
                            format!("{} {}", self.fmt_type(&p.ty), p.name)
                        })
                        .collect();
                    s.push_str(&format!("{}{} {}({})", pad, self.fmt_type(ty), name, ps.join(", ")));
                    if let Some(t) = self.drain_same_line_trailing(mspan.end) {
                        s.push_str("  ");
                        s.push_str(&t);
                    }
                    s.push('\n');
                }
                ExternMember::Struct { name, fields, .. } => {
                    s.push_str(&format!("{}struct {}", pad, name));
                    if fields.is_empty() {
                        s.push('\n');
                    } else {
                        s.push_str(" has\n");
                        for f in fields {
                            let ip = " ".repeat((self.indent + 2) * self.opts.indent_width);
                            s.push_str(&format!("{}{} {}\n", ip, self.fmt_type(&f.ty), f.name));
                        }
                        s.push_str(&format!("{}end\n", pad));
                    }
                }
                ExternMember::Enum { name, variants, .. } => {
                    s.push_str(&format!("{}enum {} has\n", pad, name));
                    for v in variants {
                        let ip = " ".repeat((self.indent + 2) * self.opts.indent_width);
                        s.push_str(&format!("{}{}\n", ip, v.name));
                    }
                    s.push_str(&format!("{}end\n", pad));
                }
                ExternMember::Const { ty, name, .. } => {
                    s.push_str(&format!("{}const {} {}", pad, self.fmt_type(ty), name));
                    if let Some(t) = self.drain_same_line_trailing(mspan.end) {
                        s.push_str("  ");
                        s.push_str(&t);
                    }
                    s.push('\n');
                }
            }
        }
        let saved = self.indent;
        self.indent += 1;
        let mut tail = String::new();
        self.drain_until_into(d.span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", self.pad_for(0)));
        s
    }

    // ── var / const ──

    fn fmt_var_decl_noalign(&mut self, d: &VarDecl) -> String {
        let mut s = format!("{}{} {}", vis_prefix(d.visibility), self.fmt_type(&d.ty), d.name);
        if let Some(init) = &d.init {
            s.push_str(&format!(" = {}", self.fmt_init_expr(init, d.span)));
        }
        s
    }

    /// Variable initializer rendering that preserves the omitted-type form
    /// (`User u = has ... end`) instead of inventing an explicit type.
    fn fmt_init_expr(&mut self, e: &Expr, var_span: Span) -> String {
        match &e.kind {
            ExprKind::StructLit { ty, fields } if self.init_omits_type(var_span) => {
                self.fmt_struct_lit_wrapped(ty, fields, e.span, true)
            }
            ExprKind::MapLit { ty, entries } if self.init_omits_type(var_span) => {
                self.fmt_map_wrapped(ty, entries, e.span, true)
            }
            _ => self.fmt_expr_wrapped(e, 0),
        }
    }

    fn fmt_const_decl(&mut self, d: &ConstDecl, _base: usize) -> String {
        let mut s = format!("{}const ", vis_prefix(d.visibility));
        if let Some(t) = &d.ty {
            s.push_str(&format!("{} ", self.fmt_type(t)));
        }
        s.push_str(&format!("{} = {}", d.name, self.fmt_expr_wrapped(&d.init, 0)));
        s
    }

    // ── Blocks & statements ──

    /// Inner statements of a `do...end` / `has...end` body at relative indent.
    fn fmt_block_inner(&mut self, b: &Block, rel: usize) -> String {
        let saved = self.indent;
        self.indent += rel;
        let mut s = String::new();
        self.fmt_stmts(&b.stmts, &mut s);
        // comments between the last statement and `end` stay in the block
        self.drain_until_into(b.span.end, &mut s);
        self.indent = saved;
        s
    }

    fn fmt_stmts(&mut self, stmts: &[Stmt], s: &mut String) {
        // group consecutive var/const decls for alignment
        let mut i = 0;
        let mut prev_end: Option<usize> = None;
        while i < stmts.len() {
            let is_varlike = matches!(stmts[i], Stmt::VarDecl(_) | Stmt::Const(_));
            if is_varlike {
                // collect group (blank line in source terminates)
                let mut group: Vec<usize> = vec![i];
                let mut j = i + 1;
                while j < stmts.len()
                    && matches!(stmts[j], Stmt::VarDecl(_) | Stmt::Const(_))
                    && !self.has_blank_between(stmt_span(&stmts[j - 1]).end, stmt_span(&stmts[j]).start)
                {
                    group.push(j);
                    j += 1;
                }
                // blank before group (source had blank and not at block start)
                if prev_end.is_some()
                    && self.has_blank_between(prev_end.unwrap(), stmt_span(&stmts[i]).start)
                    && !s.is_empty()
                {
                    s.push('\n');
                }
                s.push_str(&self.fmt_var_group(stmts, &group));
                prev_end = Some(stmt_span(&stmts[group[group.len() - 1]]).end);
                i = j;
                continue;
            }
            let st = &stmts[i];
            let sp = stmt_span(st);
            if prev_end.is_some()
                && self.has_blank_between(prev_end.unwrap(), sp.start)
                && !s.is_empty()
            {
                s.push('\n');
            }
            self.flush_leading_to(st, s);
            let line = self.fmt_stmt(st);
            // `fmt_stmt` returns fully-indented text for multi-line
            // statements (inner lines already carry their indent); only the
            // first line needs the block indent applied here.
            let pad = " ".repeat(self.indent * self.opts.indent_width);
            for (k, l) in line.lines().enumerate() {
                if l.is_empty() {
                    s.push('\n');
                } else if k == 0 {
                    s.push_str(&pad);
                    s.push_str(l);
                    s.push('\n');
                } else {
                    s.push_str(l);
                    s.push('\n');
                }
            }
            // trailing comment fixup: drain_trailing appends to s directly
            self.drain_trailing_to(sp.end, s);
            prev_end = Some(sp.end);
            i += 1;
        }
        // strip leading decorative blanks (spec) — fmt_stmts never emits them
        while s.starts_with('\n') {
            s.remove(0);
        }
        // strip trailing blank lines inside block (no decorative blanks before end)
        while s.ends_with("\n\n") {
            s.pop();
        }
    }

    fn flush_leading_to(&mut self, st: &Stmt, s: &mut String) {
        let pos = stmt_span(st).start;
        while self.comment_idx < self.comments.len()
            && self.comments[self.comment_idx].span.start < pos
        {
            let c = self.comments[self.comment_idx].clone();
            self.comment_idx += 1;
            if c.own_line {
                for line in c.text.lines() {
                    if line.trim().is_empty() {
                        s.push('\n');
                    } else {
                        s.push_str(&" ".repeat(self.indent * self.opts.indent_width));
                        s.push_str(line.trim_start());
                        s.push('\n');
                    }
                }
            } else {
                s.push_str(&" ".repeat(self.indent * self.opts.indent_width));
                s.push_str(c.text.trim());
                s.push('\n');
            }
        }
    }

    fn drain_trailing_to(&mut self, end: usize, s: &mut String) {
        let line = self.line_of(end);
        while self.comment_idx < self.comments.len()
            && self.line_of(self.comments[self.comment_idx].span.start) == line
            && !self.comments[self.comment_idx].own_line
            && self.comments[self.comment_idx].span.start >= end.saturating_sub(1)
            && self.comments[self.comment_idx].span.start < end + 200
        {
            // only drain when the comment is on the same line and after `end`
            if self.comments[self.comment_idx].span.start < end {
                break;
            }
            let c = self.comments[self.comment_idx].clone();
            self.comment_idx += 1;
            // append inline to previous line
            if s.ends_with('\n') {
                s.pop();
                s.push_str("  ");
                s.push_str(c.text.trim());
                s.push('\n');
            } else {
                s.push_str("  ");
                s.push_str(c.text.trim());
            }
        }
    }

    fn fmt_var_group(&mut self, stmts: &[Stmt], group: &[usize]) -> String {
        struct Row {
            ty: String,
            name: String,
            init: Option<String>,
            leads: Vec<Comment>,
            trail: Option<String>,
        }
        let mut rows: Vec<Row> = Vec::new();
        for &idx in group {
            match &stmts[idx] {
                Stmt::VarDecl(d) => {
                    let leads = self.take_leading(d.span.start);
                    let init = d.init.as_ref().map(|e| self.fmt_init_expr(e, d.span));
                    let trail = self.drain_same_line_trailing(d.span.end);
                    rows.push(Row {
                        ty: format!("{}{}", vis_prefix(d.visibility), self.fmt_type(&d.ty)),
                        name: d.name.clone(),
                        init,
                        leads,
                        trail,
                    });
                }
                Stmt::Const(d) => {
                    let leads = self.take_leading(d.span.start);
                    let init = Some(self.fmt_expr_wrapped(&d.init, 0));
                    let trail = self.drain_same_line_trailing(d.span.end);
                    rows.push(Row {
                        ty: format!(
                            "{}const {}",
                            vis_prefix(d.visibility),
                            d.ty.as_ref().map(|t| format!("{} ", self.fmt_type(t))).unwrap_or_default()
                        )
                        .trim_end()
                        .to_string(),
                        name: d.name.clone(),
                        init,
                        leads,
                        trail,
                    });
                }
                _ => {}
            }
        }
        let pad0 = " ".repeat(self.indent * self.opts.indent_width);
        let emit_row = |this: &Formatter, s: &mut String, r: &Row, ty_s: &str, nm_s: &str| {
            s.push_str(&this.emit_comments_at(&r.leads, this.indent));
            s.push_str(&pad0);
            match &r.init {
                Some(v) => {
                    // multiline initializers (struct/map literals): first
                    // line joins the declaration, the rest is already
                    // indented by the wrapped renderer.
                    let mut lines = v.lines();
                    let first = lines.next().unwrap_or("");
                    s.push_str(&format!("{} {} = {}", ty_s, nm_s, first));
                    s.push('\n');
                    for l in lines {
                        s.push_str(l);
                        s.push('\n');
                    }
                }
                None => {
                    s.push_str(&format!("{} {}", ty_s, r.name));
                    s.push('\n');
                }
            }
            if let Some(c) = &r.trail {
                s.pop(); // trailing comment joins the last emitted line
                s.push_str("  ");
                s.push_str(c);
                s.push('\n');
            }
        };
        let mut s = String::new();
        if rows.len() < 2 {
            let r = &rows[0];
            let (ty_s, nm_s) = (r.ty.clone(), r.name.clone());
            emit_row(self, &mut s, r, &ty_s, &nm_s);
            return s;
        }
        let max_ty = rows.iter().map(|r| r.ty.len()).max().unwrap_or(0);
        let max_name = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        // borrowck: compute padded strings first, then emit immutably
        let cells: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                (
                    format!("{:width$}", r.ty, width = max_ty),
                    format!("{:width$}", r.name, width = max_name),
                )
            })
            .collect();
        for (r, (ty_s, nm_s)) in rows.iter().zip(cells.iter()) {
            emit_row(self, &mut s, r, ty_s, nm_s);
        }
        s
    }

    fn fmt_stmt(&mut self, st: &Stmt) -> String {
        match st {
            Stmt::VarDecl(d) => self.fmt_var_decl_noalign(d),
            Stmt::Const(d) => self.fmt_const_decl(d, 0),
            Stmt::Destructure(d) => {
                let ts: Vec<String> = d
                    .targets
                    .iter()
                    .map(|t| match t {
                        DestructureTarget::Ident(n, _) => n.clone(),
                        DestructureTarget::Wildcard(_) => "_".into(),
                    })
                    .collect();
                format!("{} = {}", ts.join(", "), self.fmt_expr_wrapped(&d.expr, 0))
            }
            Stmt::Assert(a) => {
                let kw = if a.is_debug { "debug_assert" } else { "assert" };
                match &a.message {
                    Some(m) => format!("{} {}, {}", kw, self.fmt_expr_compact(&a.cond), self.fmt_expr_compact(m)),
                    None => format!("{} {}", kw, self.fmt_expr_compact(&a.cond)),
                }
            }
            Stmt::If(v) => self.fmt_if(v),
            Stmt::While(v) => {
                format!("while {} do\n{}end", self.fmt_expr_compact(&v.cond), self.fmt_block_contents(&v.body))
            }
            Stmt::Loop(v) => {
                let label = v.label.as_ref().map(|l| format!("{}: ", l)).unwrap_or_default();
                format!("{}loop do\n{}end", label, self.fmt_block_contents(&v.body))
            }
            Stmt::For(v) => {
                let label = v.label.as_ref().map(|l| format!("{}: ", l)).unwrap_or_default();
                let vars = match &v.var2 {
                    Some((v2, _)) => format!("{}, {}", v.var, v2),
                    None => v.var.clone(),
                };
                format!(
                    "{}for {} in {} do\n{}end",
                    label,
                    vars,
                    self.fmt_expr_compact(&v.iter),
                    self.fmt_block_contents(&v.body)
                )
            }
            Stmt::Return(v) => match &v.value {
                Some(e) => format!("return {}", self.fmt_expr_wrapped(e, 0)),
                None => "return".into(),
            },
            Stmt::Expr(e) => self.fmt_expr_wrapped(&e.expr, 0),
            Stmt::Block(b) => format!("do\n{}end", self.fmt_block_contents(b)),
            Stmt::Break(v) => match &v.label {
                Some(l) => format!("break {}", l),
                None => "break".into(),
            },
            Stmt::Continue(v) => match &v.label {
                Some(l) => format!("continue {}", l),
                None => "continue".into(),
            },
            Stmt::Defer(v) => match &v.inner {
                DeferInner::Expr(e) => format!("defer {}", self.fmt_expr_compact(e)),
                DeferInner::Block(b) => format!("defer do\n{}end", self.fmt_block_contents(b)),
            },
            Stmt::Delete(d) => format!("delete {}", self.fmt_expr_wrapped(&d.target, 0)),
        }
    }

    /// Block contents with indent already applied (for single-line contexts).
    fn fmt_block_contents(&mut self, b: &Block) -> String {
        let saved = self.indent;
        self.indent += 1;
        let mut s = String::new();
        self.fmt_stmts(&b.stmts, &mut s);
        // comments between the last statement and `end` stay in the block
        self.drain_until_into(b.span.end, &mut s);
        // re-indent: fmt_stmts used self.indent; lines already padded.
        // But caller emitted header at outer indent — inner lines are one deeper. Good.
        // Dedent the closing `end`: caller appends `end` at outer indent.
        // We need to strip one level? No: s lines already at saved+1. Keep.
        self.indent = saved;
        // adjust: s was built with indent saved+1 — correct.
        let mut out = String::new();
        // s lines are padded with (saved+1)*width — but our pad used self.indent
        // which we bumped, so fine. Just append and add outer pad for `end`.
        out.push_str(&s);
        out.push_str(&" ".repeat(saved * self.opts.indent_width));
        out
    }

    fn fmt_if(&mut self, v: &IfStmt) -> String {
        let outer = self.indent;
        let mut s = format!("if {} do\n", self.fmt_expr_compact(&v.cond));
        s.push_str(&self.fmt_block_inner(&v.then_block, 1));
        s.push_str(&format!("{}end", " ".repeat(outer * self.opts.indent_width)));
        if let Some(else_b) = &v.else_block {
            s.push_str(&format!("\n{}else do\n", " ".repeat(outer * self.opts.indent_width)));
            s.push_str(&self.fmt_block_inner(else_b, 1));
            s.push_str(&format!("{}end", " ".repeat(outer * self.opts.indent_width)));
        }
        s
    }

    // ── Expressions ──

    /// Compact single-line rendering with canonical spacing.
    fn fmt_expr_compact(&self, e: &Expr) -> String {
        match &e.kind {
            ExprKind::IntLit(v) => v.to_string(),
            ExprKind::FloatLit(v) => v.clone(),
            ExprKind::BoolLit(v) => v.to_string(),
            ExprKind::StringLit(v) => format!("\"{}\"", escape_string(v)),
            ExprKind::InterpolatedString(parts, _) => {
                let mut s = String::from("\"");
                for p in parts {
                    match p {
                        InterpolatedPart::Literal(l) => s.push_str(l),
                        InterpolatedPart::Expr(inner) => {
                            s.push('{');
                            s.push_str(&self.fmt_expr_compact(inner));
                            s.push('}');
                        }
                    }
                }
                s.push('"');
                s
            }
            ExprKind::CharLit(c) => format!("'{}'", escape_char(*c)),
            ExprKind::Ident(n) => n.clone(),
            ExprKind::This => "this".into(),
            ExprKind::Super => "super".into(),
            ExprKind::Null => "null".into(),
            ExprKind::Paren(inner) => format!("({})", self.fmt_expr_compact(inner)),
            ExprKind::Tuple(es) => {
                format!("({})", es.iter().map(|x| self.fmt_expr_compact(x)).collect::<Vec<_>>().join(", "))
            }
            ExprKind::Unary { op, expr } => {
                format!("{}{}", unary_prefix(*op), self.fmt_expr_compact(expr))
            }
            ExprKind::Postfix { op, expr } => {
                format!("{}{}", self.fmt_expr_compact(expr), unary_postfix(*op))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                format!(
                    "{} {} {}",
                    self.fmt_expr_compact(lhs),
                    bin_op(*op),
                    self.fmt_expr_compact(rhs)
                )
            }
            ExprKind::Assign { lhs, value } => {
                format!("{} = {}", self.fmt_expr_compact(lhs), self.fmt_expr_compact(value))
            }
            ExprKind::CompoundAssign { op, lhs, value } => {
                format!(
                    "{} {}= {}",
                    self.fmt_expr_compact(lhs),
                    bin_op(*op),
                    self.fmt_expr_compact(value)
                )
            }
            ExprKind::Conditional { cond, then_branch, else_branch } => {
                format!(
                    "{} ? {} : {}",
                    self.fmt_expr_compact(cond),
                    self.fmt_expr_compact(then_branch),
                    self.fmt_expr_compact(else_branch)
                )
            }
            ExprKind::Range { start, end, inclusive } => {
                let op = if *inclusive { "..=" } else { ".." };
                format!(
                    "{}{}{}",
                    start.as_ref().map(|x| self.fmt_expr_compact(x)).unwrap_or_default(),
                    op,
                    end.as_ref().map(|x| self.fmt_expr_compact(x)).unwrap_or_default()
                )
            }
            ExprKind::NullableMemberAccess { object, field, .. } => {
                format!("{}?.{}", self.fmt_expr_compact(object), field)
            }
            ExprKind::Call { callee, args, type_args, .. } => {
                let ta = if type_args.is_empty() {
                    String::new()
                } else {
                    format!(
                        "<{}>",
                        type_args.iter().map(|t| self.fmt_type(t)).collect::<Vec<_>>().join(", ")
                    )
                };
                let ac: Vec<String> = args.iter().map(|a| self.fmt_call_arg(a)).collect();
                format!("{}{}({})", callee, ta, ac.join(", "))
            }
            ExprKind::MethodCall { object, method, args, .. } => {
                let ac: Vec<String> = args.iter().map(|a| self.fmt_call_arg(a)).collect();
                format!("{}.{}({})", self.fmt_expr_compact(object), method, ac.join(", "))
            }
            ExprKind::New { ty, args, .. } => {
                let ac: Vec<String> = args.iter().map(|a| self.fmt_call_arg(a)).collect();
                format!("new {}({})", self.fmt_type(ty), ac.join(", "))
            }
            ExprKind::MemberAccess { object, field, .. } => {
                format!("{}.{}", self.fmt_expr_compact(object), field)
            }
            ExprKind::Index { object, index } => {
                format!("{}[{}]", self.fmt_expr_compact(object), self.fmt_expr_compact(index))
            }
            ExprKind::ArrayLit(es) => {
                format!("[{}]", es.iter().map(|x| self.fmt_expr_compact(x)).collect::<Vec<_>>().join(", "))
            }
            ExprKind::VecEmpty(_) => "vec[]".into(),
            ExprKind::MapLit { .. } => self.fmt_expr_compact_multiline_hint(e),
            ExprKind::Slice { object, start, end, inclusive } => {
                let op = if *inclusive { "..=" } else { ".." };
                format!(
                    "{}[{}{}{}]",
                    self.fmt_expr_compact(object),
                    start.as_ref().map(|x| self.fmt_expr_compact(x)).unwrap_or_default(),
                    op,
                    end.as_ref().map(|x| self.fmt_expr_compact(x)).unwrap_or_default()
                )
            }
            ExprKind::StructLit { .. } => self.fmt_expr_compact_multiline_hint(e),
            ExprKind::EnumVariant { enum_name, variant, args, .. } => {
                let ac: Vec<String> = args.iter().map(|a| self.fmt_call_arg(a)).collect();
                let base = match enum_name {
                    Some(n) => format!("{}.{}", n, variant),
                    None => format!(".{}", variant),
                };
                if args.is_empty() {
                    base
                } else {
                    format!("{}({})", base, ac.join(", "))
                }
            }
            ExprKind::Match(m) => self.fmt_match_compact(m),
            ExprKind::Closure { params, body, .. } => {
                let ps: Vec<String> = params.iter().map(|p| self.fmt_param(p)).collect();
                match body.as_ref() {
                    ClosureBody::Expr(e) => format!("|{}| => {}", ps.join(", "), self.fmt_expr_compact(e)),
                    ClosureBody::Block(_) => format!("|{}| do ... end", ps.join(", ")),
                }
            }
        }
    }

    /// Placeholder single-line for block-like literals (expanded by wrapped path).
    fn fmt_expr_compact_multiline_hint(&self, e: &Expr) -> String {
        match &e.kind {
            ExprKind::MapLit { ty, entries } => {
                let es: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", self.fmt_expr_compact(k), self.fmt_expr_compact(v)))
                    .collect();
                format!("{} has {} end", self.fmt_type(ty), es.join(", "))
            }
            ExprKind::StructLit { ty, fields } => {
                let es: Vec<String> = fields
                    .iter()
                    .map(|(n, _, v)| format!("{} = {}", n, self.fmt_expr_compact(v)))
                    .collect();
                // `;` separators keep the single-line hint parseable
                // (field initializers require statement terminators).
                format!("{} has {} end", self.fmt_type(ty), es.join("; "))
            }
            _ => self.fmt_expr_compact_fallback(e),
        }
    }

    fn fmt_expr_compact_fallback(&self, _e: &Expr) -> String {
        String::new()
    }

    fn fmt_call_arg(&self, a: &CallArg) -> String {
        match a {
            CallArg::Expr(e) => self.fmt_expr_compact(e),
            CallArg::Named { name, value, .. } => format!("{}: {}", name, self.fmt_expr_compact(value)),
            CallArg::Out { ty, name, .. } => match ty {
                Some(t) => format!("out {} {}", self.fmt_type(t), name),
                None => format!("out {}", name),
            },
            CallArg::Ref { expr, .. } => format!("ref {}", self.fmt_expr_compact(expr)),
        }
    }

    fn fmt_match_compact(&self, m: &MatchExpr) -> String {
        let arms: Vec<String> = m.arms.iter().map(|a| self.fmt_match_arm_compact(a)).collect();
        format!("match {} do {} end", self.fmt_expr_compact(&m.scrutinee), arms.join(", "))
    }

    fn fmt_match_arm_compact(&self, a: &MatchArm) -> String {
        let mut s = self.fmt_pattern(&a.pattern);
        if let Some(g) = &a.guard {
            s.push_str(&format!(" if {}", self.fmt_expr_compact(g)));
        }
        s.push_str(" -> ");
        match &a.body {
            MatchArmBody::Expr(e) => s.push_str(&self.fmt_expr_compact(e)),
            MatchArmBody::Block(_) => s.push_str("do ... end"),
        }
        s
    }

    fn fmt_pattern(&self, p: &Pattern) -> String {
        match p {
            Pattern::Wildcard(_) => "_".into(),
            Pattern::Var(n, _) => n.clone(),
            Pattern::LitInt(v, _) => v.to_string(),
            Pattern::LitBool(v, _) => v.to_string(),
            Pattern::Enum { variant, payload, .. } => {
                if let Some(pl) = payload {
                    format!(
                        ".{}({})",
                        variant,
                        pl.iter().map(|x| self.fmt_pattern(x)).collect::<Vec<_>>().join(", ")
                    )
                } else {
                    format!(".{}", variant)
                }
            }
            Pattern::Tuple(ps, _) => {
                format!("({})", ps.iter().map(|x| self.fmt_pattern(x)).collect::<Vec<_>>().join(", "))
            }
            Pattern::Alternative(ps, _) => {
                ps.iter().map(|x| self.fmt_pattern(x)).collect::<Vec<_>>().join(" | ")
            }
        }
    }

    /// Width-aware expression rendering: compact when it fits, structural
    /// multiline for calls / arrays / maps / struct literals / match.
    fn fmt_expr_wrapped(&mut self, e: &Expr, extra: usize) -> String {
        let compact = self.fmt_expr_compact(e);
        let base_width = (self.indent + extra) * self.opts.indent_width;
        if base_width + compact.len() <= self.opts.max_width {
            // block-like literals are always expanded even when compact fits
            match &e.kind {
                ExprKind::MapLit { .. } | ExprKind::StructLit { .. } => {}
                ExprKind::Match(_) => {}
                _ => return compact,
            }
        }
        match &e.kind {
            ExprKind::Call { callee, args, type_args, .. } => {
                self.fmt_call_wrapped(callee, type_args, args, None, base_width)
            }
            ExprKind::MethodCall { object, method, args, .. } => {
                let recv = self.fmt_expr_compact(object);
                self.fmt_call_wrapped(method, &[], args, Some(&recv), base_width)
            }
            ExprKind::ArrayLit(es) => self.fmt_array_wrapped(es, base_width),
            ExprKind::MapLit { ty, entries } => self.fmt_map_wrapped(ty, entries, e.span, false),
            ExprKind::StructLit { ty, fields } => self.fmt_struct_lit_wrapped(ty, fields, e.span, false),
            ExprKind::Match(m) => self.fmt_match_wrapped(m),
            ExprKind::Closure { params, body, .. } => match body.as_ref() {
                ClosureBody::Block(b) => {
                    let ps: Vec<String> = params.iter().map(|p| self.fmt_param(p)).collect();
                    let mut s = format!("|{}| do\n", ps.join(", "));
                    s.push_str(&self.fmt_block_inner(b, 1));
                    s.push_str(&format!("{}end", " ".repeat(self.indent * self.opts.indent_width)));
                    s
                }
                _ => compact,
            },
            _ => {
                // for non-wrappable long expressions keep compact (no arbitrary wrap)
                compact
            }
        }
    }

    fn fmt_call_wrapped(
        &mut self,
        name: &str,
        type_args: &[Type],
        args: &[CallArg],
        receiver: Option<&str>,
        _base_width: usize,
    ) -> String {
        if args.is_empty() {
            let ta = if type_args.is_empty() {
                String::new()
            } else {
                format!("<{}>", type_args.iter().map(|t| self.fmt_type(t)).collect::<Vec<_>>().join(", "))
            };
            return match receiver {
                Some(r) => format!("{}.{}{}()", r, name, ta),
                None => format!("{}{}()", name, ta),
            };
        }
        // try compact first (caller already determined it doesn't fit, but
        // re-check with single-line args)
        let ta = if type_args.is_empty() {
            String::new()
        } else {
            format!("<{}>", type_args.iter().map(|t| self.fmt_type(t)).collect::<Vec<_>>().join(", "))
        };
        let head = match receiver {
            Some(r) => format!("{}.{}{}(", r, name, ta),
            None => format!("{}{}(", name, ta),
        };
        let mut s = head;
        s.push('\n');
        let ip = " ".repeat((self.indent + 1) * self.opts.indent_width);
        for (i, a) in args.iter().enumerate() {
            s.push_str(&ip);
            // args that are themselves block-like get wrapped rendering
            let txt = match a {
                CallArg::Expr(e) => self.fmt_expr_wrapped(e, 1),
                _ => self.fmt_call_arg(a),
            };
            s.push_str(&txt);
            if i + 1 < args.len() {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str(&format!("{})", " ".repeat(self.indent * self.opts.indent_width)));
        s
    }

    fn fmt_array_wrapped(&mut self, es: &[Expr], _base: usize) -> String {
        if es.is_empty() {
            return "[]".into();
        }
        let compact = format!(
            "[{}]",
            es.iter().map(|x| self.fmt_expr_compact(x)).collect::<Vec<_>>().join(", ")
        );
        if (self.indent) * self.opts.indent_width + compact.len() <= self.opts.max_width {
            return compact;
        }
        let mut s = String::from("[\n");
        let ip = " ".repeat((self.indent + 1) * self.opts.indent_width);
        for e in es {
            s.push_str(&ip);
            s.push_str(&self.fmt_expr_compact(e));
            s.push_str(",\n");
        }
        s.push_str(&format!("{}]", " ".repeat(self.indent * self.opts.indent_width)));
        s
    }

    fn fmt_map_wrapped(
        &mut self,
        ty: &Type,
        entries: &[(Expr, Expr)],
        lit_span: Span,
        omitted: bool,
    ) -> String {
        let pad = " ".repeat(self.indent * self.opts.indent_width);
        let ipad = " ".repeat((self.indent + 1) * self.opts.indent_width);
        let prefix = if omitted {
            String::new()
        } else {
            format!("{} ", self.fmt_type(ty))
        };
        let mut s = format!("{}has\n", prefix);
        for (k, v) in entries {
            let __leads = self.take_leading(k.span.start);
;s.push_str(&self.emit_comments_at(&__leads, self.indent + 1));
            s.push_str(&format!("{}{}: {}", ipad, self.fmt_expr_compact(k), self.fmt_expr_compact(v)));
            if let Some(t) = self.drain_same_line_trailing(v.span.end) {
                s.push_str("  ");
                s.push_str(&t);
            }
            s.push('\n');
        }
        let saved = self.indent;
        self.indent += 1;
        let mut tail = String::new();
        self.drain_until_into(lit_span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", pad));
        s
    }

    fn fmt_struct_lit_wrapped(
        &mut self,
        ty: &Type,
        fields: &[(String, Span, Expr)],
        lit_span: Span,
        omitted: bool,
    ) -> String {
        let pad = " ".repeat(self.indent * self.opts.indent_width);
        let ipad = " ".repeat((self.indent + 1) * self.opts.indent_width);
        let prefix = if omitted {
            String::new()
        } else {
            format!("{} ", self.fmt_type(ty))
        };
        let mut s = format!("{}has\n", prefix);
        for (n, fspan, v) in fields {
            let __leads = self.take_leading(fspan.start);
;s.push_str(&self.emit_comments_at(&__leads, self.indent + 1));
            // nested struct/map literals need indent context
            let saved = self.indent;
            self.indent += 1;
            let val = self.fmt_expr_wrapped(v, 0);
            self.indent = saved;
            // multi-line values: first line inline, rest as-is
            if val.contains('\n') {
                let mut lines = val.lines();
                let first = lines.next().unwrap_or("");
                s.push_str(&format!("{}{} = {}\n", ipad, n, first));
                for l in lines {
                    s.push_str(l);
                    s.push('\n');
                }
            } else {
                s.push_str(&format!("{}{} = {}\n", ipad, n, val));
            }
            if let Some(t) = self.drain_same_line_trailing(v.span.end) {
                // trailing comment joins the last value line
                s.pop();
                s.push_str("  ");
                s.push_str(&t);
                s.push('\n');
            }
        }
        let saved = self.indent;
        self.indent += 1;
        let mut tail = String::new();
        self.drain_until_into(lit_span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", pad));
        s
    }

    fn fmt_match_wrapped(&mut self, m: &MatchExpr) -> String {
        let outer = self.indent;
        let mut s = format!("match {} do\n", self.fmt_expr_compact(&m.scrutinee));
        for arm in &m.arms {
            let __leads = self.take_leading(arm.span.start);
;s.push_str(&self.emit_comments_at(&__leads, outer + 1));
            let apad = " ".repeat((outer + 1) * self.opts.indent_width);
            let mut head = self.fmt_pattern(&arm.pattern);
            if let Some(g) = &arm.guard {
                head.push_str(&format!(" if {}", self.fmt_expr_compact(g)));
            }
            match &arm.body {
                MatchArmBody::Expr(e) => {
                    s.push_str(&format!("{}{} -> {}", apad, head, self.fmt_expr_compact(e)));
                    if let Some(t) = self.drain_same_line_trailing(arm.span.end) {
                        s.push_str("  ");
                        s.push_str(&t);
                    }
                    s.push('\n');
                }
                MatchArmBody::Block(b) => {
                    s.push_str(&format!("{}{} -> do\n", apad, head));
                    let saved = self.indent;
                    self.indent = outer + 1;
                    s.push_str(&self.fmt_block_inner(b, 1));
                    self.indent = saved;
                    s.push_str(&format!("\n{}end\n", apad));
                }
            }
        }
        let saved = self.indent;
        self.indent = outer + 1;
        let mut tail = String::new();
        self.drain_until_into(m.span.end, &mut tail);
        self.indent = saved;
        s.push_str(&tail);
        s.push_str(&format!("{}end", " ".repeat(outer * self.opts.indent_width)));
        s
    }
}

// ── Span helpers ───────────────────────────────────────────────────────────

fn item_span(item: &Item) -> Span {
    match item {
        Item::Import(d) => d.span,
        Item::Function(f) => f.span,
        Item::Struct(d) => d.span,
        Item::Class(d) => d.span,
        Item::Enum(d) => d.span,
        Item::Trait(d) => d.span,
        Item::Typedef(d) => d.span,
        Item::Distinct(d) => d.span,
        Item::Extension(d) => d.span,
        Item::Extern(d) => d.span,
        Item::Init(b) => b.span,
        Item::Const(d) => d.span,
        Item::Var(d) => d.span,
        Item::Attributed { item, .. } => item_span(item),
    }
}

fn ext_member_span(m: &ExtensionMember) -> Span {
    match m {
        ExtensionMember::Field(f) => f.span,
        ExtensionMember::Function(f) => f.span,
        ExtensionMember::Operator(o) => o.span,
        ExtensionMember::Property(p) => p.span,
        ExtensionMember::Conversion(c) => c.span,
    }
}

fn extern_member_span(m: &ExternMember) -> Span {
    match m {
        ExternMember::Function { span, .. } => *span,
        ExternMember::Struct { span, .. } => *span,
        ExternMember::Enum { span, .. } => *span,
        ExternMember::Const { span, .. } => *span,
    }
}

fn stmt_span(st: &Stmt) -> Span {
    match st {
        Stmt::VarDecl(d) => d.span,
        Stmt::Const(d) => d.span,
        Stmt::Destructure(d) => d.span,
        Stmt::Assert(d) => d.span,
        Stmt::If(d) => d.span,
        Stmt::While(d) => d.span,
        Stmt::Loop(d) => d.span,
        Stmt::For(d) => d.span,
        Stmt::Return(d) => d.span,
        Stmt::Expr(d) => d.span,
        Stmt::Block(d) => d.span,
        Stmt::Break(d) => d.span,
        Stmt::Continue(d) => d.span,
        Stmt::Defer(d) => d.span,
        Stmt::Delete(d) => d.span,
    }
}

fn vis_prefix(v: Visibility) -> &'static str {
    match v {
        Visibility::Public => "public ",
        Visibility::Private => "private ",
        Visibility::Default => "",
    }
}

fn fits(indent: usize, text: &str, max: usize) -> bool {
    indent * 4 + text.len() <= max
}

fn bin_op(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::Is => "is",
        BinOp::IsNot => "is not",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        BinOp::NullCoalesce => "??",
        BinOp::Range => "..",
        BinOp::RangeInclusive => "..=",
        BinOp::CompoundAdd => "+",
        BinOp::CompoundSub => "-",
        BinOp::CompoundMul => "*",
        BinOp::CompoundDiv => "/",
        BinOp::CompoundMod => "%",
        BinOp::CompoundBitAnd => "&",
        BinOp::CompoundBitOr => "|",
        BinOp::CompoundBitXor => "^",
        BinOp::CompoundShl => "<<",
        BinOp::CompoundShr => ">>",
    }
}

fn unary_prefix(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Not => "not ",
        UnaryOp::Pos => "+",
        UnaryOp::BitNot => "~",
        UnaryOp::Inc => "++",
        UnaryOp::Dec => "--",
    }
}

fn unary_postfix(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Inc => "++",
        UnaryOp::Dec => "--",
        _ => "",
    }
}

fn escape_string(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

fn escape_char(c: char) -> String {
    match c {
        '\n' => "\\n".into(),
        '\r' => "\\r".into(),
        '\t' => "\\t".into(),
        '\'' => "\\'".into(),
        '\\' => "\\\\".into(),
        _ => c.to_string(),
    }
}



// ── File / directory driver ────────────────────────────────────────────────

/// Collect `.hll` files from `paths` (files + recursive directories).
pub fn collect_sources(paths: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for p in paths {
        collect_one(p, &mut files);
    }
    files.sort();
    files.dedup();
    files
}

fn collect_one(p: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    if p.is_file() {
        if p.extension().is_some_and(|e| e == "hll") {
            files.push(p.to_path_buf());
        } else if p.is_file() {
            // accept explicit non-.hll files too? No — only .hll per spec.
        }
        return;
    }
    if p.is_dir() {
        let entries = std::fs::read_dir(p).map(|r| r.collect::<Vec<_>>()).unwrap_or_default();
        for e in entries {
            if let Ok(e) = e {
                collect_one(&e.path(), files);
            }
        }
    }
}

/// Format one file in place. Returns true when the file changed.
pub fn format_file(path: &std::path::Path) -> Result<bool, FormatError> {
    let src = std::fs::read_to_string(path).map_err(|e| FormatError {
        message: format!("failed to read {}: {e}", path.display()),
        span: None,
    })?;
    let formatted = format_source(&src)?;
    if formatted == src {
        return Ok(false);
    }
    std::fs::write(path, formatted).map_err(|e| FormatError {
        message: format!("failed to write {}: {e}", path.display()),
        span: None,
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        format_source(src).expect("format failed")
    }

    #[test]
    fn idempotent_simple() {
        let src = "int   main()  do\nreturn  1\nend\n";
        let once = fmt(src);
        let twice = fmt(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn binary_spacing() {
        let out = fmt("int main() do\nint x=1+2\nreturn x\nend\n");
        assert!(out.contains("1 + 2"), "got: {out}");
    }

    #[test]
    fn var_alignment() {
        let src = "int main() do\nstring name = \"a\"\nint age = 1\nbool active = true\nreturn 1\nend\n";
        let out = fmt(src);
        assert!(out.contains("string name"), "got: {out}");
        // group of 3 → padded
        assert!(out.contains("int    age"), "got: {out}");
        let twice = fmt(&out);
        assert_eq!(out, twice);
    }

    #[test]
    fn imports_two_blanks() {
        let src = "import foo\nimport bar\nconst int X = 1\n";
        let out = fmt(src);
        assert!(out.contains("import foo\nimport bar\n\n\nconst int X = 1"), "got: {out:?}");
    }

    #[test]
    fn class_ordering() {
        let src = "class User has\npublic void save() do\nreturn\nend\nstring name\nUser(string name) initialize\n~User() do\nreturn\nend\nend\n";
        let out = fmt(src);
        let f = out.find("string name").unwrap();
        let c = out.find("initialize").unwrap();
        let d = out.find("~User").unwrap();
        let m = out.find("save").unwrap();
        assert!(f < c && c < d && d < m, "got: {out}");
    }

    #[test]
    fn comments_preserved() {
        let src = "// header\nint main() do\n// inner\nreturn 1 // trailing\nend\n";
        let out = fmt(src);
        assert!(out.contains("// header"), "got: {out}");
        assert!(out.contains("// inner"), "got: {out}");
        assert!(out.contains("// trailing"), "got: {out}");
        assert_eq!(out, fmt(&out));
    }

    #[test]
    fn call_wrapping_idempotent() {
        let src = "int main() do\nresult = create_user(name, age, active, extra_long_argument_name_here, another_long_argument, more_args_here_ok)\nreturn 1\nend\n";
        let out = fmt(src);
        let twice = fmt(&out);
        assert_eq!(out, twice);
    }

    #[test]
    fn examples_idempotent() {
        for name in [
            "basics", "hello_io", "data_control", "abstraction", "advanced",
        ] {
            let path = format!("../..//examples/{}.hll", name);
            // tests run with CWD = crates/hella-fmt
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../examples/{}.hll", name));
            if !p.is_file() {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            let once = format_source(&src).unwrap_or_else(|e| panic!("{name}: {e}"));
            let twice = format_source(&once).expect("second pass failed");
            assert_eq!(once, twice, "not idempotent: {name}");
            let _ = path;
        }
    }
}
