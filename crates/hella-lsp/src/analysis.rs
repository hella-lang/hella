//! Symbol analysis built on the compiler AST.
//!
//! Walks a parsed `Program` into a lightweight symbol table (top-level
//! declarations plus local variables/parameters scoped to their block), then
//! answers the LSP features we support:
//!
//! - `textDocument/hover`
//! - `textDocument/definition`
//! - `textDocument/completion`
//! - `textDocument/documentSymbol`

use std::collections::HashSet;

use hella_compiler::ast::{self, ExprKind, Item, Stmt};
use hella_compiler::token::{Span, Token};
use lsp_types::{
    CompletionItem, CompletionItemKind, DocumentSymbol, Hover, HoverContents, InsertTextFormat,
    Location, MarkupContent, MarkupKind, SymbolKind,
};

use crate::document::span_to_range;

/// Kinds of symbols we track (shared between LSP symbol kinds and our
/// internal detail formatting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    Function,
    Method,
    Constructor,
    Destructor,
    Struct,
    Class,
    Enum,
    Trait,
    Typedef,
    Distinct,
    Constant,
    Variable,
    Parameter,
    Field,
    Property,
    Variant,
    Module,
}

impl SymKind {
    pub fn lsp(self) -> SymbolKind {
        use SymKind::*;
        match self {
            Function => SymbolKind::FUNCTION,
            Constructor => SymbolKind::CONSTRUCTOR,
            // LSP has no destructor kind; destructors are method-like.
            Method | Destructor => SymbolKind::METHOD,
            Struct => SymbolKind::STRUCT,
            Class => SymbolKind::CLASS,
            Enum => SymbolKind::ENUM,
            Trait => SymbolKind::INTERFACE,
            Typedef | Distinct => SymbolKind::TYPE_PARAMETER,
            Constant => SymbolKind::CONSTANT,
            Variable | Parameter => SymbolKind::VARIABLE,
            Field => SymbolKind::FIELD,
            Property => SymbolKind::PROPERTY,
            Variant => SymbolKind::ENUM_MEMBER,
            Module => SymbolKind::MODULE,
        }
    }

    fn completion(self) -> CompletionItemKind {
        use SymKind::*;
        match self {
            Function | Method => CompletionItemKind::FUNCTION,
            Constructor => CompletionItemKind::CONSTRUCTOR,
            // No destructor kind exists; completed via filtering (see
            // `completes_as_expression`) — this arm is unreachable there.
            Destructor => CompletionItemKind::METHOD,
            Struct | Class | Enum | Trait => CompletionItemKind::STRUCT,
            Typedef | Distinct => CompletionItemKind::TYPE_PARAMETER,
            Constant => CompletionItemKind::CONSTANT,
            Variable | Parameter => CompletionItemKind::VARIABLE,
            Field | Property => CompletionItemKind::FIELD,
            Variant => CompletionItemKind::ENUM_MEMBER,
            Module => CompletionItemKind::MODULE,
        }
    }

    /// Whether a symbol may appear as an expression completion item.
    /// Constructors/destructors are outline-only (`Rect(...)` completes via
    /// the class snippet; `this.~C` is not syntax).
    fn completes_as_expression(self) -> bool {
        !matches!(self, SymKind::Constructor | SymKind::Destructor)
    }

    fn heading(self) -> &'static str {
        use SymKind::*;
        match self {
            Function | Method => "function/method",
            Constructor => "constructor",
            Destructor => "destructor",
            Struct => "struct",
            Class => "class",
            Enum => "enum",
            Trait => "trait",
            Typedef => "typedef",
            Distinct => "distinct",
            Constant => "constant",
            Variable | Parameter => "variable",
            Field => "field",
            Property => "property",
            Variant => "enum member",
            Module => "module",
        }
    }
}

/// A single named declaration.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymKind,
    /// Byte span of the identifier (used for goto-def, selection range).
    pub name_span: Span,
    /// Byte span of the whole declaration.
    pub full_span: Span,
    /// Human-readable signature/type shown in hover & completion.
    pub detail: String,
    /// Byte span of the enclosing scope (block/decl). `None` for top-level.
    pub scope: Option<Span>,
    /// Child symbols (struct fields, class members, enum variants).
    pub children: Vec<Symbol>,
    /// Declared type name for variables/parameters/fields/constants.
    /// Used for member completion (`p.` offers `Point`'s fields).
    pub ty: Option<String>,
    /// `(name, type)` pairs for function/method parameters. Used for
    /// snippet placeholders in call completions.
    pub params: Vec<(String, String)>,
    /// Superclass name for classes (`extends` target). Used for `super.`.
    pub parent: Option<String>,
    /// Trait names a class implements (`implements` list, base names).
    /// Used for `textDocument/implementation`.
    pub implements: Vec<String>,
    /// Doc comment (`///`) directly above the declaration. Ordinary `//`
    /// comments never populate this — see `hella_compiler::docs`.
    pub doc: Option<String>,
}

impl Symbol {
    fn contains(&self, offset: usize) -> bool {
        self.name_span.start <= offset && offset <= self.name_span.end
    }
    fn with_scope(mut self, scope: Span) -> Self {
        self.scope = Some(scope);
        self
    }
    fn with_ty(mut self, ty: &str) -> Self {
        self.ty = Some(ty.to_string());
        self
    }
    fn with_params(mut self, params: &[(String, String)]) -> Self {
        self.params = params.to_vec();
        self
    }
    fn with_parent(mut self, parent: &str) -> Self {
        self.parent = Some(parent.to_string());
        self
    }
    fn with_implements(mut self, traits: &[String]) -> Self {
        self.implements = traits.to_vec();
        self
    }
    fn kind_heading(&self) -> &'static str {
        self.kind.heading()
    }
}

/// Members added to a type by an `extend` block, pending link to the
/// target type's symbol.
#[derive(Debug, Clone)]
struct ExtensionInfo {
    target: String,
    members: Vec<Symbol>,
    span: Span,
    /// True once merged into a same-file (or imported) target's children.
    linked: bool,
}

/// Full-file analysis: top-level symbols plus all lexically scoped locals.
#[derive(Debug, Default, Clone)]
pub struct Analysis {
    top: Vec<Symbol>,
    locals: Vec<Symbol>,
    /// Top-level symbols of directly imported files (single level).
    imported: Vec<Symbol>,
    /// `extend` blocks by target type name (linked ones also merged into
    /// the target's children; see `link_extensions`).
    extensions: Vec<ExtensionInfo>,
}
impl Analysis {
    /// Build a symbol table from a successfully parsed program.
    pub fn from_program(prog: &ast::Program) -> Self {
        let mut a = Analysis::default();
        for item in &prog.items {
            a.collect_item(item);
        }
        a.link_extensions();
        a
    }

    /// Build a symbol table and attach `///` doc comments from `source`.
    /// Ordinary `//` comments are ignored (see `hella_compiler::docs`).
    pub fn from_program_with_source(prog: &ast::Program, source: &str) -> Self {
        let mut a = Self::from_program(prog);
        a.attach_docs(source);
        a
    }

    /// Attach `///` docs to every known symbol (top-level, children,
    /// locals) using the source they were parsed from. Idempotent: only
    /// fills in symbols that lack docs.
    pub fn attach_docs(&mut self, source: &str) {
        for sym in self.top.iter_mut() {
            attach_symbol_docs(sym, source);
        }
        for sym in self.locals.iter_mut() {
            // Locals rarely carry docs, but a `///` above a `var` still counts.
            if sym.doc.is_none() {
                sym.doc = hella_compiler::docs::extract_doc_comment(source, sym.full_span.start);
            }
        }
        for sym in self.imported.iter_mut() {
            if sym.doc.is_none() {
                sym.doc = hella_compiler::docs::extract_doc_comment(source, sym.full_span.start);
            }
            for child in sym.children.iter_mut() {
                attach_symbol_docs(child, source);
            }
        }
    }

    /// Merge extension members into their target type's children (cloned;
    /// the originals stay listed for span lookup). Marks merged entries.
    fn link_extensions(&mut self) {
        for ext in &mut self.extensions {
            if link_extension_into(&mut self.top, ext) {
                ext.linked = true;
            }
        }
    }

    fn push_top(&mut self, sym: Symbol, name_span: Span) {
        // Avoid duplicate imports colliding; keep first occurrence by name.
        if !self.top.iter().any(|s| s.name_span == name_span) {
            self.top.push(sym);
        }
    }
    /// Shared member builders used by both class and `extend` collection,
    /// so the two stay identical. Each pushes the member symbol and walks
    /// its bodies for locals.
    fn collect_field_sym(&mut self, f: &ast::StructField, children: &mut Vec<Symbol>) {
        children.push(
            make_symbol(
                &f.name,
                SymKind::Field,
                f.name_span,
                f.span,
                format!("{} {}", f.ty.name(), f.name),
                Vec::new(),
            )
            .with_ty(&f.ty.name()),
        );
    }

    fn collect_method_sym(&mut self, m: &ast::Function, children: &mut Vec<Symbol>) {
        let detail = format!(
            "fn {}({}) -> {}",
            m.name,
            format_params(&m.params),
            m.ret_ty.name()
        );
        children.push(
            make_symbol(
                &m.name,
                SymKind::Method,
                m.name_span,
                m.span,
                detail,
                Vec::new(),
            )
            .with_params(&param_pairs(&m.params)),
        );
        self.collect_block_locals(&m.body, m.span, &m.params);
    }

    fn collect_property_sym(&mut self, p: &ast::PropertyDecl, children: &mut Vec<Symbol>) {
        let ty = p
            .ty
            .as_ref()
            .map(|t| t.name())
            .unwrap_or_else(|| "any".to_string());
        children.push(
            make_symbol(
                &p.name,
                SymKind::Property,
                p.name_span,
                p.span,
                format!("{ty} {} (property)", p.name),
                Vec::new(),
            )
            .with_ty(&ty),
        );
        // Accessor bodies see the property scope; the setter
        // parameter is in scope for its body.
        if let Some(g) = &p.getter {
            self.collect_block(g, p.span);
        }
        if let Some((param, body)) = &p.setter {
            self.collect_params(std::slice::from_ref(param), p.span);
            self.collect_block(body, p.span);
        }
    }

    fn collect_ctor_sym(&mut self, k: &ast::ConstructorDecl, children: &mut Vec<Symbol>) {
        children.push(
            make_symbol(
                &k.name,
                SymKind::Constructor,
                k.name_span,
                k.span,
                format!("{}({})", k.name, format_params(&k.params)),
                Vec::new(),
            )
            .with_params(&param_pairs(&k.params)),
        );
        self.collect_params(&k.params, k.span);
        if let Some(b) = &k.body {
            self.collect_block(b, k.span);
        }
    }

    fn collect_dtor_sym(&mut self, d: &ast::DestructorDecl, children: &mut Vec<Symbol>) {
        children.push(make_symbol(
            &format!("~{}", d.name),
            SymKind::Destructor,
            d.name_span,
            d.span,
            format!("~{}()", d.name),
            Vec::new(),
        ));
        self.collect_block(&d.body, d.span);
    }

    fn collect_operator_body(&mut self, o: &ast::OperatorDecl) {
        self.collect_block_locals(&o.body, o.span, &o.params);
    }

    fn collect_conversion_body(&mut self, cv: &ast::ConversionDecl) {
        self.collect_block(&cv.body, cv.span);
    }

