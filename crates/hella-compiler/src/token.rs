//! Hella tokens — mirrors EBNF §2-5 + §6-7 operator list.
//!
//! Phase 0: full keyword and operator coverage even though the parser
//! in Phase 1 only consumes a subset. `logos` is used for lexing, but
//! the `Token` enum itself is logos-agnostic and carries spans
//! separately (see `Span` and `SpannedToken`).

use logos::Logos;

/// Byte-offset span, inclusive start exclusive end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
    /// Convert to miette source span.
    pub fn to_source_span(self) -> miette::SourceSpan {
        (self.start, self.end - self.start).into()
    }
}

/// A token plus its span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpannedToken {
    pub token: Token,
    pub span: Span,
}

// Helper to emit `Newline` — handled explicitly via regex, not whitespace.
fn newline_callback(lex: &mut logos::Lexer<Token>) -> bool {
    // logos already advanced over the newline; we just signal presence.
    let _ = lex.slice();
    true
}

fn multiline_string_callback(lex: &mut logos::Lexer<Token>) -> bool {
    // Opening `"""` already matched; scan for closing `"""`
    let src = lex.source();
    let start = lex.span().start;
    let rest = &src[start + 3..];
    if let Some(end) = rest.find("\"\"\"") {
        lex.bump(end + 3);
        true
    } else {
        false
    }
}

fn float_callback(lex: &mut logos::Lexer<Token>) -> bool {
    let slice = lex.slice();
    // Reject `1.` when followed by `.` (i.e. `1..` range) so that `1..2` lexes as `1` `..` `2`
    // `1.` as float should not consume the first `.` of `..`
    if slice.ends_with('.') {
        let end = lex.span().end;
        let src = lex.source();
        if end < src.len() && src[end..].starts_with('.') {
            return false;
        }
    }
    true
}

/// All Hella tokens. Keywords are matched before `Ident` via `#[token]` priority.
#[derive(Logos, Debug, Clone, PartialEq, Eq, Hash)]
#[logos(skip r"[ \t\f]+")] // whitespace except newline; newlines are significant
#[logos(skip(r"//[^\n]*", allow_greedy = true))] // line comments
#[logos(skip r"/\*([^*]|\*[^/])*\*/")] // block comments (non-nested)
pub enum Token {
    // ── Statement terminator support ──────────────────────────────────
    #[regex(r"\r\n|\n|\r", newline_callback)]
    Newline,

    #[token(";")]
    Semicolon,

    // ── Keywords (EBNF §2) ────────────────────────────────────────────
    // Keep declaration order: logos priority is definition order for equal-length matches,
    // so exact `#[token]` entries must appear before `#[regex]` Ident.
    #[token("and")]
    And,
    #[token("any")]
    Any,
    #[token("arr")]
    Arr,
    #[token("vec")]
    Vec,
    #[token("assert")]
    Assert,
    #[token("async")]
    Async,
    #[token("await")]
    Await,
    #[token("spawn")]
    Spawn,
    #[token("scope")]
    Scope,
    #[token("yield")]
    Yield,
    #[token("bool")]
    Bool,
    #[token("break")]
    Break,
    #[token("class")]
    Class,
    #[token("const")]
    Const,
    #[token("continue")]
    Continue,
    #[token("debug_assert")]
    DebugAssert,
    #[token("defer")]
    Defer,
    #[token("delete")]
    Delete,
    #[token("distinct")]
    Distinct,
    #[token("do")]
    Do,
    #[token("double")]
    Double,
    #[token("else")]
    Else,
    #[token("enum")]
    Enum,
    #[token("extends")]
    Extends,
    #[token("extend")]
    Extend,
    #[token("explicit")]
    Explicit,
    #[token("extern")]
    Extern,
    #[token("float")]
    Float,
    #[token("for")]
    For,
    #[token("function")]
    Function,
    #[token("if")]
    If,
    #[token("implements")]
    Implements,
    #[token("import")]
    Import,
    #[token("from")]
    From,
    #[token("in")]
    In,
    #[token("init")]
    Init,
    #[token("int")]
    Int,
    #[token("is")]
    Is,
    #[token("loop")]
    Loop,
    #[token("match")]
    Match,
    #[token("new")]
    New,
    #[token("not")]
    Not,
    #[token("null")]
    Null,
    #[token("open")]
    Open,
    #[token("operator")]
    OperatorKw,
    #[token("or")]
    Or,
    #[token("out")]
    Out,
    #[token("own")]
    Own,
    #[token("override")]
    Override,
    #[token("private")]
    Private,
    #[token("public")]
    Public,
    #[token("ref")]
    Ref,
    #[token("return")]
    Return,
    #[token("sealed")]
    Sealed,
    #[token("static")]
    Static,
    #[token("string")]
    StringKw,
    #[token("struct")]
    Struct,
    #[token("super")]
    Super,
    #[token("this")]
    This,
    #[token("trait")]
    Trait,
    #[token("typedef")]
    Typedef,
    #[token("void")]
    Void,
    #[token("while")]
    While,
    #[token("where")]
    Where,
    #[token("convert")]
    Convert,
    #[token("to")]
    To,
    #[token("char")]
    CharKw,
    // structural delimiters (EBNF §13/22-25) — not listed in §2 but reserved
    #[token("has")]
    Has,
    #[token("end")]
    End,
    #[token("get")]
    Get,
    #[token("set")]
    Set,
    #[token("initialize")]
    Initialize,
    // soft keywords / literals that look like keywords
    #[token("true")]
    True,
    #[token("false")]
    False,
    #[token("Self")]
    SelfType,

