//! Hella AST — mirrors EBNF §6-8, §13-21 for Phase 1 subset.
//! Every node carries a `Span` (see SKILL.md: Spans everywhere).

use crate::token::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub items: Vec<Item>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDecl {
    pub path: Vec<String>,
    pub path_span: Span,
    pub symbols: Option<Vec<(String, Span)>>, // None = whole module
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub name_span: Span,
    pub args: Vec<AttributeArg>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeArg {
    Expr(Expr),
    /// `name: value` (colon form, EBNF `attribute-argument`).
    Named(String, Span, Expr),
    /// `name = value` (equals form). Used by `@cfg(os = "macos")`; kept
    /// distinct from [`AttributeArg::Named`] so the formatter can round-trip
    /// the exact spelling the author wrote.
    Assign(String, Span, Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Import(ImportDecl),
    Function(Function),
    Struct(StructDecl),
    Class(ClassDecl),
    Enum(EnumDecl),
    Trait(TraitDecl),
    Typedef(TypedefDecl),
    Distinct(DistinctDecl),
    Extension(ExtensionDecl),
    Extern(ExternDecl),
    Init(Block),
    Const(ConstDecl),
    Var(VarDecl),
    Attributed { attrs: Vec<Attribute>, item: Box<Item> },
}

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum Visibility {
    Public,
    Private,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub ret_ty: Type,
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub body: Block,
    pub visibility: Visibility,
    pub is_static: bool,
    pub is_sealed: bool,
    pub is_override: bool,
    pub is_open: bool,
    /// `Ret name(...) async do ... end` — runs on its own thread; callers
    /// get a `task<Ret>` back and must `await` it inside a `scope` (Async-1).
    /// Sync functions keep `is_async == false`.
    pub is_async: bool,
    pub async_span: Option<Span>,
    pub generic_params: Vec<GenericParam>,
    pub where_clause: Option<WhereClause>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamMode {
    None,
    Ref,
    Out,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub mode: ParamMode,
    pub is_variadic: bool,
    pub ty: Type,
    pub name: String,
    pub name_span: Span,
    /// Default value from `= expr` (EBNF `parameter`); `None` when absent.
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericParam {
    pub name: String,
    pub name_span: Span,
    pub bounds: Vec<Type>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhereClause {
    pub constraints: Vec<WhereConstraint>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhereConstraint {
    pub ty: Type,
    pub bounds: Vec<Type>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Int(Span),
    Bool(Span),
    Void(Span),
    String(Span),
    Char(Span),
    Float(Span),
    Double(Span),
    Named(String, Span),       // Phase 2: struct name
    Generic(String, Vec<Type>, Span), // Phase 5: Box<int>
    FunctionType(Box<Type>, Vec<Type>, Span), // function<Ret(Args)>
    Tuple(Vec<Type>, Span),
    Any(Span),
    Array(Box<Type>, Span),    // T[]  (Phase 2)
    /// Fixed-size array (types skill §7-8): `TYPE arr` (size inferred from
    /// initializer) or `TYPE arr[SIZE]` (explicit size, zero-initialized).
    /// `arr` is a compiler keyword; the element type precedes it.
    FixedArray { elem: Box<Type>, size: Option<u64>, span: Span },
    /// Dynamic owning vector (types skill §9): `TYPE vec`. Vectors may grow;
    /// empty vectors come from the `vec[]` expression with `any` element type
    /// until the first `push` establishes it.
    Vec { elem: Box<Type>, span: Span },
    /// Associative map (types skill §12): `KEY_TYPE:VALUE_TYPE`
    /// (e.g. `string:int`). There is no `map` keyword.
    Map { key: Box<Type>, value: Box<Type>, span: Span },
    Pointer(Box<Type>, Span),  // T*  (Phase 2)
    Optional(Box<Type>, Span), // T? (Phase 2 future)
    /// Owning heap pointer: `own T` (moves, scope-destroyed, never null).
    Own(Box<Type>, Span),
    /// Pending computation: `task<T>` — the return handle of an `async`
    /// function call or a `spawn` expression (Async-1). Single-await,
    /// scope-bound; `T` is the `await` result type (`task<void>` awaits
    /// to no value).
    Task(Box<Type>, Span),
}

impl Type {
    pub fn span(&self) -> Span {
        match self {
            Type::Int(s)
            | Type::Bool(s)
            | Type::Void(s)
            | Type::String(s)
            | Type::Char(s)
            | Type::Float(s)
            | Type::Double(s)
            | Type::Named(_, s)
            | Type::Generic(_, _, s)
            | Type::FunctionType(_, _, s)
            | Type::Tuple(_, s)
            | Type::Any(s)
            | Type::Array(_, s)
            | Type::FixedArray { span: s, .. }
            | Type::Vec { span: s, .. }
            | Type::Map { span: s, .. }
            | Type::Pointer(_, s)
            | Type::Optional(_, s)
            | Type::Own(_, s)
            | Type::Task(_, s) => *s,
        }
    }
    pub fn name(&self) -> String {
        match self {
            Type::Int(_) => "int".into(),
            Type::Bool(_) => "bool".into(),
            Type::Void(_) => "void".into(),
            Type::String(_) => "string".into(),
            Type::Char(_) => "char".into(),
            Type::Float(_) => "float".into(),
            Type::Double(_) => "double".into(),
            Type::Named(n, _) => n.clone(),
            Type::Generic(n, args, _) => format!("{}<{}>", n, args.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")),
            Type::FunctionType(ret, args, _) => format!("function<{}({})>", ret.name(), args.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")),
            Type::Tuple(tys, _) => format!("({})", tys.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")),
            Type::Any(_) => "any".into(),
            Type::Array(el, _) => format!("{}[]", el.name()),
            Type::FixedArray { elem, size, .. } => match size {
                Some(n) => format!("{} arr[{}]", elem.name(), n),
                None => format!("{} arr", elem.name()),
            },
            Type::Vec { elem, .. } => format!("{} vec", elem.name()),
            Type::Map { key, value, .. } => format!("{}:{}", key.name(), value.name()),
            Type::Pointer(el, _) => format!("{}*", el.name()),
            Type::Optional(el, _) => format!("{}?", el.name()),
            Type::Own(el, _) => format!("own {}", el.name()),
            Type::Task(el, _) => format!("task<{}>", el.name()),
        }
    }
    pub fn is_void(&self) -> bool {
        matches!(self, Type::Void(_))
    }
}

// Phase 2: struct declaration + member
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructDecl {
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<StructField>,
    pub generic_params: Vec<GenericParam>,
    pub where_clause: Option<WhereClause>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructField {
    pub ty: Type,
    pub name: String,
    pub name_span: Span,
    pub visibility: Visibility,
    pub default: Option<Expr>,
    pub span: Span,
}

// Phase 4: class
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassDecl {
    pub name: String,
    pub name_span: Span,
    pub is_open: bool,
    pub is_sealed: bool,
    pub generic_params: Vec<GenericParam>,
    pub where_clause: Option<WhereClause>,
    pub extends: Option<Type>,
    pub implements: Vec<Type>,
    pub fields: Vec<StructField>,
    pub methods: Vec<Function>,
    pub constructors: Vec<ConstructorDecl>,
    pub destructors: Vec<DestructorDecl>,
    pub properties: Vec<PropertyDecl>,
    pub operators: Vec<OperatorDecl>,
    pub conversions: Vec<ConversionDecl>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstructorDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub body: Option<Block>,
    pub visibility: Visibility,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestructorDecl {
    pub name: String,
    pub name_span: Span,
    pub body: Block,
    pub visibility: Visibility,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyDecl {
    pub ty: Option<Type>,
    pub name: String,
    pub name_span: Span,
    pub visibility: Visibility,
    pub getter: Option<Block>,
    pub setter: Option<(Param, Block)>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitDecl {
    pub name: String,
    pub name_span: Span,
    pub generic_params: Vec<GenericParam>,
    pub where_clause: Option<WhereClause>,
    pub methods: Vec<TraitMethod>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitMethod {
    pub ret_ty: Type,
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub is_sealed: bool,
    pub generic_params: Vec<GenericParam>,
    pub where_clause: Option<WhereClause>,
    pub span: Span,
}

// Phase 4: enum (EBNF §28)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDecl {
    pub name: String,
    pub name_span: Span,
    pub generic_params: Vec<GenericParam>,
    pub where_clause: Option<WhereClause>,
    pub variants: Vec<EnumVariant>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumVariant {
    pub name: String,
    pub name_span: Span,
    pub discriminant: Option<Expr>, // EBNF: `identifier [ "=" expression ]`
    pub payload_params: Vec<Param>, // EBNF: `identifier [ "(" [parameter-list] ")" ]` multi-param
    pub span: Span,
}

// Phase 5: type aliases and distinct
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedefDecl {
    pub name: String,
    pub name_span: Span,
    pub ty: Type,
    pub visibility: Visibility,
    pub generic_params: Vec<GenericParam>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistinctDecl {
    pub name: String,
    pub name_span: Span,
    pub ty: Type,
    pub visibility: Visibility,
    pub generic_params: Vec<GenericParam>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionDecl {
    pub ty: Type,
    pub members: Vec<ExtensionMember>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionMember {
    Field(StructField),
    Function(Function),
    Operator(OperatorDecl),
    Property(PropertyDecl),
    Conversion(ConversionDecl),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorDecl {
    pub visibility: Visibility,
    pub is_static: bool,
    pub op: String,
    pub op_span: Span,
    pub params: Vec<Param>,
    pub body: Block,
    pub where_clause: Option<WhereClause>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionDecl {
    pub visibility: Visibility,
    pub is_explicit: bool,
    pub from_ty: Type,
    pub to_ty: Type,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternDecl {
    pub lib: String,
    pub lib_span: Span,
    pub file: String,
    pub file_span: Span,
    pub members: Vec<ExternMember>,
    pub visibility: Visibility,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternMember {
    Function { ty: Type, name: String, name_span: Span, params: Vec<ExternParam>, span: Span },
    Struct { name: String, name_span: Span, fields: Vec<ExternField>, span: Span },
    Enum { name: String, name_span: Span, variants: Vec<EnumVariant>, span: Span },
    Const { ty: Type, name: String, name_span: Span, span: Span },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternParam {
    pub is_variadic: bool,
    /// `ref`/`out` on an extern parameter means the C function writes
    /// through the pointer (e.g. `rand_s(ref int out)`): the call site
    /// passes the variable's address, exactly like a Hella `ref` param.
    pub mode: ParamMode,
    pub ty: Type,
    pub name: String,
    pub name_span: Span,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternField {
    pub ty: Type,
    pub name: String,
    pub name_span: Span,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    VarDecl(VarDecl),
    Const(ConstDecl),
    Destructure(DestructureStmt),
    Assert(AssertStmt),
    If(IfStmt),
    While(WhileStmt),
    Loop(LoopStmt),
    For(ForStmt),
    Return(ReturnStmt),
    Expr(ExprStmt),
    Block(Block),
    Break(BreakStmt),
    Continue(ContinueStmt),
    Defer(DeferStmt),
    Delete(DeleteStmt),
    /// Structured-concurrency scope: `scope do ... end` (Async-1). Child
    /// tasks created inside must be awaited before the scope exits
    /// (implicit `join`); awaiting outside the scope is an error.
    Scope(Block),
    /// Cooperative yield: `yield` (optional terminator) — parks the current
    /// task and lets ready siblings run (Async-1/Async-5).
    Yield(Span),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestructureStmt {
    pub targets: Vec<DestructureTarget>,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestructureTarget {
    Ident(String, Span),
    Wildcard(Span),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertStmt {
    pub is_debug: bool,
    pub cond: Expr,
    pub message: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarDecl {
    pub visibility: Visibility,
    pub ty: Type,
    pub name: String,
    pub name_span: Span,
    pub init: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstDecl {
    pub visibility: Visibility,
    pub ty: Option<Type>,
    pub name: String,
    pub name_span: Span,
    pub init: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfStmt {
    pub cond: Expr,
    pub then_block: Block,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhileStmt {
    pub cond: Expr,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnStmt {
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprStmt {
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakStmt {
    pub label: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueStmt {
    pub label: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopStmt {
    pub label: Option<String>,
    pub label_span: Option<Span>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForStmt {
    pub label: Option<String>,
    pub label_span: Option<Span>,
    pub var: String,
    pub var_span: Span,
    /// Optional second loop variable (`for x, i in ...`). Arrays, vectors
    /// and strings bind the index; maps bind the value (first binds the key).
    pub var2: Option<(String, Span)>,
    pub iter: Expr,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferStmt {
    pub inner: DeferInner,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteStmt {
    pub target: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferInner {
    Expr(Box<Expr>),
    Block(Block),
}

// ── Expressions (§8) ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpolatedPart {
    Literal(String),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosureBody {
    Expr(Box<Expr>),
    Block(Block),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallArg {
    Expr(Expr),
    Named { name: String, name_span: Span, value: Expr, span: Span },
    Out { ty: Option<Type>, name: String, name_span: Span, span: Span },
    Ref { expr: Box<Expr>, span: Span },
}

impl CallArg {
    pub fn span(&self) -> Span {
        match self {
            CallArg::Expr(e) => e.span,
            CallArg::Named { span, .. } => *span,
            CallArg::Out { span, .. } => *span,
            CallArg::Ref { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    IntLit(i64),
    FloatLit(String),
    BoolLit(bool),
    StringLit(String),
    InterpolatedString(Vec<InterpolatedPart>, Span),
    CharLit(char),
    Ident(String),
    This,
    Super,
    Null,
    Paren(Box<Expr>),
    Tuple(Vec<Expr>),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Assign {
        lhs: Box<Expr>,
        value: Box<Expr>,
    },
    CompoundAssign {
        op: BinOp,
        lhs: Box<Expr>,
        value: Box<Expr>,
    },
    Conditional {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    Range {
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    },
    Postfix {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    NullableMemberAccess {
        object: Box<Expr>,
        field: String,
        field_span: Span,
    },
    Call {
        callee: String,
        callee_span: Span,
        args: Vec<CallArg>,
        type_args: Vec<Type>, // Phase 5: generic args like foo<int>(x)
    },
    MethodCall {
        object: Box<Expr>,
        method: String,
        method_span: Span,
        args: Vec<CallArg>,
    },
    MemberAccess {
        object: Box<Expr>,
        field: String,
        field_span: Span,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    }, // a[i] Phase 2
    /// Array literal (types skill §7-8): `[1, 2, 3, 4]` — initializer for
    /// `TYPE arr` declarations. Size is inferred from element count.
    ArrayLit(Vec<Expr>),
    /// Empty vector (types skill §10): `vec[]` — no established element type
    /// until the first `push`. Declared with `any` (e.g. `any xs = vec[]`).
    VecEmpty(Span),
    /// Map literal (types skill §13): `has "k": v, ... end` (explicit
    /// `TYPE has ... end` or inferred bare `has ... end` in var init).
    MapLit {
        ty: Type,
        entries: Vec<(Expr, Expr)>,
    },
    Slice {
        object: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    }, // a[l..r] / a[..r] / a[l..] / a[..] per EBNF §8 index-or-range
    StructLit {
        ty: Type,
        fields: Vec<(String, Span, Expr)>,
    }, // Type has field = expr ... end
    EnumVariant {
        enum_name: Option<String>, // qualified prefix if any, e.g. Option in Option.Some
        variant: String,
        variant_span: Span,
        args: Vec<CallArg>,
    },
    Match(MatchExpr),
    Closure {
        params: Vec<Param>,
        body: Box<ClosureBody>,
        span: Span,
    },
    /// Heap construction: `new Type(args)` — runs the constructor on a
    /// fresh heap object, yields an owning value.
    New {
        ty: Type,
        args: Vec<CallArg>,
        span: Span,
    },
    /// Blocking wait: `await task_expr` (Async-1). Joins the child task,
    /// parks the current task until the child completes, then yields the
    /// child's return value. Errors when used outside an `async` function
    /// body or when the task already completed / lives in another scope.
    Await { task: Box<Expr>, span: Span },
    /// Concurrent child: `spawn callee(args)` or `spawn block-expr`
    /// (Async-1/Async-4). Runs the async callee on its own worker thread,
    /// returns `task<T>` immediately. Must appear inside a `scope` in an
    /// `async` function (or `async main`); the enclosing scope joins it.
    Spawn { task: Box<Expr>, span: Span },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg, // -
    Not, // not
    Pos, // + (unary plus, no-op)
    BitNot, // ~
    Inc, // ++ prefix/postfix
    Dec, // -- prefix/postfix
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot, // "is" / "is not"
    And,
    Or,
    BitAnd, // &
    BitOr, // |
    BitXor, // ^
    Shl, // <<
    Shr, // >>
    NullCoalesce, // ??
    Range, // ..
    RangeInclusive, // ..=
    CompoundAdd, // +=
    CompoundSub, // -=
    CompoundMul, // *=
    CompoundDiv, // /=
    CompoundMod, // %=
    CompoundBitAnd, // &=
    CompoundBitOr, // |=
    CompoundBitXor, // ^=
    CompoundShl, // <<=
    CompoundShr, // >>=
}

// Phase 2: simple match (EBNF §10, §16)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchExpr {
    pub scrutinee: Box<Expr>,
    pub arms: Vec<MatchArm>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: MatchArmBody,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    Wildcard(Span), // _
    Var(String, Span),
    LitInt(i64, Span),
    LitBool(bool, Span),
    Enum {
        variant: String,
        variant_span: Span,
        payload: Option<Vec<Pattern>>,
    },
    Tuple(Vec<Pattern>, Span), // (a, b, ...)
    Alternative(Vec<Pattern>, Span), // a | b | c  or  a or b
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard(s) => *s,
            Pattern::Var(_, s) => *s,
            Pattern::LitInt(_, s) => *s,
            Pattern::LitBool(_, s) => *s,
            Pattern::Enum{variant_span, ..} => *variant_span,
            Pattern::Tuple(_, s) => *s,
            Pattern::Alternative(_, s) => *s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchArmBody {
    Expr(Box<Expr>),
    Block(Block), // `do ... end` – evaluated for value via last expr? For now treat as block with return
}