    /// Collect an `extend` block with the exact same member treatment as a
    /// class. Members are staged for linking into the target type; bodies
    /// are walked immediately so locals resolve inside extensions.
    fn collect_extension(&mut self, e: &ast::ExtensionDecl) {
        let mut members = Vec::new();
        for m in &e.members {
            match m {
                ast::ExtensionMember::Field(f) => self.collect_field_sym(f, &mut members),
                ast::ExtensionMember::Function(f) => self.collect_method_sym(f, &mut members),
                ast::ExtensionMember::Operator(o) => self.collect_operator_body(o),
                ast::ExtensionMember::Property(p) => self.collect_property_sym(p, &mut members),
                ast::ExtensionMember::Conversion(c) => self.collect_conversion_body(c),
            }
        }
        self.extensions.push(ExtensionInfo {
            target: e.ty.name(),
            members,
            span: e.span,
            linked: false,
        });
    }

fn collect_item(&mut self, item: &Item) {
        match item {
            Item::Function(f) => {
                let detail = format!(
                    "fn {}({}) -> {}",
                    f.name,
                    format_params(&f.params),
                    f.ret_ty.name()
                );
                let sym = make_symbol(
                    &f.name,
                    SymKind::Function,
                    f.name_span,
                    f.span,
                    detail,
                    Vec::new(),
                )
                .with_params(&param_pairs(&f.params));
                self.push_top(sym, f.name_span);
                self.collect_block_locals(&f.body, f.span, &f.params);
            }
            Item::Struct(s) => {
                let children = s
                    .fields
                    .iter()
                    .map(|f| {
                        make_symbol(
                            &f.name,
                            SymKind::Field,
                            f.name_span,
                            f.span,
                            format!("{} {}", f.ty.name(), f.name),
                            Vec::new(),
                        )
                        .with_ty(&f.ty.name())
                    })
                    .collect();
                let sym = make_symbol(
                    &s.name,
                    SymKind::Struct,
                    s.name_span,
                    s.span,
                    format!("struct {}", s.name),
                    children,
                );
                self.push_top(sym, s.name_span);
            }
            Item::Class(c) => {
                let mut children = Vec::new();
                for f in &c.fields {
                    self.collect_field_sym(f, &mut children);
                }
                for m in &c.methods {
                    self.collect_method_sym(m, &mut children);
                }
                for p in &c.properties {
                    self.collect_property_sym(p, &mut children);
                }
                for k in &c.constructors {
                    self.collect_ctor_sym(k, &mut children);
                }
                for d in &c.destructors {
                    self.collect_dtor_sym(d, &mut children);
                }
                for o in &c.operators {
                    self.collect_operator_body(o);
                }
                for cv in &c.conversions {
                    self.collect_conversion_body(cv);
                }
                // First constructor's parameters drive the call snippet on
                // the class name (`Rect(${1:int w})$0`).
                let ctor_params = c
                    .constructors
                    .first()
                    .map(|k| param_pairs(&k.params))
                    .unwrap_or_default();
                let mut sym = make_symbol(
                    &c.name,
                    SymKind::Class,
                    c.name_span,
                    c.span,
                    format!("class {}", c.name),
                    children,
                )
                .with_params(&ctor_params);
                if let Some(ext) = &c.extends {
                    sym = sym.with_parent(&ext.name());
                }
                let impls: Vec<String> = c
                    .implements
                    .iter()
                    .map(|t| base_type_name(&t.name()))
                    .collect();
                sym = sym.with_implements(&impls);
                self.push_top(sym, c.name_span);
            }
            Item::Enum(e) => {
                let children = e
                    .variants
                    .iter()
                    .map(|v| {
                        make_symbol(
                            &v.name,
                            SymKind::Variant,
                            v.name_span,
                            v.span,
                            format!("variant {}", v.name),
                            Vec::new(),
                        )
                    })
                    .collect();
                let sym = make_symbol(
                    &e.name,
                    SymKind::Enum,
                    e.name_span,
                    e.span,
                    format!("enum {}", e.name),
                    children,
                );
                self.push_top(sym, e.name_span);
            }
Item::Trait(t) => {
                let mut children = Vec::new();
                for m in &t.methods {
                    let detail = format!(
                        "fn {}({}) -> {}",
                        m.name,
                        format_params(&m.params),
                        m.ret_ty.name()
                    );
                    children.push(
                        make_symbol(
                            &m.name,
                            SymKind::Method,
                            m.name_span,
                            m.span,
                            detail,
                            Vec::new(),
                        )
                        .with_params(&param_pairs(&m.params)),
                    );
                    self.collect_params(&m.params, m.span);
                }
                let sym = make_symbol(
                    &t.name,
                    SymKind::Trait,
                    t.name_span,
                    t.span,
                    format!("trait {}", t.name),
                    children,
                );
                self.push_top(sym, t.name_span);
            }
            Item::Typedef(t) => {
                let sym = make_symbol(
                    &t.name,
                    SymKind::Typedef,
                    t.name_span,
                    t.span,
                    format!("typedef {} = {}", t.name, t.ty.name()),
                    Vec::new(),
                )
                .with_ty(&t.ty.name());
                self.push_top(sym, t.name_span);
            }
            Item::Distinct(d) => {
                let sym = make_symbol(
                    &d.name,
                    SymKind::Distinct,
                    d.name_span,
                    d.span,
                    format!("distinct {} = {}", d.name, d.ty.name()),
                    Vec::new(),
                )
                .with_ty(&d.ty.name());
                self.push_top(sym, d.name_span);
            }
            Item::Const(c) => {
                let ty = c.ty.as_ref().map(|t| t.name()).unwrap_or_else(|| "*".into());
                let sym = make_symbol(
                    &c.name,
                    SymKind::Constant,
                    c.name_span,
                    c.span,
                    format!("const {}: {}", c.name, ty),
                    Vec::new(),
                )
                .with_ty(&ty);
                self.push_top(sym, c.name_span);
            }
            Item::Var(v) => {
                let sym = make_symbol(
                    &v.name,
                    SymKind::Variable,
                    v.name_span,
                    v.span,
                    format!("var {}: {}", v.name, v.ty.name()),
                    Vec::new(),
                )
                .with_ty(&v.ty.name());
                self.push_top(sym, v.name_span);
            }
            Item::Import(imp) => {
                let name = imp.path.join("::");
                let sym = make_symbol(
                    &name,
                    SymKind::Module,
                    imp.path_span,
                    imp.span,
                    format!("module {}", name),
                    Vec::new(),
                );
                self.push_top(sym, imp.path_span);
            }
            Item::Extern(e) => {
                for m in &e.members {
                    match m {
                        ast::ExternMember::Function {
                            name,
                            name_span,
                            span,
                            params,
                            ..
                        } => {
                            let pairs: Vec<(String, String)> = params
                                .iter()
                                .map(|p| {
                                    if p.is_variadic && p.name.is_empty() {
                                        // bare C varargs `...`
                                        ("...".to_string(), String::new())
                                    } else {
                                        (p.name.clone(), p.ty.name())
                                    }
                                })
                                .collect();
                            let sym = make_symbol(
                                name,
                                SymKind::Function,
                                *name_span,
                                *span,
                                format!("extern {}", name),
                                Vec::new(),
                            )
                            .with_params(&pairs);
                            self.push_top(sym, *name_span);
                        }
                        ast::ExternMember::Struct { name, name_span, span, .. }
                        | ast::ExternMember::Enum { name, name_span, span, .. }
                        | ast::ExternMember::Const { name, name_span, span, .. } => {
                            let sym = make_symbol(
                                name,
                                SymKind::Function,
                                *name_span,
                                *span,
                                format!("extern {}", name),
                                Vec::new(),
                            );
                            self.push_top(sym, *name_span);
                        }
                    }
                }
            }
            Item::Init(b) => self.collect_block_locals(b, b.span, &[]),
            Item::Extension(e) => {
                self.collect_extension(e);
            }
            Item::Attributed { item, .. } => self.collect_item(item),
        }
    }

    fn collect_params(&mut self, params: &[ast::Param], scope: Span) {
        for p in params {
            let detail = format!("{} {}", p.ty.name(), p.name);
            let sym = make_symbol(
                &p.name,
                SymKind::Parameter,
                p.name_span,
                p.span,
                detail,
                Vec::new(),
            )
            .with_ty(&p.ty.name());
            self.locals.push(sym.with_scope(scope));
        }
    }

    fn collect_block_locals(&mut self, block: &ast::Block, scope: Span, params: &[ast::Param]) {
        self.collect_params(params, scope);
        self.collect_block(block, scope);
    }

