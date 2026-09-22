//! AST traversal for lint rules. Exhaustive matches keep new syntax visible.
use crate::ast::*;

pub(super) fn program(p: &Program, visit: &mut impl FnMut(&Expr)) {
    for i in &p.items {
        item(i, visit);
    }
}

/// Crate-visible: visit every expression in a single function body (used by
/// the async reachability analysis, Async-8).
pub(crate) fn function_exprs(f: &Function, v: &mut impl FnMut(&Expr)) {
    function(f, v);
}

/// Crate-visible: visit every expression in a block (Async-8 roots).
pub(crate) fn block_exprs(b: &Block, v: &mut impl FnMut(&Expr)) {
    block(b, v);
}

/// Crate-visible: visit a single expression and its children (used by
/// the selective-import dependency closure, modules.rs).
pub(crate) fn exprs(e: &Expr, v: &mut impl FnMut(&Expr)) {
    expr(e, v);
}

/// Crate-visible: visit every *named type* in an item (used by the
/// selective-import dependency closure, modules.rs, which must keep
/// user-defined types a kept symbol references — e.g. a `Tm` local
/// inside an imported function). Pushes base names (`a::B` → `B`);
/// single-uppercase generic params never match item names downstream.
/// Skips `where`-clause bounds (no stdlib case needs them yet).
pub(crate) fn item_named_types(i: &Item, v: &mut impl FnMut(&str)) {
    item_types(i, v);
}
fn ty(t: &Type, v: &mut impl FnMut(&str)) {
    match t {
        Type::Named(n, _) => v(n.rsplit("::").next().unwrap_or(n)),
        Type::Generic(b, args, _) => {
            v(b.rsplit("::").next().unwrap_or(b));
            for a in args {
                ty(a, v);
            }
        }
        Type::FunctionType(r, ps, _) => {
            ty(r, v);
            for p in ps {
                ty(p, v);
            }
        }
        Type::Tuple(ts, _) => {
            for t in ts {
                ty(t, v);
            }
        }
        Type::Array(e, _)
        | Type::Vec { elem: e, .. }
        | Type::Pointer(e, _)
        | Type::Optional(e, _)
        | Type::Own(e, _)
        | Type::Task(e, _) => ty(e, v),
        Type::FixedArray { elem: e, .. } => ty(e, v),
        Type::Map { key: k, value: val, .. } => {
            ty(k, v);
            ty(val, v);
        }
        Type::Int(_)
        | Type::Bool(_)
        | Type::Void(_)
        | Type::String(_)
        | Type::Char(_)
        | Type::Float(_)
        | Type::Double(_)
        | Type::Any(_) => {}
    }
}
fn params_types(ps: &[Param], v: &mut impl FnMut(&str)) {
    for p in ps {
        ty(&p.ty, v);
    }
}
fn function_types(f: &Function, v: &mut impl FnMut(&str)) {
    ty(&f.ret_ty, v);
    params_types(&f.params, v);
    block_types(&f.body, v);
}
fn block_types(b: &Block, v: &mut impl FnMut(&str)) {
    for s in &b.stmts {
        match s {
            Stmt::VarDecl(d) => ty(&d.ty, v),
            Stmt::Const(d) => {
                if let Some(t) = &d.ty {
                    ty(t, v);
                }
            }
            Stmt::Block(b) => block_types(b, v),
            Stmt::If(i) => {
                block_types(&i.then_block, v);
                if let Some(b) = &i.else_block {
                    block_types(b, v);
                }
            }
            Stmt::While(w) => block_types(&w.body, v),
            Stmt::Loop(l) => block_types(&l.body, v),
            Stmt::For(f) => block_types(&f.body, v),
            Stmt::Defer(d) => match &d.inner {
                DeferInner::Block(b) => block_types(b, v),
                DeferInner::Expr(_) => {}
            },
            Stmt::Scope(b) => block_types(b, v),
            Stmt::Destructure(_)
            | Stmt::Expr(_)
            | Stmt::Return(_)
            | Stmt::Delete(_)
            | Stmt::Assert(_)
            | Stmt::Yield(_)
            | Stmt::Break(_)
            | Stmt::Continue(_) => {}
        }
    }
}
fn property_types(p: &PropertyDecl, v: &mut impl FnMut(&str)) {
    if let Some(t) = &p.ty {
        ty(t, v);
    }
    if let Some(b) = &p.getter {
        block_types(b, v);
    }
    if let Some((pm, b)) = &p.setter {
        ty(&pm.ty, v);
        block_types(b, v);
    }
}
fn fields_types(fs: &[StructField], v: &mut impl FnMut(&str)) {
    for f in fs {
        ty(&f.ty, v);
    }
}
fn item_types(i: &Item, v: &mut impl FnMut(&str)) {
    match i {
        Item::Function(f) => function_types(f, v),
        Item::Init(b) => block_types(b, v),
        Item::Var(d) => ty(&d.ty, v),
        Item::Const(d) => {
            if let Some(t) = &d.ty {
                ty(t, v);
            }
        }
        Item::Attributed { item: i, .. } => item_types(i, v),
        Item::Struct(s) => fields_types(&s.fields, v),
        Item::Class(c) => {
            fields_types(&c.fields, v);
            for f in &c.methods {
                function_types(f, v);
            }
            for c in &c.constructors {
                params_types(&c.params, v);
                if let Some(b) = &c.body {
                    block_types(b, v);
                }
            }
            for d in &c.destructors {
                block_types(&d.body, v);
            }
            for p in &c.properties {
                property_types(p, v);
            }
            for o in &c.operators {
                params_types(&o.params, v);
                block_types(&o.body, v);
            }
            for c in &c.conversions {
                ty(&c.from_ty, v);
                ty(&c.to_ty, v);
                block_types(&c.body, v);
            }
        }
        Item::Extension(e) => {
            for m in &e.members {
                match m {
                    ExtensionMember::Function(f) => function_types(f, v),
                    ExtensionMember::Field(f) => ty(&f.ty, v),
                    ExtensionMember::Property(p) => property_types(p, v),
                    ExtensionMember::Operator(o) => {
                        params_types(&o.params, v);
                        block_types(&o.body, v);
                    }
                    ExtensionMember::Conversion(c) => {
                        ty(&c.from_ty, v);
                        ty(&c.to_ty, v);
                        block_types(&c.body, v);
                    }
                }
            }
        }
        Item::Enum(e) => {
            for a in &e.variants {
                params_types(&a.payload_params, v);
            }
        }
        Item::Trait(t) => {
            for m in &t.methods {
                ty(&m.ret_ty, v);
                params_types(&m.params, v);
            }
        }
        Item::Typedef(t) => ty(&t.ty, v),
        Item::Distinct(d) => ty(&d.ty, v),
        Item::Import(_) | Item::Extern(_) => {}
    }
}
fn params(ps: &[Param], v: &mut impl FnMut(&Expr)) {
    for p in ps {
        if let Some(e) = &p.default {
            expr(e, v);
        }
    }
}
fn function(f: &Function, v: &mut impl FnMut(&Expr)) {
    params(&f.params, v);
    block(&f.body, v);
}
fn property(p: &PropertyDecl, v: &mut impl FnMut(&Expr)) {
    if let Some(b) = &p.getter {
        block(b, v);
    }
    if let Some((_, b)) = &p.setter {
        block(b, v);
    }
}
fn fields(fs: &[StructField], v: &mut impl FnMut(&Expr)) {
    for f in fs {
        if let Some(e) = &f.default {
            expr(e, v);
        }
    }
}
fn item(i: &Item, v: &mut impl FnMut(&Expr)) {
    match i {
        Item::Function(f) => function(f, v),
        Item::Init(b) => block(b, v),
        Item::Var(d) => {
            if let Some(e) = &d.init {
                expr(e, v);
            }
        }
        Item::Const(d) => {
            expr(&d.init, v);
        }
        Item::Attributed { item: i, .. } => item(i, v),
        Item::Struct(s) => fields(&s.fields, v),
        Item::Class(c) => {
            fields(&c.fields, v);
            for f in &c.methods {
                function(f, v);
            }
            for c in &c.constructors {
                params(&c.params, v);
                if let Some(b) = &c.body {
                    block(b, v);
                }
            }
            for d in &c.destructors {
                block(&d.body, v);
            }
            for p in &c.properties {
                property(p, v);
            }
            for o in &c.operators {
                params(&o.params, v);
                block(&o.body, v);
            }
            for c in &c.conversions {
                block(&c.body, v);
            }
        }
        Item::Extension(e) => {
            for m in &e.members {
                match m {
                    ExtensionMember::Function(f) => function(f, v),
                    ExtensionMember::Field(f) => {
                        if let Some(e) = &f.default {
                            expr(e, v);
                        }
                    }
                    ExtensionMember::Property(p) => property(p, v),
                    ExtensionMember::Operator(o) => {
                        params(&o.params, v);
                        block(&o.body, v);
                    }
                    ExtensionMember::Conversion(c) => block(&c.body, v),
                }
            }
        }
        Item::Enum(e) => {
            for a in &e.variants {
                params(&a.payload_params, v);
                if let Some(e) = &a.discriminant {
                    expr(e, v);
                }
            }
        }
        Item::Trait(t) => {
            for m in &t.methods {
                params(&m.params, v);
            }
        }
        Item::Import(_)
        | Item::Typedef(_)
        | Item::Distinct(_)
        | Item::Extern(_) => {}
    }
}
fn block(b: &Block, v: &mut impl FnMut(&Expr)) {
    for s in &b.stmts {
        match s {
            Stmt::VarDecl(d) => {
                if let Some(e) = &d.init {
                    expr(e, v);
                }
            }
            Stmt::Const(d) => {
                expr(&d.init, v);
            }
            Stmt::Destructure(d) => {
                expr(&d.expr, v);
            }
            Stmt::Expr(e) => expr(&e.expr, v),
            Stmt::Return(r) => {
                if let Some(e) = &r.value {
                    expr(e, v);
                }
            }
            Stmt::Delete(d) => expr(&d.target, v),
            Stmt::Block(b) => block(b, v),
            Stmt::If(i) => {
                expr(&i.cond, v);
                block(&i.then_block, v);
                if let Some(b) = &i.else_block {
                    block(b, v);
                }
            }
            Stmt::While(w) => {
                expr(&w.cond, v);
                block(&w.body, v);
            }
            Stmt::Loop(l) => block(&l.body, v),
            Stmt::For(f) => {
                expr(&f.iter, v);
                block(&f.body, v);
            }
            Stmt::Assert(a) => {
                expr(&a.cond, v);
                if let Some(e) = &a.message {
                    expr(e, v);
                }
            }
            Stmt::Defer(d) => match &d.inner {
                DeferInner::Expr(e) => expr(e, v),
                DeferInner::Block(b) => block(b, v),
            },
            Stmt::Scope(b) => block(b, v),
            Stmt::Yield(_) => {}
            Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }
}
fn args(args: &[CallArg], v: &mut impl FnMut(&Expr)) {
    for a in args {
        match a {
            CallArg::Expr(e) | CallArg::Named { value: e, .. } => expr(e, v),
            CallArg::Ref { expr: e, .. } => expr(e, v),
            CallArg::Out { .. } => {}
        }
    }
}

fn expr(e: &Expr, v: &mut impl FnMut(&Expr)) {
    v(e);
    match &e.kind {
        ExprKind::Paren(e)
        | ExprKind::Unary { expr: e, .. }
        | ExprKind::Postfix { expr: e, .. } => expr(e, v),
        ExprKind::Binary { lhs, rhs, .. } => {
            expr(lhs, v);
            expr(rhs, v);
        }
        ExprKind::Assign { lhs, value }
        | ExprKind::CompoundAssign { lhs, value, .. } => {
            expr(lhs, v);
            expr(value, v);
        }
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            expr(cond, v);
            expr(then_branch, v);
            expr(else_branch, v);
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(e) = start {
                expr(e, v);
            }
            if let Some(e) = end {
                expr(e, v);
            }
        }
        ExprKind::Call { args: a, .. }
        | ExprKind::New { args: a, .. }
        | ExprKind::EnumVariant { args: a, .. } => args(a, v),
        ExprKind::Await { task, .. } | ExprKind::Spawn { task, .. } => expr(task, v),
        ExprKind::MethodCall {
            object, args: a, ..
        } => {
            expr(object, v);
            args(a, v);
        }
        ExprKind::MemberAccess { object, .. }
        | ExprKind::NullableMemberAccess { object, .. } => expr(object, v),
        ExprKind::Index { object, index } => {
            expr(object, v);
            expr(index, v);
        }
        ExprKind::Slice {
            object, start, end, ..
        } => {
            expr(object, v);
            if let Some(e) = start {
                expr(e, v);
            }
            if let Some(e) = end {
                expr(e, v);
            }
        }
        ExprKind::Tuple(es) | ExprKind::ArrayLit(es) => {
            for e in es {
                expr(e, v);
            }
        }
        ExprKind::MapLit { entries, .. } => {
            for (k, e) in entries {
                expr(k, v);
                expr(e, v);
            }
        }
        ExprKind::StructLit { fields, .. } => {
            for (_, _, e) in fields {
                expr(e, v);
            }
        }
        ExprKind::InterpolatedString(parts, _) => {
            for p in parts {
                if let InterpolatedPart::Expr(e) = p {
                    expr(e, v);
                }
            }
        }
        ExprKind::Match(m) => {
            expr(&m.scrutinee, v);
            for a in &m.arms {
                if let Some(e) = &a.guard {
                    expr(e, v);
                }
                match &a.body {
                    MatchArmBody::Expr(e) => expr(e, v),
                    MatchArmBody::Block(b) => block(b, v),
                }
            }
        }
        ExprKind::Closure {
            params: ps, body, ..
        } => {
            params(ps, v);
            match body.as_ref() {
                ClosureBody::Expr(e) => expr(e, v),
                ClosureBody::Block(b) => block(b, v),
            }
        }
        ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::BoolLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::CharLit(_)
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::Null
        | ExprKind::VecEmpty(_) => {}
    }
}
