//! AST traversal for lint rules. Exhaustive matches keep new syntax visible.
use crate::ast::*;

pub(super) fn program(p: &Program, visit: &mut impl FnMut(&Expr)) {
    for i in &p.items {
        item(i, visit);
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