    fn collect_block(&mut self, block: &ast::Block, scope: Span) {
        for stmt in &block.stmts {
            self.collect_stmt(stmt, scope);
        }
    }
fn collect_stmt(&mut self, stmt: &Stmt, scope: Span) {
        match stmt {
            Stmt::VarDecl(v) => {
                let detail = format!("var {}: {}", v.name, v.ty.name());
                let sym = make_symbol(
                    &v.name,
                    SymKind::Variable,
                    v.name_span,
                    v.span,
                    detail,
                    Vec::new(),
                )
                .with_ty(&v.ty.name())
                .with_scope(scope);
                self.locals.push(sym);
                if let Some(init) = &v.init {
                    self.collect_expr(init);
                }
            }
            Stmt::Const(c) => {
                let ty = c.ty.as_ref().map(|t| t.name()).unwrap_or_else(|| "*".into());
                let detail = format!("const {}: {}", c.name, ty);
                let sym = make_symbol(
                    &c.name,
                    SymKind::Constant,
                    c.name_span,
                    c.span,
                    detail,
                    Vec::new(),
                )
                .with_ty(&ty)
                .with_scope(scope);
                self.locals.push(sym);
                self.collect_expr(&c.init);
            }
            Stmt::Destructure(d) => {
                for t in &d.targets {
                    if let ast::DestructureTarget::Ident(name, nspan) = t {
                        let sym = make_symbol(
                            name,
                            SymKind::Variable,
                            *nspan,
                            *nspan,
                            name.clone(),
                            Vec::new(),
                        )
                        .with_scope(scope);
                        self.locals.push(sym);
                    }
                }
                self.collect_expr(&d.expr);
            }
            Stmt::If(s) => {
                self.collect_expr(&s.cond);
                self.collect_block(&s.then_block, scope);
                if let Some(b) = &s.else_block {
                    self.collect_block(b, scope);
                }
            }
            Stmt::While(s) => {
                self.collect_expr(&s.cond);
                self.collect_block(&s.body, scope);
            }
            Stmt::Loop(s) => self.collect_block(&s.body, scope),
            Stmt::For(s) => {
                let sym = make_symbol(
                    &s.var,
                    SymKind::Variable,
                    s.var_span,
                    s.span,
                    format!("for {}", s.var),
                    Vec::new(),
                )
                .with_scope(scope);
                self.locals.push(sym);
                if let Some((v2, s2)) = &s.var2 {
                    let sym2 = make_symbol(
                        v2,
                        SymKind::Variable,
                        *s2,
                        s.span,
                        format!("for {}", v2),
                        Vec::new(),
                    )
                    .with_scope(scope);
                    self.locals.push(sym2);
                }
                self.collect_expr(&s.iter);
                self.collect_block(&s.body, scope);
            }
            Stmt::Return(s) => {
                if let Some(v) = &s.value {
                    self.collect_expr(v);
                }
            }
            Stmt::Expr(e) => self.collect_expr(&e.expr),
            Stmt::Block(b) => self.collect_block(b, scope),
            Stmt::Assert(s) => {
                self.collect_expr(&s.cond);
                if let Some(m) = &s.message {
                    self.collect_expr(m);
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::Defer(d) => match &d.inner {
                ast::DeferInner::Expr(e) => self.collect_expr(e),
                ast::DeferInner::Block(b) => self.collect_block(b, scope),
            },
            Stmt::Delete(d) => self.collect_expr(&d.target),
        }
    }

    fn collect_expr(&mut self, expr: &ast::Expr) {
        match &expr.kind {
            ExprKind::IntLit(_)
            | ExprKind::FloatLit(_)
            | ExprKind::BoolLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::CharLit(_)
            | ExprKind::Ident(_)
            | ExprKind::This
            | ExprKind::Super
            | ExprKind::Null => {}
            ExprKind::InterpolatedString(parts, _) => {
                for p in parts {
                    if let ast::InterpolatedPart::Expr(e) = p {
                        self.collect_expr(e);
                    }
                }
            }
            ExprKind::Paren(e)
            | ExprKind::Unary { expr: e, .. }
            | ExprKind::Postfix { expr: e, .. } => self.collect_expr(e),
            ExprKind::Tuple(es) | ExprKind::ArrayLit(es) => {
                for e in es {
                    self.collect_expr(e);
                }
            }
            ExprKind::VecEmpty(_) => {}
            ExprKind::MapLit { entries, .. } => {
                for (k, v) in entries {
                    self.collect_expr(k);
                    self.collect_expr(v);
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.collect_expr(lhs);
                self.collect_expr(rhs);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.collect_expr(s);
                }
                if let Some(e) = end {
                    self.collect_expr(e);
                }
            }
            ExprKind::Assign { lhs, value } | ExprKind::CompoundAssign { lhs, value, .. } => {
                self.collect_expr(lhs);
                self.collect_expr(value);
            }
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                self.collect_expr(cond);
                self.collect_expr(then_branch);
                self.collect_expr(else_branch);
            }
            ExprKind::NullableMemberAccess { object, .. } | ExprKind::MemberAccess { object, .. } => {
                self.collect_expr(object);
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.collect_expr(object);
                self.collect_call_args(args);
            }
            ExprKind::Call { args, .. } => self.collect_call_args(args),
            ExprKind::New { args, .. } => self.collect_call_args(args),
            ExprKind::Index { object, index } => {
                self.collect_expr(object);
                self.collect_expr(index);
            }
            ExprKind::Slice { object, start, end, .. } => {
                self.collect_expr(object);
                if let Some(s) = start {
                    self.collect_expr(s);
                }
                if let Some(e) = end {
                    self.collect_expr(e);
                }
            }
            ExprKind::StructLit { fields, .. } => {
                for (_, _, e) in fields {
                    self.collect_expr(e);
                }
            }
            ExprKind::EnumVariant { args, .. } => self.collect_call_args(args),
            ExprKind::Match(m) => {
                self.collect_expr(&m.scrutinee);
                for arm in &m.arms {
                    if let Some(g) = &arm.guard {
                        self.collect_expr(g);
                    }
                    match &arm.body {
                        ast::MatchArmBody::Expr(e) => self.collect_expr(e),
                        ast::MatchArmBody::Block(b) => self.collect_block(b, arm.span),
                    }
                }
            }
            ExprKind::Closure { body, .. } => match body.as_ref() {
                ast::ClosureBody::Expr(e) => self.collect_expr(e),
                ast::ClosureBody::Block(b) => self.collect_block(b, b.span),
            },
        }
    }

    fn collect_call_args(&mut self, args: &[ast::CallArg]) {
        for a in args {
            match a {
                ast::CallArg::Expr(e) => self.collect_expr(e),
                ast::CallArg::Named { value, .. } => self.collect_expr(value),
                ast::CallArg::Ref { expr, .. } => self.collect_expr(expr),
                ast::CallArg::Out { .. } => {}
            }
        }
    }
// ── Queries ───────────────────────────────────────────────────────

    /// All top-level symbols (including imported modules, useful for
    /// completion but filtered out of document symbols).
    pub fn top_symbols(&self) -> &[Symbol] {
        &self.top
    }

    /// Symbols whose identifier span covers `offset` (innermost first):
    /// top-level/children plus any local in that scope.
    pub fn symbols_at(&self, offset: usize) -> Vec<&Symbol> {
        let mut out = Vec::new();
        collect_containing(&self.top, offset, &mut out);
        out.extend(
            self.locals
                .iter()
                .filter(|l| l.contains(offset) && scope_covers(l, offset)),
        );
        out.sort_by_key(|s| s.name_span.end - s.name_span.start);
        out
    }

    /// The innermost symbol at `offset`, if any.
    pub fn symbol_at(&self, offset: usize) -> Option<&Symbol> {
        self.symbols_at(offset).into_iter().next()
    }

    /// Find a callable (function/method/constructor) by name for signature
    /// help. Searches top-level then imported symbols; prefers items that
    /// actually declare parameters.
    pub fn lookup_callable(&self, name: &str) -> Option<Symbol> {
        let mut fallback: Option<Symbol> = None;
        for s in self
            .top_recursive()
            .into_iter()
            .chain(self.imported_recursive())
        {
            if s.name != name {
                continue;
            }
            let callable = matches!(
                s.kind,
                SymKind::Function | SymKind::Method | SymKind::Constructor
            );
            if !callable {
                continue;
            }
            if !s.params.is_empty() {
                return Some(s.clone());
            }
            fallback.get_or_insert_with(|| s.clone());
        }
        fallback
    }

    fn top_recursive(&self) -> Vec<&Symbol> {
        walk_symbols(&self.top)
    }

    fn imported_recursive(&self) -> Vec<&Symbol> {
        walk_symbols(&self.imported)
    }

    // ── LSP feature handlers ──────────────────────────────────────────

    pub fn hover(&self, source: &str, offset: usize) -> Option<Hover> {
        let sym = self.resolve_symbol_at(source, offset)?;
        let mut value = format!("**{}**\n\n```hll\n{}\n```", sym.kind_heading(), sym.detail);
        // `///` doc comment, when present. Ordinary `//` comments never
        // reach `Symbol::doc`, so they stay invisible here by construction.
        if let Some(doc) = sym.doc.as_deref() {
            let doc = doc.trim();
            if !doc.is_empty() {
                value.push_str("\n\n");
                value.push_str(doc);
            }
        }
        // `own` slots move on assignment and are destroyed at scope exit —
        // worth surfacing where the type is hovered.
        if sym.ty.as_deref().is_some_and(|t| t.strip_prefix("own ").is_some_and(|r| !r.is_empty())) {
            value.push_str("\n\n*heap-owned: moves on assignment, destroyed at scope exit (`delete` frees early).*");
        }
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(span_to_range(source, sym.name_span)),
        })
    }

    pub fn definition(
        &self,
        source: &str,
        uri: &lsp_types::Uri,
        offset: usize,
    ) -> Option<Location> {
        let span = self.resolve_definition_span(source, offset)?;
        Some(Location {
            uri: uri.clone(),
            range: span_to_range(source, span),
        })
    }

    /// Resolve the symbol the cursor is on: either the declaration itself
    /// (a symbol whose name covers `offset`) or a *reference* to some
    /// top-level/local/imported symbol whose name matches the word under
    /// the cursor. Powers hover (shows the referenced symbol's info) and
    /// goto-definition.
    fn resolve_symbol_at(&self, source: &str, offset: usize) -> Option<&Symbol> {
        // 1. Cursor on a declaration name.
        if let Some(sym) = self.symbol_at(offset) {
            return Some(sym);
        }
        // 2. Cursor on a reference: look up the word by name.
        let word = ident_at(source, offset)?;
        let local = innermost_local(&self.locals, &word, offset, false);
        if let Some(l) = local {
            return Some(l);
        }
        self.top_recursive()
            .into_iter()
            .chain(self.imported_recursive())
            .find(|s| s.name == word)
    }

    /// Span of the declaration the identifier at `offset` refers to.
    fn resolve_definition_span(&self, source: &str, offset: usize) -> Option<Span> {
        let word = ident_at(source, offset)?;
        if let Some(l) = innermost_local(&self.locals, &word, offset, false) {
            return Some(l.name_span);
        }
        self.top_recursive()
            .into_iter()
            .find(|s| s.name == word)
            .map(|s| s.name_span)
    }

    /// Anchor declaration for the identifier at `offset`: its name span
    /// plus the scope to narrow reference search to. Locals resolve
    /// inward (shadowing-safe); top-level/imported names are document-wide.
    /// Unresolved words get `(None, None)` — the whole-document fallback.
    fn anchor_for(&self, word: &str, offset: usize) -> (Option<Span>, Option<Span>) {
        if let Some(l) = innermost_local(&self.locals, word, offset, false) {
            return (Some(l.name_span), l.scope);
        }
        match self
            .top_recursive()
            .into_iter()
            .chain(self.imported_recursive())
            .find(|s| s.name == word)
        {
            Some(s) => (Some(s.name_span), s.scope),
            None => (None, None),
        }
    }

    /// All reference spans of the identifier at `offset` in this document.
    ///
    /// Lexer-driven (`Token::Ident` with equal slice), so strings and
    /// comments never count. Scoped to the anchor declaration when the
    /// word resolves to a local (shadowing-safe); whole-document otherwise,
    /// which also doubles as the fallback for unparseable buffers.
    pub fn reference_spans(
        &self,
        source: &str,
        offset: usize,
        include_declaration: bool,
    ) -> Vec<Span> {
        let Some(word) = ident_at(source, offset) else {
            return Vec::new();
        };
        let (decl, scope) = self.anchor_for(&word, offset);
        let lexed = hella_compiler::lexer::lex(source);
        let mut out = Vec::new();
        for t in &lexed.tokens {
            if t.token != Token::Ident {
                continue;
            }
            if t.span.end > source.len() || &source[t.span.start..t.span.end] != word {
                continue;
            }
            if let Some(scope) = scope {
                if t.span.start < scope.start || t.span.start > scope.end {
                    continue;
                }
            }
            if !include_declaration && decl == Some(t.span) {
                continue;
            }
            out.push(t.span);
        }
        out.sort_by_key(|s| s.start);
        out.dedup();
        out
    }

    /// Document highlights at `offset`: every reference span paired with
    /// whether it is the declaration (`true` = write, `false` = read).
    pub fn highlights(&self, source: &str, offset: usize) -> Vec<(Span, bool)> {
        let Some(word) = ident_at(source, offset) else {
            return Vec::new();
        };
        let (decl, _) = self.anchor_for(&word, offset);
        self.reference_spans(source, offset, true)
            .into_iter()
            .map(|s| {
                let is_decl = decl == Some(s);
                (s, is_decl)
            })
            .collect()
    }

    /// Byte span of the identifier under the cursor (for `prepareRename`).
    pub fn word_span_at(source: &str, offset: usize) -> Option<Span> {
        let bytes = source.as_bytes();
        let offset = offset.min(bytes.len());
        let mut lo = offset;
        while lo > 0 && (bytes[lo - 1].is_ascii_alphanumeric() || bytes[lo - 1] == b'_') {
            lo -= 1;
        }
        let mut hi = offset;
        while hi < bytes.len() && (bytes[hi].is_ascii_alphanumeric() || bytes[hi] == b'_') {
            hi += 1;
        }
        if lo == hi {
            return None;
        }
        Some(Span::new(lo, hi))
    }

    /// Whether `name` is a legal rename target: identifier shape, no
    /// leading digit, not a keyword.
    pub fn valid_ident(name: &str) -> bool {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$') {
            return false;
        }
        if name.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
            return false;
        }
        !KEYWORDS.contains(&name)
    }

    /// Fuzzy case-insensitive symbol search over top-level symbols
    /// (including members) for `workspace/symbol`. Empty queries match
    /// nothing; results are cloned so the server can mix open documents.
    pub fn search(&self, query: &str) -> Vec<Symbol> {
        if query.is_empty() {
            return Vec::new();
        }
        let q = query.to_lowercase();
        self.top_recursive()
            .into_iter()
            .filter(|s| s.kind != SymKind::Module && s.name.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }

    /// Resolve the declared type of the identifier at `offset` to its
    /// declaration span (`textDocument/typeDefinition`). Handles `own T`
    /// wrappers, generic/array suffixes, and cursor-on-the-type-name
    /// (which resolves to itself).
    pub fn type_definition_span(&self, source: &str, offset: usize) -> Option<Span> {
        let word = ident_at(source, offset)?;
        let sym = if let Some(l) = innermost_local(&self.locals, &word, offset, false) {
            l.clone()
        } else {
            self.top_recursive()
                .into_iter()
                .chain(self.imported_recursive())
                .find(|s| s.name == word)?
                .clone()
        };
        let ty_name = match &sym.ty {
            Some(t) => base_type_name(t),
            // Cursor is on the type name itself: a type resolves to itself.
            None if matches!(
                sym.kind,
                SymKind::Struct
                    | SymKind::Class
                    | SymKind::Enum
                    | SymKind::Trait
                    | SymKind::Typedef
                    | SymKind::Distinct
            ) =>
            {
                return Some(sym.name_span)
            }
            None => return None,
        };
        self.top_recursive()
            .into_iter()
            .chain(self.imported_recursive())
            .find(|s| {
                s.name == ty_name
                    && matches!(
                        s.kind,
                        SymKind::Struct
                            | SymKind::Class
                            | SymKind::Enum
                            | SymKind::Trait
                            | SymKind::Typedef
                            | SymKind::Distinct
                    )
            })
            .map(|s| s.name_span)
    }

    /// Implementation locations for the trait or trait method at `offset`
    /// (`textDocument/implementation`): implementing classes (trait name)
    /// or their same-named method declarations (trait method).
    pub fn implementation_spans(&self, source: &str, offset: usize) -> Vec<Span> {
        let Some(word) = ident_at(source, offset) else {
            return Vec::new();
        };
        // Find the enclosing trait when the cursor is on one of its methods.
        let mut trait_name: Option<String> = None;
        let mut method_name: Option<String> = None;
        for t in self.top_recursive() {
            if t.kind != SymKind::Trait {
                continue;
            }
            if t.name == word {
                trait_name = Some(t.name.clone());
                break;
            }
            if t.full_span.start <= offset
                && offset <= t.full_span.end
                && t.children.iter().any(|c| c.name == word)
            {
                trait_name = Some(t.name.clone());
                method_name = Some(word.clone());
                break;
            }
        }
        let Some(trait_name) = trait_name else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in self.top_recursive() {
            if c.kind != SymKind::Class || !c.implements.contains(&trait_name) {
                continue;
            }
            match &method_name {
                Some(m) => {
                    for child in &c.children {
                        if child.name == *m && child.kind == SymKind::Method {
                            out.push(child.name_span);
                        }
                    }
                }
                None => out.push(c.name_span),
            }
        }
        out.sort_by_key(|s| s.start);
        out
    }

    pub fn document_symbols(&self, source: &str) -> Vec<DocumentSymbol> {
        self.top
            .iter()
            .filter(|s| s.kind != SymKind::Module)
            .map(|s| to_document_symbol(source, s))
            .collect()
    }

    /// Full completion pipeline at `offset`.
    ///
    /// Mid-typing buffers often don't parse (`return p.|`), which would
    /// leave an empty symbol table. So this parses error-tolerantly: on
    /// failure it retries with a dummy identifier completing the member
    /// access at the cursor (`p.__hella_complete`), then queries at the
    /// adjusted offset.
    pub fn complete(source: &str, offset: usize, snippets: bool) -> Vec<CompletionItem> {
        Self::complete_with_path(source, None, offset, snippets)
    }

    /// Full completion pipeline with the document's filesystem path (for
    /// import resolution). See [`Analysis::complete`].
    pub fn complete_with_path(
        source: &str,
        doc_path: Option<&std::path::Path>,
        offset: usize,
        snippets: bool,
    ) -> Vec<CompletionItem> {
        let offset = offset.min(source.len());
        // Import paths need no parse at all — pure line/filesystem context.
        if let Some(ctx) = import_context(source, offset) {
            if let Some(items) = complete_import(&ctx, doc_path) {
                return items;
            }
        }
        let (analysis, effective, adjusted) = match Self::parse_for_completion(source, offset) {
            Some((prog, eff, adj)) => {
                let mut a = Self::from_program_with_source(&prog, &eff);
                if let Some(p) = doc_path {
                    a.load_imports(p, &prog);
                }
                (a, eff, adj)
            }
            None => (Self::default(), source.to_string(), offset),
        };
        analysis.completions_in(&effective, adjusted, snippets)
    }

    fn parsed(text: &str) -> Option<ast::Program> {
        let out = hella_compiler::lexer::lex(text);
        hella_compiler::parse::parse(out.tokens, text.to_string()).ok()
    }

    /// Tolerant parse chain for mid-typing buffers. Tries, in order: the
    /// source as-is; balanced unclosed blocks; dummy identifiers completing
    /// every line-final dangling dot (`return this.` → `this.__hella_complete`)
    /// and the member access at the cursor — each on the raw and balanced
    /// variants. Offsets are preserved (balancing appends at EOF; dummies
    /// remap the query offset), so the returned analysis lines up with the
    /// query position.
    fn parse_for_completion(
        source: &str,
        offset: usize,
    ) -> Option<(ast::Program, String, usize)> {
        let mut candidates: Vec<(String, usize)> = vec![(source.to_string(), offset)];
        let balanced = balance_ends(source);
        if balanced != source {
            candidates.push((balanced.clone(), offset));
        }
        // Repairs that need the cursor, applied to each base (deduped).
        let mut extra: Vec<(String, usize)> = Vec::new();
        for (text, adj) in candidates.clone() {
            if let Some((t, a)) = global_dot_dummy(&text, adj) {
                extra.push((t, a));
            }
            if let Some((t, a)) = dummy_completion_source(&text, adj) {
                extra.push((t, a));
            }
        }
        // Combined: buffer-wide dots first, then the cursor access.
        for (text, adj) in extra.clone() {
            if let Some((t, a)) = dummy_completion_source(&text, adj) {
                extra.push((t, a));
            }
        }
        candidates.extend(extra);
        // Parse each distinct text once (repairs often converge).
        let mut seen: Vec<&str> = Vec::new();
        for (text, adjusted) in &candidates {
            if seen.contains(&text.as_str()) {
                continue;
            }
            seen.push(text.as_str());
            if let Some(p) = Self::parsed(text) {
                return Some((p, text.clone(), *adjusted));
            }
        }
        None
    }

    /// Load top-level symbols from directly imported files so their names
    /// complete as globals (and their types resolve for member completion),
    /// with `///` docs attached from each imported file's own source.
    /// Single level only; failures are skipped silently.
    pub(crate) fn load_imports(&mut self, doc_path: &std::path::Path, prog: &ast::Program) {
        let bases = hella_compiler::modules::search_bases(doc_path);
        let mut decls: Vec<&ast::ImportDecl> = Vec::new();
        fn walk<'a>(items: &'a [Item], out: &mut Vec<&'a ast::ImportDecl>) {
            for item in items {
                match item {
                    Item::Import(d) => out.push(d),
                    Item::Attributed { item, .. } => walk(std::slice::from_ref(item.as_ref()), out),
                    _ => {}
                }
            }
        }
        walk(&prog.items, &mut decls);
        let mut seen: HashSet<std::path::PathBuf> = HashSet::new();
        for decl in decls {
            let resolved = match hella_compiler::modules::resolve_import(&decl.path, &bases) {
                Some(p) => p,
                None => continue,
            };
            if resolved == doc_path || !seen.insert(resolved.clone()) {
                continue;
            }
            let src = match std::fs::read_to_string(&resolved) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let sub = match Self::parsed(&src) {
                Some(p) => p,
                None => continue,
            };
            let wanted: Option<HashSet<&str>> = decl
                .symbols
                .as_ref()
                .map(|v| v.iter().map(|(s, _)| s.as_str()).collect());
            let mut sub_analysis = Self::from_program_with_source(&sub, &src);
            for s in std::mem::take(&mut sub_analysis.top) {
                if s.kind == SymKind::Module {
                    continue;
                }
                if let Some(w) = &wanted {
                    if !w.contains(s.name.as_str()) {
                        continue;
                    }
                }
                self.imported.push(s);
            }
            // Imported extensions apply to local targets too (mirroring
            // sema inlining): link what matches, keep the rest listed.
            // Sub-internal links already happened in `from_program`.
            for mut ext in std::mem::take(&mut sub_analysis.extensions) {
                if ext.linked
                    || link_extension_into(&mut self.top, &ext)
                    || link_extension_into(&mut self.imported, &ext)
                {
                    ext.linked = true;
                }
                self.extensions.push(ext);
            }
        }
    }

    /// Completion items at `offset` against an already-built analysis.
    ///
    /// - After `.` (`p.|`, `p.fo|`, `this.|`): member completion — fields,
    ///   methods and properties of the receiver's type, or enum variants
    ///   after a type name. Unresolvable receivers fall back to globals.
    /// - Elsewhere: in-scope locals (tier 0), top-level/nested symbols
    ///   (tier 1), keywords (tier 2).
    ///
    /// When `snippets` is set (client advertised `snippetSupport`),
    /// functions/methods complete to call snippets with tab-stop arguments
    /// and block keywords (`if`, `while`, …) complete to templates that
    /// include the closing `end`.
    fn completions_in(&self, source: &str, offset: usize, snippets: bool) -> Vec<CompletionItem> {
        if let Some((receiver, prefix)) = member_receiver(source, offset) {
            if let Some(items) = self.member_completions(&receiver, &prefix, offset, snippets) {
                return items;
            }
        }
        let prefix = word_prefix_at(source, offset).unwrap_or_default();
        let mut items = Vec::new();
        let matches = |name: &str| prefix.is_empty() || name.to_lowercase().starts_with(&prefix);
        // Locals in scope, declared before the cursor.
        for l in self
            .locals
            .iter()
            .filter(|l| scope_covers_prefix(l, offset))
        {
            if matches(&l.name) {
                items.push(symbol_item(l, 0, snippets));
            }
        }
        // Top-level + nested symbols (constructors/destructors are
        // outline-only, not expressions).
        for s in self.top_recursive() {
            if s.kind.completes_as_expression() && matches(&s.name) {
                items.push(symbol_item(s, 1, snippets));
            }
        }
        // Names from directly imported files (same tier: imports are
        // textually inlined before sema, so these are in-scope names).
        for s in self.imported_recursive() {
            if s.kind.completes_as_expression() && matches(&s.name) {
                items.push(symbol_item(s, 1, snippets));
            }
        }
        // Keywords (block openers become `end`-closing snippets).
        for kw in KEYWORDS {
            if matches(kw) {
                items.push(keyword_item(kw, snippets));
            }
        }
        sort_completions(&mut items);
        items
    }

    /// Member completion for a resolved receiver. `None` when the receiver
    /// cannot be resolved (caller falls back to global completion).
    fn member_completions(
        &self,
        receiver: &str,
        prefix: &str,
        offset: usize,
        snippets: bool,
    ) -> Option<Vec<CompletionItem>> {
        let members: Vec<&Symbol> = if receiver == "this" {
            if let Some(c) = self.enclosing_class(offset) {
                let mut out: Vec<&Symbol> = c.children.iter().collect();
                out.extend(self.unlinked_members(&c.name));
                out
            } else if let Some(ext) = self.enclosing_extension(offset) {
                self.type_members(&ext.target.clone())
            } else {
                return None;
            }
        } else if receiver == "super" {
            self.super_chain_members(offset)?
        } else if let Some(local) = innermost_local(&self.locals, receiver, offset, true) {
            let ty = local.ty.as_deref()?;
            let inner = ty.strip_prefix("own ").unwrap_or(ty);
            let resolved = self.resolve_named_type(inner)?;
            match resolved.kind {
                SymKind::Struct | SymKind::Class => {
                    let mut out: Vec<&Symbol> = resolved.children.iter().collect();
                    out.extend(self.unlinked_members(inner));
                    out
                }
                // Trait-typed value: the interface methods are the members.
                SymKind::Trait => resolved.children.iter().collect(),
                // Variants belong to the enum *type* (`Status.Ok`), not a value.
                _ => return None,
            }
        } else if let Some(ty) = self.resolve_named_type(receiver) {
            match ty.kind {
                // `Status.|` offers variants for `Status.Ok` construction.
                SymKind::Enum => ty.children.iter().collect(),
                _ => return None,
            }
        } else {
            return None;
        };
        let prefix = prefix.to_lowercase();
        let mut items: Vec<CompletionItem> = members
            .into_iter()
            .filter(|m| {
                m.kind.completes_as_expression()
                    && (prefix.is_empty() || m.name.to_lowercase().starts_with(&prefix))
            })
            .map(|m| symbol_item(m, 0, snippets))
            .collect();
        sort_completions(&mut items);
        Some(items)
    }

    /// Innermost class whose body contains `offset` (for `this.`).
    fn enclosing_class(&self, offset: usize) -> Option<&Symbol> {
        let mut best: Option<&Symbol> = None;
        let mut stack: Vec<&Symbol> = self.top.iter().collect();
        while let Some(s) = stack.pop() {
            if s.kind == SymKind::Class
                && s.full_span.start <= offset
                && offset <= s.full_span.end
            {
                // Prefer the innermost (nested classes are not in Hella, but
                // extensions/methods nest inside the class span).
                let narrower = match best {
                    Some(b) => {
                        (s.full_span.end - s.full_span.start) < (b.full_span.end - b.full_span.start)
                    }
                    None => true,
                };
                if narrower {
                    best = Some(s);
                }
            }
            stack.extend(s.children.iter());
        }
        best
    }

    /// Innermost `extend` block containing `offset` (for `this.` inside
    /// extensions, where no class span covers the cursor).
    fn enclosing_extension(&self, offset: usize) -> Option<&ExtensionInfo> {
        self.extensions
            .iter()
            .filter(|e| e.span.start <= offset && offset <= e.span.end)
            .min_by_key(|e| e.span.end - e.span.start)
    }

    /// Extension members that were not linked into a target's children.
    fn unlinked_members(&self, name: &str) -> Vec<&Symbol> {
        self.extensions
            .iter()
            .filter(|e| !e.linked && e.target == name)
            .flat_map(|e| e.members.iter())
            .collect()
    }

    /// Members of a named struct/class/enum type: declaration children
    /// (which already include linked extension members) plus unlinked
    /// extension members targeting it.
    fn type_members(&self, name: &str) -> Vec<&Symbol> {
        let mut out = Vec::new();
        if let Some(sym) = self.resolve_named_type(name) {
            if matches!(
                sym.kind,
                SymKind::Struct | SymKind::Class | SymKind::Enum
            ) {
                out.extend(sym.children.iter());
            }
        }
        out.extend(self.unlinked_members(name));
        out
    }

    /// Members visible through `super.`: walks the `extends` chain from the
    /// enclosing class (nearest first — stable sort + dedup keep the
    /// shadowing member), with a cycle guard and depth cap. `None` when
    /// there is no chain to resolve (caller falls back to globals).
    fn super_chain_members(&self, offset: usize) -> Option<Vec<&Symbol>> {
        let mut out: Vec<&Symbol> = Vec::new();
        let mut seen_types: HashSet<String> = HashSet::new();
        let mut next: Option<String> = self
            .enclosing_class(offset)
            .and_then(|c| c.parent.clone());
        let mut resolved_any = false;
        for _ in 0..8 {
            let name = match next {
                Some(n) => n,
                None => break,
            };
            if !seen_types.insert(name.clone()) {
                break;
            }
            let Some(sym) = self.resolve_named_type(&name) else {
                break;
            };
            resolved_any = true;
            out.extend(sym.children.iter());
            out.extend(self.unlinked_members(&name));
            next = sym.parent.clone();
        }
        if resolved_any { Some(out) } else { None }
    }

    /// Resolve a type name to its declaration (own file first, then
    /// imports), following `typedef`/`distinct` aliases (depth-capped).
    /// Traits resolve too, so trait-typed locals complete their methods.
    fn resolve_named_type(&self, name: &str) -> Option<&Symbol> {
        let mut current = name;
        for _ in 0..8 {
            let sym = self
                .top_recursive()
                .into_iter()
                .chain(self.imported_recursive())
                .find(|s| {
                    s.name == current
                        && matches!(
                            s.kind,
                            SymKind::Struct
                                | SymKind::Class
                                | SymKind::Enum
                                | SymKind::Trait
                                | SymKind::Typedef
                                | SymKind::Distinct
                        )
                })?;
            match sym.kind {
                SymKind::Typedef | SymKind::Distinct => {
                    current = sym.ty.as_deref()?;
                }
                _ => return Some(sym),
            }
        }
        None
    }
}
// ── Helpers ──────────────────────────────────────────────────────────