    // ── Operators — compound first (EBNF §5) ──────────────────────────
    #[token("<<=")]
    LShiftAssign,
    #[token(">>=")]
    RShiftAssign,
    #[token("..=")]
    DotDotEq,
    #[token("?.")]
    QuestionDot,
    #[token("??")]
    QuestionQuestion,
    #[token("::")]
    ColonColon,
    #[token("->")]
    Arrow,
    #[token("=>")]
    FatArrow,
    #[token("<<")]
    LShift,
    #[token(">>")]
    RShift,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,
    #[token("+=")]
    PlusAssign,
    #[token("-=")]
    MinusAssign,
    #[token("*=")]
    StarAssign,
    #[token("/=")]
    SlashAssign,
    #[token("%=")]
    PercentAssign,
    #[token("&=")]
    AndAssign,
    #[token("|=")]
    OrAssign,
    #[token("^=")]
    XorAssign,
    #[token("++")]
    PlusPlus,
    #[token("--")]
    MinusMinus,
    #[token("...")]
    DotDotDot,
    #[token("..")]
    DotDot,
    #[token("==")]
    EqEq, // NOTE: not in EBNF — `"="` only, but keep for future/typo diagnostics

    // single-char / remaining
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("=")]
    Eq,
    #[token("&")]
    Ampersand,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,
    #[token("~")]
    Tilde,
    #[token("?")]
    Question,
    #[token(":")]
    Colon,
    #[token(".")]
    Dot,
    #[token(",")]
    Comma,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("@")]
    At,

    // ── Literals (EBNF §4) ────────────────────────────────────────────
    // Order matters: hex/binary before decimal.
    #[regex(r"0x[0-9a-fA-F_]+")]
    HexInt,
    #[regex(r"0b[01_]+")]
    BinInt,
    #[regex(r"[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9_]+)?|\.[0-9][0-9_]*([eE][+-]?[0-9_]+)?|[0-9][0-9_]*[eE][+-]?[0-9_]+", float_callback)]
    FloatLit,
    #[regex(r"[0-9][0-9_]*")]
    IntLit,

    // Strings: normal `"..."` and multiline `"""..."""` (both with interpolation handled in parser)
    // Multiline must come before normal to prefer `"""` over `""` — priority 2 ensures `"""` wins
    #[regex("\"\"\"", multiline_string_callback, priority = 2)]
    #[regex(r#""([^"\\\n]|\\.)*""#)]
    StringLit,
    #[regex(r#"'([^'\\\n]|\\.)*'"#)]
    CharLit,
    // Raw string r"..." — no escapes, no interpolation
    #[regex(r#"r"[^"]*""#)]
    RawStringLit,

    // ── Identifier (must be last among word-like patterns) ────────────
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,
    // ── Errors are injected by the lexer wrapper (Logos `Error`) ─────
    // We surface them as a distinct variant via the lexer's Result.
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Use debug-ish names for operator printing; keywords print as themselves.
        let s = match self {
            Self::Newline => "<newline>",
            Self::Semicolon => ";",
            Self::And => "and",
            Self::Any => "any",
            Self::Arr => "arr",
            Self::Vec => "vec",
            Self::Assert => "assert",
            Self::Async => "async",
            Self::Await => "await",
            Self::Spawn => "spawn",
            Self::Scope => "scope",
            Self::Yield => "yield",
            Self::Bool => "bool",
            Self::Break => "break",
            Self::Class => "class",
            Self::Const => "const",
            Self::Continue => "continue",
            Self::DebugAssert => "debug_assert",
            Self::Defer => "defer",
            Self::Delete => "delete",
            Self::Distinct => "distinct",
            Self::Do => "do",
            Self::Double => "double",
            Self::Else => "else",
            Self::Enum => "enum",
            Self::Extends => "extends",
            Self::Extend => "extend",
            Self::Explicit => "explicit",
            Self::Extern => "extern",
            Self::Float => "float",
            Self::For => "for",
            Self::Function => "function",
            Self::If => "if",
            Self::Implements => "implements",
            Self::Import => "import",
            Self::From => "from",
            Self::In => "in",
            Self::Init => "init",
            Self::Int => "int",
            Self::Is => "is",
            Self::Loop => "loop",
            Self::Match => "match",
            Self::New => "new",
            Self::Not => "not",
            Self::Null => "null",
            Self::Open => "open",
            Self::OperatorKw => "operator",
            Self::Or => "or",
            Self::Out => "out",
            Self::Own => "own",
            Self::Override => "override",
            Self::Private => "private",
            Self::Public => "public",
            Self::Ref => "ref",
            Self::Return => "return",
            Self::Sealed => "sealed",
            Self::Static => "static",
            Self::StringKw => "string",
            Self::Struct => "struct",
            Self::Super => "super",
            Self::This => "this",
            Self::Trait => "trait",
            Self::Typedef => "typedef",
            Self::Void => "void",
            Self::While => "while",
            Self::Where => "where",
            Self::Convert => "convert",
            Self::To => "to",
            Self::CharKw => "char",
            Self::Has => "has",
            Self::End => "end",
            Self::Get => "get",
            Self::Set => "set",
            Self::Initialize => "initialize",
            Self::True => "true",
            Self::False => "false",
            Self::SelfType => "Self",
            Self::LShiftAssign => "<<=",
            Self::RShiftAssign => ">>=",
            Self::DotDotEq => "..=",
            Self::QuestionDot => "?.",
            Self::QuestionQuestion => "??",
            Self::ColonColon => "::",
            Self::Arrow => "->",
            Self::FatArrow => "=>",
            Self::LShift => "<<",
            Self::RShift => ">>",
            Self::LtEq => "<=",
            Self::GtEq => ">=",
            Self::PlusAssign => "+=",
            Self::MinusAssign => "-=",
            Self::StarAssign => "*=",
            Self::SlashAssign => "/=",
            Self::PercentAssign => "%=",
            Self::AndAssign => "&=",
            Self::OrAssign => "|=",
            Self::XorAssign => "^=",
            Self::PlusPlus => "++",
            Self::MinusMinus => "--",
            Self::DotDotDot => "...",
            Self::DotDot => "..",
            Self::EqEq => "==",
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::Eq => "=",
            Self::Ampersand => "&",
            Self::Pipe => "|",
            Self::Caret => "^",
            Self::Tilde => "~",
            Self::Question => "?",
            Self::Colon => ":",
            Self::Dot => ".",
            Self::Comma => ",",
            Self::LParen => "(",
            Self::RParen => ")",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::LBrace => "{",
            Self::RBrace => "}",
            Self::At => "@",
            Self::HexInt => "<hex int>",
            Self::BinInt => "<bin int>",
            Self::FloatLit => "<float>",
            Self::IntLit => "<int>",
            Self::StringLit => "<string>",
            Self::CharLit => "<char>",
            Self::RawStringLit => "<raw string>",
            Self::Ident => "<ident>",
        };
        write!(f, "{s}")
    }
}
