//! Advisory checks, separate from semantic errors. Not a borrow checker.
use crate::ast::*;
use crate::token::Span;
use std::collections::HashSet;

#[cfg(test)]
mod tests;
pub(crate) mod walk;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
}

/// Analyze a single, unexpanded source file (spans belong to that file).
/// H001 checks explicit `ref`/`out` arguments for repeated bare variables.
/// It deliberately makes no claims about aliasing through pointers or fields.
pub fn check(program: &Program) -> Vec<Warning> {
    let mut warnings = Vec::new();
    walk::program(program, &mut |expr| {
        let args = match &expr.kind {
            ExprKind::Call { args, .. }
            | ExprKind::MethodCall { args, .. }
            | ExprKind::New { args, .. }
            | ExprKind::EnumVariant { args, .. } => args,
            _ => return,
        };
        let mut seen = HashSet::new();
        for arg in args {
            let name = match arg {
                CallArg::Ref { expr, .. } => ident(expr),
                CallArg::Out { ty: None, name, .. } => Some(name.as_str()),
                // Conservatively treat typed out as a binding boundary. It can
                // create a variable; without scope resolution we do not warn
                // on the declaration itself (even when lowering reuses a slot).
                CallArg::Out {
                    ty: Some(_), name, ..
                } => {
                    seen.remove(name.as_str());
                    seen.insert(name.as_str());
                    continue;
                }
                _ => None,
            };
            if let Some(name) = name {
                if !seen.insert(name) {
                    warnings.push(Warning {
                        code: "H001",
                        message: format!("`{name}` is passed to multiple ref/out arguments in this call; writes through one argument also change the other"),
                        span: arg.span(),
                    });
                }
            }
        }
    });
    warnings.sort_by_key(|w| (w.span.start, w.span.end));
    warnings
}

fn ident(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Ident(name) => Some(name),
        ExprKind::Paren(inner) => ident(inner),
        _ => None,
    }
}