fn make_symbol(
    name: &str,
    kind: SymKind,
    name_span: Span,
    full_span: Span,
    detail: String,
    children: Vec<Symbol>,
) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind,
        name_span,
        full_span,
        detail,
        scope: None,
        children,
        ty: None,
        params: Vec::new(),
        parent: None,
        implements: Vec::new(),
        doc: None,
    }
}

/// Attach `///` docs to a symbol and its children from `source`.
fn attach_symbol_docs(sym: &mut Symbol, source: &str) {
    if sym.doc.is_none() {
        sym.doc = hella_compiler::docs::extract_doc_comment(source, sym.full_span.start);
    }
    for child in sym.children.iter_mut() {
        attach_symbol_docs(child, source);
    }
}

/// `(name, type-name)` pairs for snippet placeholders.
fn base_type_name(ty: &str) -> String {
    // `own Speaker` → `Speaker`; `Vec<int>` → `Vec`; `int[]` → `int`.
    let t = ty.strip_prefix("own ").unwrap_or(ty).trim();
    let end = t
        .find(|c: char| c == '<' || c == '[' || c == '(' || c == ':' || c.is_whitespace())
        .unwrap_or(t.len());
    t[..end].to_string()
}

/// `(name, type-name)` pairs for snippet placeholders.
fn param_pairs(params: &[ast::Param]) -> Vec<(String, String)> {    params
        .iter()
        .map(|p| (p.name.clone(), p.ty.name()))
        .collect()
}

fn format_params(params: &[ast::Param]) -> String {
    params
        .iter()
        .map(|p| {
            let mut s = String::new();
            if p.is_variadic {
                s.push_str("...");
            }
            s.push_str(&p.ty.name());
            s.push(' ');
            s.push_str(&p.name);
            s
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Merge an extension's members into the same-named struct/class target's
/// children. Returns true when a target matched.
fn link_extension_into(tops: &mut [Symbol], ext: &ExtensionInfo) -> bool {
    let mut linked = false;
    for top in tops.iter_mut() {
        if matches!(top.kind, SymKind::Struct | SymKind::Class) && top.name == ext.target {
            top.children.extend(ext.members.iter().cloned());
            linked = true;
        }
    }
    linked
}

/// All symbols in a list plus descendants (pre-order).
fn walk_symbols(syms: &[Symbol]) -> Vec<&Symbol> {
    fn walk<'a>(syms: &'a [Symbol], out: &mut Vec<&'a Symbol>) {
        for s in syms {
            out.push(s);
            walk(&s.children, out);
        }
    }
    let mut v = Vec::new();
    walk(syms, &mut v);
    v
}

fn collect_containing<'a>(syms: &'a [Symbol], offset: usize, out: &mut Vec<&'a Symbol>) {
    for s in syms {
        // Descend only into symbols whose *body* could contain the offset;
        // a symbol "matches" when the offset is on its name.
        if s.full_span.start <= offset && offset <= s.full_span.end {
            if s.name_span.start <= offset && offset <= s.name_span.end {
                out.push(s);
            }
            collect_containing(&s.children, offset, out);
        }
    }
}

/// Scope containment for symbol/hover/definition lookup: a local is visible
/// anywhere inside its enclosing scope (cursor may sit exactly on the decl).
fn scope_covers(l: &Symbol, offset: usize) -> bool {
    match l.scope {
        Some(scope) => scope.start <= offset && offset <= scope.end,
        None => true,
    }
}

/// Innermost visible declaration of a name. The old `min_by_key(name width)`
/// tiebreak was meaningless for same-name identifiers (equal widths always
/// tie, so the outermost declaration won); narrowest enclosing scope wins,
/// ties go to the latest declaration, so shadowing resolves inward.
fn innermost_local<'a>(
    locals: &'a [Symbol],
    name: &str,
    offset: usize,
    prefix: bool,
) -> Option<&'a Symbol> {
    locals
        .iter()
        .filter(|l| {
            l.name == name
                && if prefix {
                    scope_covers_prefix(l, offset)
                } else {
                    scope_covers(l, offset)
                }
        })
        .min_by(|a, b| {
            let wa = a.scope.map(|s| s.end - s.start).unwrap_or(usize::MAX);
            let wb = b.scope.map(|s| s.end - s.start).unwrap_or(usize::MAX);
            wa.cmp(&wb)
                .then_with(|| b.name_span.start.cmp(&a.name_span.start))
        })
}

/// Completion variant: additionally require the declaration to appear before
/// the cursor, so we don't offer a symbol in the middle of typing it.
fn scope_covers_prefix(l: &Symbol, offset: usize) -> bool {
    match l.scope {
        Some(scope) => scope.start <= offset && offset <= scope.end && l.name_span.end <= offset,
        None => true,
    }
}

fn to_document_symbol(source: &str, sym: &Symbol) -> DocumentSymbol {
    #[allow(deprecated)]
    DocumentSymbol {
        name: sym.name.clone(),
        detail: Some(sym.detail.clone()),
        kind: sym.kind.lsp(),
        tags: None,
        deprecated: None,
        range: span_to_range(source, sym.full_span),
        selection_range: span_to_range(source, sym.name_span),
        children: Some(
            sym.children
                .iter()
                .map(|c| to_document_symbol(source, c))
                .collect(),
        ),
    }
}

/// Sort by tier (`sort_text`) then label, and drop shadowed duplicates
/// (keeps the lowest-tier — most local — entry).
fn sort_completions(items: &mut Vec<CompletionItem>) {
    items.sort_by(|a, b| {
        (
            a.sort_text.as_deref().unwrap_or(""),
            a.label.as_str(),
        )
            .cmp(&(
                b.sort_text.as_deref().unwrap_or(""),
                b.label.as_str(),
            ))
    });
    items.dedup_by(|a, b| a.label == b.label);
}

/// `name(${1:Type arg}, …)$0` call snippet from parameter pairs.
fn call_snippet(name: &str, params: &[(String, String)]) -> String {
    if params.is_empty() {
        return format!("{name}()$0");
    }
    let args = params
        .iter()
        .enumerate()
        .map(|(i, (n, t))| {
            let hint = if t.is_empty() {
                n.clone()
            } else {
                format!("{t} {n}")
            };
            format!("${{{}:{hint}}}", i + 1)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({args})$0")
}

fn symbol_item(sym: &Symbol, tier: u8, snippets: bool) -> CompletionItem {
    let mut item = CompletionItem {
        label: sym.name.clone(),
        kind: Some(sym.kind.completion()),
        detail: Some(sym.detail.clone()),
        sort_text: Some(format!("{tier}_{}", sym.name)),
        documentation: sym.doc.as_deref().map(|d| {
            lsp_types::Documentation::MarkupContent(lsp_types::MarkupContent {
                kind: lsp_types::MarkupKind::Markdown,
                value: d.to_string(),
            })
        }),
        ..Default::default()
    };
    // Functions/methods always snippet to calls; types snippet only when
    // they carry constructor parameters (`Rect(${1:int w})$0`). Properties
    // complete as plain names (`c.count`, never `c.count()`).
    if snippets
        && (matches!(sym.kind, SymKind::Function | SymKind::Method) || !sym.params.is_empty())
    {
        item.insert_text = Some(call_snippet(&sym.name, &sym.params));
        item.insert_text_format = Some(InsertTextFormat::SNIPPET);
    }
    item
}

/// Block keywords that open a `do…end` / `has…end` body, with the snippet
/// template (including the closing `end`) and a short detail.
const BLOCK_SNIPPETS: &[(&str, &str, &str)] = &[
    ("if", "if ${1:condition} do\n    $0\nend", "if … do … end"),
    ("while", "while ${1:condition} do\n    $0\nend", "while … do … end"),
    ("for", "for ${1:x} in ${2:iter} do\n    $0\nend", "for … in … do … end"),
    ("loop", "loop do\n    $0\nend", "loop … end"),
    ("match", "match ${1:expr} do\n    ${2:_} -> $0\nend", "match … do … end"),
    ("do", "do\n    $0\nend", "do … end"),
    ("struct", "struct ${1:Name} has\n    $0\nend", "struct … has … end"),
    ("class", "class ${1:Name} has\n    $0\nend", "class … has … end"),
    ("init", "init do\n    $0\nend", "init … end"),
];

fn keyword_item(kw: &str, snippets: bool) -> CompletionItem {
    if snippets {
        if let Some((_, snippet, detail)) = BLOCK_SNIPPETS.iter().find(|(k, _, _)| *k == kw) {
            return CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some((*detail).to_string()),
                insert_text: Some((*snippet).to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: Some(format!("2_{kw}")),
                ..Default::default()
            };
        }
    }
    CompletionItem {
        label: kw.to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        detail: Some(format!("keyword {kw}")),
        sort_text: Some(format!("2_{kw}")),
        ..Default::default()
    }
}

/// Dummy identifier used to repair incomplete member access for parsing.
const COMPLETE_DUMMY: &str = "__hella_complete";

/// Repair every line-final dangling dot in the buffer (`return this.` →
/// `return this.__hella_complete`), so one half-typed access doesn't sink
/// completion elsewhere in the file. Insertions inside strings/comments are
/// harmless (they stay inside the literal). Returns the new source and the
/// remapped query offset.
fn global_dot_dummy(source: &str, offset: usize) -> Option<(String, usize)> {
    let mut result = String::with_capacity(source.len() + 32);
    let mut last = 0;
    let mut pos = 0; // byte offset of the current line start
    let mut adjusted = offset;
    let mut any = false;
    for line in source.split_inclusive('\n') {
        let line_end = pos + line.len();
        let body = line.strip_suffix('\n').unwrap_or(line);
        let body = body.strip_suffix('\r').unwrap_or(body);
        let stripped = body.trim_end_matches([' ', '\t']);
        if stripped.ends_with('.') && !stripped.ends_with("..") {
            // Byte index of the dot: `stripped` ends with ASCII `.`.
            let dot = pos + stripped.len() - 1;
            let insert_at = dot + 1;
            result.push_str(&source[last..insert_at]);
            result.push_str(COMPLETE_DUMMY);
            last = insert_at;
            if insert_at < offset {
                adjusted += COMPLETE_DUMMY.len();
            }
            any = true;
        }
        pos = line_end;
    }
    if !any {
        return None;
    }
    result.push_str(&source[last..]);
    Some((result, adjusted))
}

/// Retry source for incomplete member access: replaces the partial member
/// name (or inserts) at the cursor with a dummy identifier so the buffer
/// parses (`return p.fo|` → `return p.__hella_complete`). Returns the new
/// source and the adjusted query offset (right after the dot).
fn dummy_completion_source(source: &str, offset: usize) -> Option<(String, usize)> {
    if !source.is_char_boundary(offset) {
        return None;
    }
    let bytes = source.as_bytes();
    let mut lo = offset;
    while lo > 0 && (bytes[lo - 1].is_ascii_alphanumeric() || bytes[lo - 1] == b'_') {
        lo -= 1;
    }
    if lo == 0 || bytes[lo - 1] != b'.' {
        return None;
    }
    let mut text = String::with_capacity(source.len() + COMPLETE_DUMMY.len());
    text.push_str(&source[..lo]);
    text.push_str(COMPLETE_DUMMY);
    text.push_str(&source[offset..]);
    Some((text, lo))
}

/// Append missing `end`s for unclosed `do`/`has` blocks so mid-typing
/// buffers (a new method without its closing ends yet) still parse for
/// completion. Counts real lexer tokens, so strings and comments cannot
/// confuse the depth, and only appends at EOF — existing offsets are
/// preserved. Capped and returned unchanged when already balanced.
fn balance_ends(source: &str) -> String {
    let out = hella_compiler::lexer::lex(source);
    let mut depth: i32 = 0;
    for t in &out.tokens {
        match &t.token {
            Token::Do | Token::Has => depth += 1,
            Token::End => depth -= 1,
            _ => {}
        }
    }
    if depth <= 0 {
        return source.to_string();
    }
    let mut text = source.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    for _ in 0..depth.min(24) {
        text.push_str("end\n");
    }
    text
}

/// Import-line completion context: `import std::i|` (path segments plus a
/// partial name) or `import std::io::{pr|` (names exported by a module).
enum ImportCtx {
    Path { segments: Vec<String>, prefix: String },
    Members { module: Vec<String>, prefix: String },
}

/// Detect an `import` line at the cursor. Purely line-based, so it works in
/// buffers that don't parse. Returns `None` for non-import lines (caller
/// falls through to normal completion).
fn import_context(source: &str, offset: usize) -> Option<ImportCtx> {
    let line_start = source[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = source[line_start..offset].trim_start();
    let rest = line.strip_prefix("import")?;
    if !rest.is_empty() && !(rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    let rest = rest.trim_start();
    if let Some((mod_part, inner)) = rest.split_once('{') {
        if inner.contains('}') {
            return None; // cursor past the selector
        }
        let module = mod_part
            .split("::")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if module.is_empty() {
            return None;
        }
        let prefix = inner
            .rsplit(',')
            .next()
            .unwrap_or("")
            .trim_start()
            .to_string();
        if !is_word_frag(&prefix) {
            return None;
        }
        return Some(ImportCtx::Members { module, prefix });
    }
    // `import std::io::|` — a trailing separator (or nothing yet) means an
    // empty prefix inside that directory.
    let trailing_sep =
        rest.is_empty() || rest.ends_with("::") || rest.ends_with(' ') || rest.ends_with('\t');
    let mut segments: Vec<String> = rest
        .split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let prefix = if trailing_sep {
        String::new()
    } else {
        segments.pop().unwrap_or_default()
    };
    if !is_word_frag(&prefix)
        || !segments.iter().all(|s| !s.is_empty() && is_word_frag(s))
    {
        return None;
    }
    Some(ImportCtx::Path { segments, prefix })
}

fn is_word_frag(s: &str) -> bool {
    s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// A directory is offered for `import` completion only when selecting it
/// would resolve (`net.hll` or `net/mod.hll` must exist).
fn is_importable_dir(parent: &std::path::Path, name: &str) -> bool {
    parent.join(name).join("mod.hll").is_file()
        || parent.join(format!("{name}.hll")).is_file()
}

/// Complete an import context against the filesystem / module contents.
/// `None` means "cannot handle" (e.g. untitled buffer with no path) and the
/// caller falls through to normal completion.
fn complete_import(
    ctx: &ImportCtx,
    doc_path: Option<&std::path::Path>,
) -> Option<Vec<CompletionItem>> {
    let doc_path = doc_path?;
    let bases = hella_compiler::modules::search_bases(doc_path);
    match ctx {
        ImportCtx::Path { segments, prefix } => {
            let needle = prefix.to_lowercase();
            let mut items = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for base in &bases {
                let dir: std::path::PathBuf =
                    segments.iter().fold(base.clone(), |p, s| p.join(s));
                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') || !seen.insert(name.clone()) {
                        continue;
                    }
                    let matches =
                        needle.is_empty() || name.to_lowercase().starts_with(&needle);
                    if !matches {
                        continue;
                    }
                    let ft = match entry.file_type() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if ft.is_dir() {
                        if matches!(name.as_str(), "target" | "out" | "node_modules") {
                            continue;
                        }
                        // Offer every directory for navigation (`net::http`
                        // is reachable only through `net`, which itself may
                        // not be directly importable); the detail marks
                        // which ones select as modules.
                        let full: Vec<String> = segments
                            .iter()
                            .chain(std::iter::once(&name))
                            .cloned()
                            .collect();
                        let detail = if is_importable_dir(&dir, &name) {
                            format!("module {}", full.join("::"))
                        } else {
                            "directory".to_string()
                        };
                        items.push(CompletionItem {
                            label: name.clone(),
                            kind: Some(CompletionItemKind::FOLDER),
                            detail: Some(detail),
                            sort_text: Some(name),
                            ..Default::default()
                        });
                    } else if ft.is_file() {
                        let Some(stem) = name.strip_suffix(".hll") else {
                            continue;
                        };
                        if entry.path() == doc_path {
                            continue; // no self-imports
                        }
                        items.push(CompletionItem {
                            label: stem.to_string(),
                            kind: Some(CompletionItemKind::FILE),
                            detail: Some(format!(
                                "module {}",
                                segments
                                    .iter()
                                    .cloned()
                                    .chain(std::iter::once(stem.to_string()))
                                    .collect::<Vec<_>>()
                                    .join("::")
                            )),
                            sort_text: Some(stem.to_string()),
                            ..Default::default()
                        });
                    }
                }
            }
            items.sort_by(|a, b| a.label.cmp(&b.label));
            Some(items)
        }
        ImportCtx::Members { module, prefix } => {
            let resolved = hella_compiler::modules::resolve_import(module, &bases)?;
            let src = std::fs::read_to_string(&resolved).ok()?;
            let sub = Analysis::parsed(&src)?;
            let owner = Analysis::from_program(&sub);
            let needle = prefix.to_lowercase();
            let mut items: Vec<CompletionItem> = walk_symbols(&owner.top)
                .into_iter()
                .filter(|s| {
                    s.kind != SymKind::Module
                        && (needle.is_empty() || s.name.to_lowercase().starts_with(&needle))
                })
                .map(|s| CompletionItem {
                    label: s.name.clone(),
                    kind: Some(s.kind.completion()),
                    detail: Some(s.detail.clone()),
                    sort_text: Some(s.name.clone()),
                    ..Default::default()
                })
                .collect();
            items.sort_by(|a, b| a.label.cmp(&b.label));
            items.dedup_by(|a, b| a.label == b.label);
            Some(items)
        }
    }
}

/// Member-access context: `receiver.|`, `receiver.pre|`. Returns the receiver
/// word and the partial member prefix. Bails (→ global completion) for
/// non-word receivers (`foo().|`, `a[i].|`, ranges).
fn member_receiver(source: &str, offset: usize) -> Option<(String, String)> {
    let bytes = source.as_bytes();
    if offset > bytes.len() {
        return None;
    }
    // Partial member name after the dot (empty for `p.|`).
    let mut lo = offset;
    while lo > 0 && (bytes[lo - 1].is_ascii_alphanumeric() || bytes[lo - 1] == b'_') {
        lo -= 1;
    }
    let prefix = source[lo..offset].to_string();
    if lo == 0 || bytes[lo - 1] != b'.' {
        return None;
    }
    // Receiver word before the dot (tolerating spaces: `p .|`).
    let mut end = lo - 1;
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    if start == end {
        return None;
    }
    let receiver = source[start..end].to_string();
    // Numeric literal (`3.14`), not a receiver.
    if receiver.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((receiver, prefix))
}

/// The identifier/keyword at `offset`, if any. Works for a cursor anywhere
/// on/inside/at-either-edge-of a word by expanding over identifier
/// characters, rather than relying on token span boundaries (which are
/// contiguous, so an inclusive end-match on the previous token would
/// otherwise shadow the word's first character).
fn ident_at(source: &str, offset: usize) -> Option<String> {
    let bytes = source.as_bytes();
    if offset > bytes.len() {
        return None;
    }
    let mut lo = offset;
    while lo > 0 && (bytes[lo - 1].is_ascii_alphanumeric() || bytes[lo - 1] == b'_') {
        lo -= 1;
    }
    let mut hi = offset;
    while hi < bytes.len() && (bytes[hi].is_ascii_alphanumeric() || bytes[hi] == b'_') {
        hi += 1;
    }
    if lo == hi {
        return None;
    }
    Some(source[lo..hi].to_string())
}

/// Lowercased word prefix ending at `offset`, or `None` when not in a word.
pub fn word_prefix_for_completion(source: &str, offset: usize) -> Option<String> {
    word_prefix_at(source, offset)
}

fn word_prefix_at(source: &str, offset: usize) -> Option<String> {
    let bytes = source.as_bytes();
    let mut lo = offset;
    while lo > 0 && (bytes[lo - 1].is_ascii_alphanumeric() || bytes[lo - 1] == b'_') {
        lo -= 1;
    }
    let prefix = source[lo..offset].to_lowercase();
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Hella keywords offered in completion.
const KEYWORDS: &[&str] = &[
    "and", "any", "assert", "bool", "break", "char", "class", "const", "continue",
    "debug_assert", "defer", "distinct", "do", "double", "else", "end", "enum",
    "extends", "extend", "explicit", "extern", "float", "for", "function", "from",
    "get", "has", "if", "import", "implements", "in", "init", "initialize", "int",
    "is", "loop", "match", "not", "null", "open", "operator", "or", "out",
    "override", "private", "public", "ref", "return", "sealed", "set", "static",
    "string", "struct", "super", "this", "to", "trait", "typedef", "var", "void",
    "where", "while",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(src: &str) -> Analysis {
        let out = hella_compiler::lexer::lex(src);
        let prog = hella_compiler::parse::parse(out.tokens, src.to_string()).unwrap();
        Analysis::from_program(&prog)
    }

    #[test]
    fn collects_top_level() {
        let a = analyze("void main() do\n  int x = 1\nend\nint foo(int a) do\n  return a\nend");
        let names: Vec<_> = a.top_symbols().iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"foo"));
    }

    #[test]
    fn collects_locals() {
        let a = analyze("void main() do\n  int x = 1\n  print(x)\nend");
        assert!(a.locals.iter().any(|l| l.name == "x"));
        let a = analyze("int foo(int a) do\n  bool b = true\n  return a\nend");
        assert!(a.locals.iter().any(|l| l.name == "a"));
        assert!(a.locals.iter().any(|l| l.name == "b"));
    }

    #[test]
    fn finds_symbol_at() {
        let src = "void main() do\n  int x = 1\nend\nint foo(int a) do\n  return a\nend";
        let a = analyze(src);
        let x_pos = src.find("int x").unwrap() + 4;
        assert_eq!(a.symbol_at(x_pos).map(|s| s.name.as_str()), Some("x"));
        let foo_name = src.find("foo").unwrap();
        assert_eq!(a.symbol_at(foo_name).map(|s| s.name.as_str()), Some("foo"));
    }

    #[test]
    fn references_local_with_and_without_declaration() {
        let src = "void main() do\n  int x = 1\n  int y = x + x\nend";
        let a = analyze(src);
        let use_off = src.find("x + x").unwrap();
        assert_eq!(a.reference_spans(src, use_off, true).len(), 3); // decl + 2 uses
        assert_eq!(a.reference_spans(src, use_off, false).len(), 2); // uses only
    }

    #[test]
    fn references_top_level_function() {
        let src = "int twice(int x) do\n  return x * 2\nend\nvoid main() do\n  int a = twice(1)\n  int b = twice(2)\nend";
        let a = analyze(src);
        let off = src.find("twice(1)").unwrap();
        assert_eq!(a.reference_spans(src, off, true).len(), 3); // decl + 2 calls
        assert_eq!(a.reference_spans(src, off, false).len(), 2);
    }

    #[test]
    fn references_skip_strings_and_comments() {
        let src = "void main() do\n  int x = 1\n  // x mentions nothing\n  string s = \"x\"\n  int y = x\nend";
        let a = analyze(src);
        let decl_off = src.find("int x").unwrap() + 4;
        assert_eq!(a.reference_spans(src, decl_off, true).len(), 2); // decl + real use
    }

    #[test]
    fn references_respect_shadowing() {
        let src = "int f(int x) do\n  return x\nend\nint g(int x) do\n  return x\nend\nvoid main() do\nend";
        let a = analyze(src);
        let first_use = src.find("return x").unwrap() + 7;
        // f's param decl + f's use only — g's pair is a different binding.
        assert_eq!(a.reference_spans(src, first_use, true).len(), 2);
    }

    #[test]
    fn references_empty_off_word() {
        let src = "void main() do\n  int x = 1\nend";
        let a = analyze(src);
        let space = src.find(" main").unwrap(); // whitespace before `main`
        assert!(a.reference_spans(src, space, true).is_empty());
        assert!(a.reference_spans(src, src.len(), true).is_empty()); // past end
    }

    #[test]
    fn highlights_mark_declaration_write() {
        let src = "void main() do\n  int x = 1\n  int y = x\nend";
        let a = analyze(src);
        let use_off = src.find("int y = x").unwrap() + 8;
        let hl = a.highlights(src, use_off);
        assert_eq!(hl.len(), 2); // decl + use
        assert_eq!(hl.iter().filter(|(_, w)| *w).count(), 1); // exactly one write
        let decl_off = src.find("int x").unwrap() + 4;
        let decl_span = Analysis::word_span_at(src, decl_off).unwrap();
        assert!(hl.iter().any(|(s, w)| *s == decl_span && *w));
    }

    #[test]
    fn rename_validation() {
        assert!(Analysis::valid_ident("newName"));
        assert!(Analysis::valid_ident("_x1"));
        assert!(!Analysis::valid_ident(""));
        assert!(!Analysis::valid_ident("1abc"));
        assert!(!Analysis::valid_ident("has space"));
        assert!(!Analysis::valid_ident("if")); // keyword
        assert!(!Analysis::valid_ident("end")); // keyword
    }

    #[test]
    fn workspace_search_finds_members() {
        let src = "struct Point has\n  int x\nend\nint distance(int a) do\n  return a\nend\nvoid main() do\nend";
        let a = analyze(src);
        let hits: Vec<_> = a.search("point").iter().map(|s| s.name.clone()).collect();
        assert!(hits.contains(&"Point".to_string()), "struct: {hits:?}");
        let field: Vec<_> = a.search("DIST").iter().map(|s| s.name.clone()).collect();
        assert!(field.contains(&"distance".to_string()), "case-insensitive fn: {field:?}");
        assert!(a.search("").is_empty());
        assert!(a.search("zzz_no_match").is_empty());
    }

    #[test]
    fn type_definition_var_to_struct() {
        let src = "struct Point has\n  int x\nend\nvoid main() do\n  Point p = has x = 1 end\nend";
        let a = analyze(src);
        let use_off = src.find("Point p").unwrap();
        let span = a.type_definition_span(src, use_off).expect("type def");
        let decl_off = src.find("struct Point").unwrap() + 7;
        assert_eq!(span, Analysis::word_span_at(src, decl_off).unwrap());
    }

    #[test]
    fn type_definition_own_trait() {
        let src = "trait Speaker has\n  string speak()\nend\nopen class Cat implements Speaker has\n  public string speak() do\n    return \"m\"\n  end\nend\nvoid main() do\n  own Speaker s = new Cat()\nend";
        let a = analyze(src);
        let use_off = src.find("Speaker s").unwrap();
        let span = a.type_definition_span(src, use_off).expect("type def");
        let decl_off = src.find("trait Speaker").unwrap() + 6;
        assert_eq!(span, Analysis::word_span_at(src, decl_off).unwrap());
    }

    #[test]
    fn type_definition_on_type_name_resolves_itself() {
        let src = "struct Point has\n  int x\nend\nvoid main() do\nend";
        let a = analyze(src);
        let off = src.find("Point").unwrap();
        let span = a.type_definition_span(src, off).expect("self");
        assert_eq!(span, Analysis::word_span_at(src, off).unwrap());
    }

    #[test]
    fn type_definition_unknown_returns_none() {
        let src = "void main() do\n  int x = 1\nend";
        let a = analyze(src);
        let off = src.find("int x").unwrap() + 4; // `x: int` is primitive
        assert_eq!(a.type_definition_span(src, off), None);
    }

    #[test]
    fn implementations_of_trait() {
        let src = "trait Speaker has\n  string speak()\nend\nopen class Cat implements Speaker has\n  public string speak() do\n    return \"m\"\n  end\nend\nopen class Dog implements Speaker has\n  public string speak() do\n    return \"w\"\n  end\nend\nvoid main() do\nend";
        let a = analyze(src);
        let off = src.find("trait Speaker").unwrap() + 6;
        let impls = a.implementation_spans(src, off);
        assert_eq!(impls.len(), 2, "Cat + Dog: {impls:?}");
    }

    #[test]
    fn implementations_of_trait_method() {
        let src = "trait Speaker has\n  string speak()\nend\nopen class Cat implements Speaker has\n  public string speak() do\n    return \"m\"\n  end\nend\nvoid main() do\nend";
        let a = analyze(src);
        let off = src.find("string speak()").unwrap() + 7; // trait's method
        let impls = a.implementation_spans(src, off);
        assert_eq!(impls.len(), 1, "Cat::speak: {impls:?}");
        // ...and it points at the class method, not the class name.
        let cat_method = src.find("public string speak()").unwrap() + 15;
        assert_eq!(impls[0], Analysis::word_span_at(src, cat_method).unwrap());
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn completion_tiers_locals_first() {
        // `main` shadows nothing here; local `x` (tier 0) sorts before
        // top-level `xray` (tier 1) and keywords (tier 2).
        let src = "int xray() do\n  return 1\nend\nvoid main() do\n  int x = 1\n  x\nend";
        let off = src.rfind("\n  x\n").unwrap() + 3;
        let items = Analysis::complete(src, off, false);
        let names = labels(&items);
        assert!(names.contains(&"x"));
        assert!(names.contains(&"xray"));
        let pos_x = names.iter().position(|n| *n == "x").unwrap();
        let pos_kw = names.iter().position(|n| *n == "if").unwrap();
        assert!(pos_x < pos_kw, "locals sort before keywords: {names:?}");
        // plain mode: no snippet payloads
        assert!(items.iter().all(|i| i.insert_text.is_none()));
    }

    #[test]
    fn completion_function_snippets() {
        let src = "int add(int a, int b) do\n  return a\nend\nvoid main() do\n  ad\nend";
        let off = src.find("\n  ad").unwrap() + 4;
        let items = Analysis::complete(src, off, true);
        let add = items.iter().find(|i| i.label == "add").unwrap();
        assert_eq!(
            add.insert_text.as_deref(),
            Some("add(${1:int a}, ${2:int b})$0")
        );
        assert_eq!(
            add.insert_text_format,
            Some(lsp_types::InsertTextFormat::SNIPPET)
        );
        // without snippet support: plain label, no insert text
        let plain = Analysis::complete(src, off, false);
        let add_plain = plain.iter().find(|i| i.label == "add").unwrap();
        assert!(add_plain.insert_text.is_none());
    }

    #[test]
    fn completion_block_snippet_closes_end() {
        let src = "void main() do\n  i\nend";
        let off = src.find("\n  i").unwrap() + 3;
        let items = Analysis::complete(src, off, true);
        let if_ = items.iter().find(|i| i.label == "if").unwrap();
        let text = if_.insert_text.as_deref().unwrap();
        assert!(text.contains("\nend"), "block snippet must close with end: {text:?}");
        assert!(text.contains("$0"), "block snippet needs final tab stop");
        assert_eq!(
            if_.insert_text_format,
            Some(lsp_types::InsertTextFormat::SNIPPET)
        );
    }

    #[test]
    fn completion_member_after_dot() {
        // NB: `return p.` does not parse — completion retries with a dummy
        // identifier, exactly like a mid-typing editor buffer.
        let src = "struct Point has\n  int x\n  int y\nend\nint dist(Point p) do\n  return p.\nend";
        let off = src.find("p.").unwrap() + 2;
        let items = Analysis::complete(src, off, false);
        let names = labels(&items);
        assert!(names.contains(&"x"), "struct fields offered: {names:?}");
        assert!(names.contains(&"y"));
        assert!(!names.contains(&"if"), "no keywords in member position");
        assert!(!names.contains(&"dist"), "no globals in member position");
        // with prefix filtering
        let src2 = "struct Point has\n  int x\n  int y\nend\nint dist(Point p) do\n  return p.y\nend";
        let off2 = src2.find("p.y").unwrap() + 3;
        let items2 = Analysis::complete(src2, off2, false);
        assert_eq!(labels(&items2), vec!["y"]);
    }

    #[test]
    fn completion_this_and_enum_type() {
        let src = "class C has\n  int n\n  int fetch() do\n    return this.\nend\nend";
        let this_off = src.find("this.").unwrap() + 5;
        let items = Analysis::complete(src, this_off, false);
        let names = labels(&items);
        assert!(names.contains(&"n"), "this. offers fields: {names:?}");
        assert!(names.contains(&"fetch"), "this. offers methods: {names:?}");
        let src_enum = "enum S has\n  Ok\n  Err\nend\nS s() do\n  return S.\nend";
        let enum_off = src_enum.find("S.").unwrap() + 2;
        let enum_items = Analysis::complete(src_enum, enum_off, false);
        let enum_names = labels(&enum_items);
        assert!(enum_names.contains(&"Ok"), "Type. offers variants: {enum_names:?}");
        assert!(enum_names.contains(&"Err"));
        // unknown receiver falls back to globals
        let src2 = "void main() do\n  nosuch.\nend";
        let off2 = src2.find("nosuch.").unwrap() + 7;
        let fallback = Analysis::complete(src2, off2, false);
        assert!(labels(&fallback).contains(&"if"), "fallback offers keywords");
    }

    #[test]
    fn completion_this_in_unclosed_class() {
        // Mid-typing state: the class has no closing `end`s yet. Completion
        // must still see the fields/methods typed so far.
        let src = "class C has\n  int n\n  int fetch() do\n    return this.";
        let off = src.len();
        let items = Analysis::complete(src, off, false);
        let names = labels(&items);
        assert!(names.contains(&"n"), "this. offers fields: {names:?}");
        assert!(names.contains(&"fetch"), "this. offers methods: {names:?}");
    }

    /// Scratch project: `<dir>/main.hll` (the open document) plus an
    /// importable `util.hll` and a `net/http.hll` submodule.
    fn scratch_import_project() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hella-lsp-import-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("net")).unwrap();
        std::fs::write(
            dir.join("util.hll"),
            "int twice(int x) do\n    return x * 2\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("net/http.hll"),
            "int get(string url) do\n    return 0\nend\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.hll"), "").unwrap();
        dir
    }

    #[test]
    fn completion_import_paths() {
        let dir = scratch_import_project();
        let main = dir.join("main.hll");
        // `import |` offers sibling modules and subdirs.
        let items = Analysis::complete_with_path("import ", Some(&main), 7, false);
        let names = labels(&items);
        assert!(names.contains(&"util"), "sibling module: {names:?}");
        assert!(names.contains(&"net"), "submodule dir: {names:?}");
        assert!(!names.contains(&"main"), "no self-import: {names:?}");
        // prefix filtering
        let items = Analysis::complete_with_path("import uti", Some(&main), 10, false);
        assert_eq!(labels(&items), vec!["util"]);
        // `import net::|` descends
        let items = Analysis::complete_with_path("import net::", Some(&main), 12, false);
        assert_eq!(labels(&items), vec!["http"]);
        // `import net::h` filters
        let items = Analysis::complete_with_path("import net::h", Some(&main), 13, false);
        assert_eq!(labels(&items), vec!["http"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_import_braces_and_names() {
        let dir = scratch_import_project();
        let main = dir.join("main.hll");
        // `import util::{t|` offers the module's names.
        let src = "import util::{t";
        let items = Analysis::complete_with_path(src, Some(&main), src.len(), false);
        assert_eq!(labels(&items), vec!["twice"]);
        // names from imports complete as globals in code.
        let src2 = "import util\n\nvoid main() do\n  tw\nend\n";
        let off2 = src2.find("\n  tw").unwrap() + 4;
        let items2 = Analysis::complete_with_path(src2, Some(&main), off2, false);
        assert!(
            labels(&items2).contains(&"twice"),
            "imported name completes: {:?}",
            labels(&items2)
        );
        // ... and through member completion on imported types.
        std::fs::write(
            dir.join("shapes.hll"),
            "struct Circle has\n  int r\nend\n",
        )
        .unwrap();
        let src3 = "import shapes\n\nvoid main() do\n  Circle c = has\n    r = 1\n  end\n  c.\nend\n";
        let off3 = src3.find("c.").unwrap() + 2;
        let items3 = Analysis::complete_with_path(src3, Some(&main), off3, false);
        assert_eq!(labels(&items3), vec!["r"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_property_members() {
        let src = "class Counter has\n  private int _count\n  int count get do\n    return this._count\n  end\n  int count set(int v) do\n    this._count = v\n  end\nend";
        let off = src.find("this._count").unwrap() + 5;
        let items = Analysis::complete(src, off, true);
        let names = labels(&items);
        assert!(names.contains(&"_count"), "fields: {names:?}");
        assert!(names.contains(&"count"), "properties: {names:?}");
        // properties complete as plain names — never call snippets
        let prop = items.iter().find(|i| i.label == "count").unwrap();
        assert!(prop.insert_text.is_none(), "no parens on properties");
    }

    #[test]
    fn completion_constructor_snippet() {
        let src = "class Rect has\n  public int w\n  Rect(int w) initialize\nend\nvoid main() do\n  Re\nend";
        let off = src.find("\n  Re").unwrap() + 4;
        let items = Analysis::complete(src, off, true);
        let rect = items.iter().find(|i| i.label == "Rect").unwrap();
        assert_eq!(
            rect.insert_text.as_deref(),
            Some("Rect(${1:int w})$0"),
            "ctor call snippet"
        );
        // structs (struct-literal init, no call syntax) get no snippet
        let src2 = "struct Point has\n  int x\nend\nvoid main() do\n  Po\nend";
        let off2 = src2.find("\n  Po").unwrap() + 4;
        let items2 = Analysis::complete(src2, off2, true);
        let point = items2.iter().find(|i| i.label == "Point").unwrap();
        assert!(point.insert_text.is_none());
    }

    #[test]
    fn completion_ctor_name_global() {
        let src = "class Counter has\n    private int _count\n    int count get do\n        return this._count\n    end\n    int count set(int v) do\n        this._count = v\n    end\n    Counter(int c) initialize\nend\nvoid main() do\n    Counter\nend\n";
        // sanity: it parses
        let out = hella_compiler::lexer::lex(src);
        assert!(out.errors.is_empty());
        let prog = hella_compiler::parse::parse(out.tokens, src.to_string()).unwrap();
        let a = Analysis::from_program(&prog);
        assert!(a.top_symbols().iter().any(|s| s.name == "Counter"));
        let off = src.find("\n    Counter\n").unwrap() + 12;
        assert_eq!(&src[off - 7..off], "Counter");
        let items = Analysis::complete(src, off, true);
        assert!(
            items.iter().any(|i| i.label == "Counter"),
            "class name completes: {:?}",
            labels(&items)
        );
    }

    #[test]
    fn completion_destructor_body_locals() {
        let src = "class C has\n  int n\n  ~C() do\n    int x = n\n    x\n  end\nend";
        let off = src.rfind("\n    x").unwrap() + 6; // right after `x`
        let items = Analysis::complete(src, off, false);
        assert_eq!(labels(&items), vec!["x"], "local filtered by prefix");
        // fields are visible in the same scope with an empty prefix
        let off2 = src.find("do\n").unwrap() + 3;
        let items2 = Analysis::complete(src, off2, false);
        let names2 = labels(&items2);
        assert!(names2.contains(&"n"), "fields visible in dtor: {names2:?}");
        assert!(!names2.contains(&"x"), "later local not visible: {names2:?}");
    }

    #[test]
    fn completion_constructor_body_params() {
        let src = "class C has\n  int n\n  C(int c) initialize do\n    this.n = c\n  end\nend";
        let off = src.find("= c").unwrap() + 2;
        let items = Analysis::complete(src, off, false);
        let names = labels(&items);
        assert!(names.contains(&"c"), "ctor params: {names:?}");
        assert!(names.contains(&"n"), "fields visible in ctor: {names:?}");
    }

    #[test]
    fn completion_this_in_extension() {
        let src = "struct Point has\n  int x\nend\nextend Point do\n  int doubled() do\n    return this.\n  end\nend";
        let off = src.find("this.").unwrap() + 5;
        let items = Analysis::complete(src, off, false);
        let names = labels(&items);
        assert!(names.contains(&"x"), "target fields: {names:?}");
        assert!(names.contains(&"doubled"), "extension methods: {names:?}");
    }

    #[test]
    fn completion_extension_body_locals() {
        let src = "struct Point has\n  int x\nend\nextend Point do\n  int doubled() do\n    int two = 2\n    two\n  end\nend";
        let off = src.rfind("\n    two").unwrap() + 7;
        let items = Analysis::complete(src, off, false);
        assert_eq!(labels(&items), vec!["two"]);
    }

    #[test]
    fn completion_unlinked_extension() {
        // Target declared nowhere: members still resolve by target name.
        let src = "extend Elsewhere do\n  int helper() do\n    return this.\n  end\nend";
        let off = src.find("this.").unwrap() + 5;
        let items = Analysis::complete(src, off, false);
        assert_eq!(labels(&items), vec!["helper"]);
    }

    #[test]
    fn completion_super_chain() {
        let src = "class A has\n  int a\n  int who() do\n    return 1\n  end\nend\nclass B extends A has\n  int b\n  int who() do\n    return 2\n  end\nend\nclass C extends B has\n  int c\n  int test() do\n    return super.\n  end\nend";
        let off = src.find("super.").unwrap() + 6;
        let items = Analysis::complete(src, off, false);
        let names = labels(&items);
        assert!(names.contains(&"a"), "grandparent members: {names:?}");
        assert!(names.contains(&"b"), "parent members: {names:?}");
        assert!(names.contains(&"who"), "parent methods: {names:?}");
        assert!(!names.contains(&"c"), "own members excluded: {names:?}");
        assert!(!names.contains(&"test"), "own methods excluded: {names:?}");
        // `super` without a chain falls back to globals
        let src2 = "class Solo has\n  int n\n  int f() do\n    return super.\n  end\nend";
        let off2 = src2.find("super.").unwrap() + 6;
        let fallback = Analysis::complete(src2, off2, false);
        assert!(labels(&fallback).contains(&"if"), "fallback offers keywords");
    }

    #[test]
    fn extension_members_merge_into_target_outline() {
        let src = "struct Point has\n  int x\nend\nextend Point do\n  int doubled() do\n    return this.x\n  end\nend";
        let out = hella_compiler::lexer::lex(src);
        let prog = hella_compiler::parse::parse(out.tokens, src.to_string()).unwrap();
        let a = Analysis::from_program(&prog);
        let point = a
            .top_symbols()
            .iter()
            .find(|s| s.name == "Point")
            .unwrap();
        let names: Vec<_> = point.children.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"x"), "own fields: {names:?}");
        assert!(names.contains(&"doubled"), "extension methods: {names:?}");
        let symbols = a.document_symbols(src);
        let point_sym = symbols.iter().find(|s| s.name == "Point").unwrap();
        #[allow(deprecated)]
        let kids: Vec<_> = point_sym
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(kids.contains(&"doubled"), "outline merged: {kids:?}");
    }

    #[test]
    fn trait_methods_collected() {
        let src = "trait Drawable has\n  void draw()\n  int area(int scale)\nend";
        let a = analyze(src);
        let t = a.top_symbols().iter().find(|s| s.name == "Drawable").unwrap();
        let names: Vec<_> = t.children.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"draw"), "trait methods: {names:?}");
        assert!(names.contains(&"area"), "trait methods: {names:?}");
        // outline + hover/goto resolve through the child symbol
        let symbols = a.document_symbols(src);
        let t_sym = symbols.iter().find(|s| s.name == "Drawable").unwrap();
        #[allow(deprecated)]
        let kids: Vec<_> = t_sym
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(kids.contains(&"draw"), "outline: {kids:?}");
        let draw_off = src.find("draw").unwrap() + 1;
        assert_eq!(
            a.symbol_at(draw_off).map(|s| s.name.as_str()),
            Some("draw")
        );
        // call snippet from trait method params (fresh line: empty prefix)
        let src2 = format!("{src}\n  ");
        let items = Analysis::complete(&src2, src2.len(), true);
        let area = items.iter().find(|i| i.label == "area").unwrap();
        assert_eq!(
            area.insert_text.as_deref(),
            Some("area(${1:int scale})$0")
        );
    }

    #[test]
    fn completion_trait_typed_local() {
        let src = "trait Raf has\n  string nnn()\nend\nopen class User implements Raf has\n  public string nnn() do\n    return \"x\"\n  end\nend\nvoid taker(Raf r) do\n  r.\nend";
        let off = src.find("r.").unwrap() + 2;
        let items = Analysis::complete(src, off, false);
        assert_eq!(labels(&items), vec!["nnn"]);
    }


    #[test]
    fn definition_resolves_local_and_toppub() {
        let src = "void main() do\n  int x = 1\n  print(x)\nend\nint foo() do\n  return 1\nend\nvoid bar() do\n  foo()\nend";
        let a = analyze(src);
        // `print(x)` — the `x` argument is 6 bytes into the match ("print(").
        let x_off = src.find("print(x)").unwrap() + 6;
        assert_eq!(src.as_bytes()[x_off], b'x');
        assert_eq!(
            a.resolve_definition_span(src, x_off),
            Some(Span::new(
                src.find("int x").unwrap() + 4,
                src.find("int x").unwrap() + 5,
            ))
        );
        // `foo()` inside bar resolves to foo's declaration name_span.
        let foo_use = src.rfind("foo()").unwrap(); // the call inside bar
        assert_eq!(a.resolve_definition_span(src, foo_use).map(|s| &src[s.start..s.end]), Some("foo"));
    }

    #[test]
    fn shadowed_local_resolves_inward() {
        let src = "void main() do\n  int x = 1\n  if true do\n    string x = \"s\"\n    print(x)\n  end\nend\n";
        let a = analyze(src);
        let inner_decl = src.find("string x").unwrap() + 7;
        let use_off = src.find("print(x)").unwrap() + 6;
        assert_eq!(src.as_bytes()[use_off], b'x');
        assert_eq!(
            a.resolve_definition_span(src, use_off),
            Some(Span::new(inner_decl, inner_decl + 1)),
            "shadowed use should resolve to the inner declaration"
        );
    }

    #[test]
    fn hover_own_notes_ownership() {
        let src = "open class User has\n  User() initialize\nend\nvoid main() do\n  own User a = new User()\n  print(a)\nend\n";
        let a = analyze(src);
        let use_off = src.find("print(a)").unwrap() + 6;
        let hover = a.hover(src, use_off).expect("hover resolves");
        let HoverContents::Markup(m) = &hover.contents else {
            panic!("expected markup hover");
        };
        assert!(m.value.contains("own User"), "type shown, got: {}", m.value);
        assert!(m.value.contains("heap-owned"), "ownership note, got: {}", m.value);
    }

    fn analyze_with_docs(src: &str) -> Analysis {
        let out = hella_compiler::lexer::lex(src);
        let prog = hella_compiler::parse::parse(out.tokens, src.to_string()).unwrap();
        Analysis::from_program_with_source(&prog, src)
    }

    fn hover_text(a: &Analysis, src: &str, offset: usize) -> String {
        let hover = a.hover(src, offset).expect("hover resolves");
        let HoverContents::Markup(m) = &hover.contents else {
            panic!("expected markup hover");
        };
        m.value.clone()
    }

    #[test]
    fn hover_shows_doc_comment() {
        let src = "/// Prints hello.\nvoid print(string s) do\nend\nvoid main() do\n  print(\"x\")\nend";
        let a = analyze_with_docs(src);
        let use_off = src.find("print(\"x\")").unwrap();
        let text = hover_text(&a, src, use_off);
        assert!(text.contains("Prints hello."), "doc shown, got: {text}");
    }

    #[test]
    fn hover_hides_normal_comment() {
        let src = "// Prints hello.\nvoid print(string s) do\nend\nvoid main() do\n  print(\"x\")\nend";
        let a = analyze_with_docs(src);
        let use_off = src.find("print(\"x\")").unwrap();
        let text = hover_text(&a, src, use_off);
        assert!(!text.contains("Prints hello."), "normal // hidden, got: {text}");
    }

    #[test]
    fn hover_imported_doc_resolves() {
        let dir = scratch_import_project();
        std::fs::write(
            dir.join("util.hll"),
            "/// Doubles `x`.\nint twice(int x) do\n    return x * 2\nend\n",
        )
        .unwrap();
        let main = dir.join("main.hll");
        let src = "import util\n\nvoid main() do\n  twice(1)\nend\n";
        std::fs::write(&main, src).unwrap();
        let out = hella_compiler::lexer::lex(src);
        let prog = hella_compiler::parse::parse(out.tokens, src.to_string()).unwrap();
        let mut a = Analysis::from_program_with_source(&prog, src);
        a.load_imports(&main, &prog);
        let use_off = src.find("twice(1)").unwrap();
        let text = hover_text(&a, src, use_off);
        assert!(text.contains("Doubles `x`."), "imported doc shown, got: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_own_trait_upcast_members() {
        let src = "trait Speaker has\n  string speak()\nend\nopen class Cat implements Speaker has\n  Cat() initialize\n  public string speak() do\n    return \"m\"\n  end\nend\nvoid main() do\n  own Speaker s = new Cat()\n  s.\nend\n";
        let off = src.find("s.").unwrap() + 2;
        let items = Analysis::complete(src, off, false);
        assert_eq!(labels(&items), vec!["speak"]);
    }
}
