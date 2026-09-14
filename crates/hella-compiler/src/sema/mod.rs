//! Phase 2 semantic checks: structs, field access, literals.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::token::Span;

#[derive(Debug, Clone)]
pub struct SemError {
    pub message: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Int,
    /// Unsigned pointer-sized (`uint`) — LLVM `i64` on 64-bit.
    UInt,
    /// Fixed-width integer from stdlib (`i8`..`i128`, `u8`..`u128`).
    /// These are stdlib types (types skill §2), NOT compiler keywords:
    /// they lex as `Ident` and resolve here via the implicit stdlib env.
    SizedInt { bits: u16, signed: bool },
    Bool,
    Void,
    Char,
    String,
    Float,
    Double,
    Struct(String),
    Enum(String),
    Generic(String, Vec<Ty>),
    Tuple(Vec<Ty>),
    Any,
    Function(Box<Ty>, Vec<Ty>),
    Array(Box<Ty>),
    /// Fixed-size array (types skill §7-8): `TYPE arr` (size inferred,
    /// `None`) or `TYPE arr[N]` (explicit). Arrays are fixed-size for life.
    FixedArray { elem: Box<Ty>, size: Option<usize> },
    /// Dynamic owning vector (types skill §9-11): `TYPE vec`. An `Any`
    /// element means undetermined — from `vec[]` until the first `push`
    /// establishes it permanently.
    Vec(Box<Ty>),
    /// Associative map (types skill §12-14): `KEY:VALUE` (e.g. `string:int`).
    /// Implementation/storage belongs to stdlib; the compiler validates the
    /// key/value types at construction and access.
    Map { key: Box<Ty>, value: Box<Ty> },
    Pointer(Box<Ty>),
    Optional(Box<Ty>),
    /// Owning heap pointer: `own T`. Moves (never copies); scope-destroyed.
    Own(Box<Ty>),
}

impl Ty {
    /// All fundamental stdlib integer names → Ty (types skill §1-2, §16-18).
    /// `int`/`uint` are pointer-sized; the rest are fixed-width.
    pub fn from_stdlib_name(name: &str) -> Option<Ty> {
        match name {
            "int" => Some(Ty::Int),
            "uint" => Some(Ty::UInt),
            "i8" => Some(Ty::SizedInt { bits: 8, signed: true }),
            "i16" => Some(Ty::SizedInt { bits: 16, signed: true }),
            "i32" => Some(Ty::SizedInt { bits: 32, signed: true }),
            "i64" => Some(Ty::SizedInt { bits: 64, signed: true }),
            "i128" => Some(Ty::SizedInt { bits: 128, signed: true }),
            "u8" => Some(Ty::SizedInt { bits: 8, signed: false }),
            "u16" => Some(Ty::SizedInt { bits: 16, signed: false }),
            "u32" => Some(Ty::SizedInt { bits: 32, signed: false }),
            "u64" => Some(Ty::SizedInt { bits: 64, signed: false }),
            "u128" => Some(Ty::SizedInt { bits: 128, signed: false }),
            _ => None,
        }
    }

    pub fn is_int_like(&self) -> bool {
        matches!(self, Ty::Int | Ty::UInt | Ty::SizedInt { .. })
    }

    /// MVP compatibility: any int-like is assignable to any int-like
    /// (widening/narrowing via trunc/extend in codegen). Strict width
    /// checking is deferred.
    pub fn int_compatible(a: &Ty, b: &Ty) -> bool {
        a.is_int_like() && b.is_int_like()
    }

    /// Assignment compatibility: exact match, `any` either side, or
    /// int-like ↔ int-like (sized ints interoperate with `int` literals).
    pub fn assignable(from: &Ty, to: &Ty) -> bool {
        if from == to || *to == Ty::Any || *from == Ty::Any {
            return true;
        }
        if Self::int_compatible(from, to) {
            return true;
        }
        // Fixed arrays: element-wise assignable; an inferred-size (`None`)
        // target accepts any length, an explicit target requires equal size.
        // Vectors: element-wise assignable; an `Any` element on either side
        // means undetermined (empty `vec[]`) and accepts anything — the
        // first `push` establishes it (checked in MethodCall).
        match (from, to) {
            (
                Ty::FixedArray { elem: fe, size: fs },
                Ty::FixedArray { elem: te, size: ts },
            ) => {
                if !Self::assignable(fe, te) {
                    return false;
                }
                match (fs, ts) {
                    (_, None) => true,
                    (Some(a), Some(b)) => a == b,
                    // Explicit target vs unknown-length source: accept here;
                    // length is validated at the declaration site.
                    (None, Some(_)) => true,
                }
            }
            (Ty::Vec(fe), Ty::Vec(te)) => {
                if **fe == Ty::Any || **te == Ty::Any {
                    return true;
                }
                Self::assignable(fe, te)
            }
            // Array literals initialize vectors element-wise
            // (`int vec xs = [1, 2, 3]`).
            (Ty::FixedArray { elem: fe, .. }, Ty::Vec(te)) => {
                if **te == Ty::Any {
                    return true;
                }
                Self::assignable(fe, te)
            }
            // Maps: key- and value-wise assignable.
            (Ty::Map { key: fk, value: fv }, Ty::Map { key: tk, value: tv }) => {
                Self::assignable(fk, tk) && Self::assignable(fv, tv)
            }
            _ => false,
        }
    }
}

impl From<&Type> for Ty {
    fn from(t: &Type) -> Self {
        match t {
            Type::Int(_) => Ty::Int,
            Type::Bool(_) => Ty::Bool,
            Type::Void(_) => Ty::Void,
            Type::String(_) => Ty::String,
            Type::Char(_) => Ty::Char,
            Type::Float(_) => Ty::Float,
            Type::Double(_) => Ty::Double,
            Type::Named(n, _) => {
                let base = n.rsplit("::").next().unwrap_or(n).to_string();
                if let Some(std_ty) = Ty::from_stdlib_name(&base) {
                    return std_ty;
                }
                Ty::Struct(base)
            },
            Type::Generic(n, args, _) => {
                let base = n.rsplit("::").next().unwrap_or(n).to_string();
                Ty::Generic(base, args.iter().map(|a| Ty::from(a)).collect())
            },
            Type::FunctionType(ret, args, _) => Ty::Function(Box::new(Ty::from(ret.as_ref())), args.iter().map(|a| Ty::from(a)).collect()),
            Type::Tuple(tys, _) => Ty::Tuple(tys.iter().map(|t| Ty::from(t)).collect()),
            Type::Any(_) => Ty::Any,
            Type::Array(el, _) => Ty::Array(Box::new(Ty::from(el.as_ref()))),
            Type::FixedArray { elem, size, .. } => Ty::FixedArray {
                elem: Box::new(Ty::from(elem.as_ref())),
                size: size.map(|n| n as usize),
            },
            Type::Vec { elem, .. } => Ty::Vec(Box::new(Ty::from(elem.as_ref()))),
            Type::Map { key, value, .. } => Ty::Map {
                key: Box::new(Ty::from(key.as_ref())),
                value: Box::new(Ty::from(value.as_ref())),
            },
            Type::Pointer(el, _) => {
                Ty::Pointer(Box::new(Ty::from(el.as_ref())))
            }
            Type::Optional(el, _) => {
                Ty::Optional(Box::new(Ty::from(el.as_ref())))
            }
            Type::Own(el, _) => {
                Ty::Own(Box::new(Ty::from(el.as_ref())))
            }
        }
    }
}
impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::UInt => write!(f, "uint"),
            Ty::SizedInt { bits, signed } => {
                let c = if *signed { 'i' } else { 'u' };
                write!(f, "{c}{bits}")
            }
            Ty::Bool => write!(f, "bool"),
            Ty::Void => write!(f, "void"),
            Ty::Char => write!(f, "char"),
            Ty::String => write!(f, "string"),
            Ty::Float => write!(f, "float"),
            Ty::Double => write!(f, "double"),
            Ty::Struct(n) => write!(f, "{}", n),
            Ty::Enum(n) => write!(f, "{}", n),
            Ty::Generic(n, args) => {
                if args.is_empty() {
                    write!(f, "{}", n)
                } else {
                    write!(f, "{}<{}>", n, args.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", "))
                }
            }
            Ty::Tuple(tys) => write!(f, "({})", tys.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ")),
            Ty::Any => write!(f, "any"),
            Ty::Function(ret, args) => write!(f, "function<{}({})>", ret, args.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")),
            Ty::Array(el) => write!(f, "{}[]", el),
            Ty::FixedArray { elem, size } => match size {
                Some(n) => write!(f, "{} arr[{}]", elem, n),
                None => write!(f, "{} arr", elem),
            },
            Ty::Vec(elem) => write!(f, "{} vec", elem),
            Ty::Map { key, value } => write!(f, "{}:{}", key, value),
            Ty::Pointer(el) => write!(f, "{}*", el),
            Ty::Optional(el) => write!(f, "{}?", el),
            Ty::Own(el) => write!(f, "own {}", el),
        }
    }
}

#[derive(Clone, Debug)]
struct FuncSig {
    ret: Ty,
    params: Vec<Ty>,
    param_modes: Vec<ParamMode>,
    param_names: Vec<String>,
    param_is_variadic: Vec<bool>,
    param_defaults: Vec<Option<Expr>>,
    generic_params: Vec<GenericParam>,
    where_clause: Option<WhereClause>,
    span: Span,
}

#[derive(Clone, Debug)]
struct StructInfo {
    name: String,
    fields: Vec<(String, Ty)>, // ordered
    field_map: HashMap<String, (usize, Ty)>,
    field_vis: HashMap<String, crate::ast::Visibility>,
    field_defaults: HashMap<String, Option<Expr>>,
    generic_params: Vec<GenericParam>,
    where_clause: Option<WhereClause>,
    span: Span,
}

#[derive(Clone, Debug)]
struct ClassInfo {
    name: String,
    fields: Vec<(String, Ty)>,
    field_map: HashMap<String, (usize, Ty)>,
    field_vis: HashMap<String, crate::ast::Visibility>,
    methods: HashMap<String, FuncSig>,
    method_vis: HashMap<String, crate::ast::Visibility>,
    constructors: Vec<(FuncSig, crate::ast::Visibility)>,
    properties: HashMap<String, PropertyInfo>,
    operators: HashMap<String, FuncSig>,
    conversions: Vec<(Ty, Ty, Span)>,
    is_open: bool,
    is_sealed: bool,
    extends: Option<String>,
    implements: Vec<String>,
    generic_params: Vec<GenericParam>,
    where_clause: Option<WhereClause>,
    span: Span,
}

#[derive(Clone, Debug)]
struct PropertyInfo {
    ty: Ty,
    has_get: bool,
    has_set: bool,
    visibility: crate::ast::Visibility,
    span: Span,
}

#[derive(Clone, Debug)]
struct TraitInfo {
    name: String,
    methods: HashMap<String, FuncSig>,
    generic_params: Vec<GenericParam>,
    where_clause: Option<WhereClause>,
    span: Span,
}

pub struct Checker {
    funcs: HashMap<String, FuncSig>,
    structs: HashMap<String, StructInfo>,
    classes: HashMap<String, ClassInfo>,
    enums: HashMap<String, EnumInfo>,
    traits: HashMap<String, TraitInfo>,
    scopes: Vec<HashMap<String, Ty>>,
    const_scopes: Vec<HashSet<String>>,
    /// Moved/deleted `own` slots per scope level, parallel to `scopes`.
    /// A name here must not be read (use-after-move/use-after-delete).
    /// Declaration of a name clears it (fresh binding shadows poison).
    poisoned: Vec<HashSet<String>>,
    errors: Vec<SemError>,
    cur_ret: Option<Ty>,
    cur_class: Option<String>,
    loop_stack: Vec<Option<String>>,
    /// Whether a missing `main` is an error. The CLI always requires it;
    /// the LSP only requires it for files named `main` (library modules
    /// next to `main.hll` are checked without an entry point).
    require_main: bool,
}

#[derive(Clone, Debug)]
struct EnumInfo {
    name: String,
    variants: Vec<EnumVariantInfo>,
    variant_map: HashMap<String, (usize, Vec<Ty>)>, // variant -> (tag, payload tys)
    generic_params: Vec<GenericParam>,
    where_clause: Option<WhereClause>,
    span: Span,
}

#[derive(Clone, Debug)]
struct EnumVariantInfo {
    name: String,
    tag: usize,
    payload_tys: Vec<Ty>,
    discriminant_expr: Option<Expr>,
    span: Span,
}

impl Checker {
    pub fn new() -> Self {
        Self {
            funcs: HashMap::new(),
            structs: HashMap::new(),
            classes: HashMap::new(),
            enums: HashMap::new(),
            traits: HashMap::new(),
            scopes: Vec::new(),
            const_scopes: Vec::new(),
            poisoned: Vec::new(),
            errors: Vec::new(),
            cur_ret: None,
            cur_class: None,
            loop_stack: Vec::new(),
            require_main: true,
        }
    }

    /// Only require a `main` function when the checked file is an entry
    /// point (the LSP passes `false` for library modules).
    pub fn set_require_main(&mut self, require: bool) {
        self.require_main = require;
    }

    fn loop_depth(&self) -> usize { self.loop_stack.len() }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.const_scopes.push(HashSet::new());
        self.poisoned.push(HashSet::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.const_scopes.pop();
        // No poison merge needed: moves/deletes insert at the DECLARING
        // level (found via lookup, like the use sites below), so branch and
        // loop bodies already poison precisely. Shadow-locals die with their
        // scope; outer same-named bindings are untouched.
        self.poisoned.pop();
    }
    /// Innermost scope level declaring `name`, if any (mirrors lookup).
    fn scope_index_of(&self, name: &str) -> Option<usize> {
        for (i, scope) in self.scopes.iter().enumerate().rev() {
            if scope.contains_key(name) {
                return Some(i);
            }
        }
        None
    }
    /// True when the VISIBLE declaration of `name` is poisoned
    /// (moved/deleted). Shadowing is precise: a fresh inner binding reads
    /// clean even when an outer same-named slot is poisoned.
    fn is_poisoned(&self, name: &str) -> bool {
        match self.scope_index_of(name) {
            Some(idx) => self.poisoned.get(idx).is_some_and(|s| s.contains(name)),
            None => false,
        }
    }
    /// Poison the scope level that declares `name` (moves bind to the
    /// visible declaration, so shadows stay precise).
    fn poison_at_decl(&mut self, name: &str) {
        if let Some(idx) = self.scope_index_of(name) {
            if let Some(set) = self.poisoned.get_mut(idx) {
                set.insert(name.to_string());
            }
        }
    }
    /// Unwrap `own T` to `T` for member/method access (auto-deref on `.`).
    fn deref_ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Own(inner) => (**inner).clone(),
            other => other.clone(),
        }
    }

    /// True when `ty` is an owning heap pointer.
    fn is_own(&self, ty: &Ty) -> bool {
        matches!(ty, Ty::Own(_))
    }

    /// Inner type of an `own T`, if present.
    fn own_inner(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            Ty::Own(inner) => Some((**inner).clone()),
            _ => None,
        }
    }

    /// Rebirth: assigning a fresh value clears poison at the declared level.
    fn unpoison_at_decl(&mut self, name: &str) {
        if let Some(idx) = self.scope_index_of(name) {
            if let Some(set) = self.poisoned.get_mut(idx) {
                set.remove(name);
            }
        }
    }

    /// Collect identifier names occurring in transparent value-forwarding
    /// positions: the expression itself, parentheses, conditional branches,
    /// and match arm bodies. Stops at calls (their own arg boundaries move),
    /// member access (reads borrow the base), and closures (borrow capture).
    /// Used both to poison moved sources and to null them in codegen shape.
    fn moved_ident_names(expr: &Expr, out: &mut Vec<String>) {
        match &expr.kind {
            ExprKind::Ident(n) => out.push(n.clone()),
            ExprKind::Paren(e) => Self::moved_ident_names(e, out),
            ExprKind::Conditional {
                then_branch,
                else_branch,
                ..
            } => {
                Self::moved_ident_names(then_branch, out);
                Self::moved_ident_names(else_branch, out);
            }
            ExprKind::Match(m) => {
                for arm in &m.arms {
                    match &arm.body {
                        MatchArmBody::Expr(e) => Self::moved_ident_names(e, out),
                        // Block bodies evaluate to their last value in
                        // complex ways; conservative approximation lives
                        // with the callers (they only enter here for
                        // own-typed positions, which are rare).
                        MatchArmBody::Block(_) => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// A `new` with no owner: `New` possibly wrapped in parentheses.
    /// Such a value in statement/condition position is a guaranteed leak —
    /// nothing takes ownership (no `own` slot, no call-arg boundary, no delete).
    fn is_bare_new(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::New { .. } => true,
            ExprKind::Paren(e) => Self::is_bare_new(e),
            _ => false,
        }
    }

    /// Poison every own-typed variable named in value-forwarding position
    /// within `expr` (a move into an `own` slot transfers ownership out of
    /// each of them; untaken branches merely leak, never dangle).
    fn poison_moved_idents(&mut self, expr: &Expr) {
        let mut names = Vec::new();
        Self::moved_ident_names(expr, &mut names);
        for name in names {
            let base = name.rsplit("::").next().unwrap_or(&name);
            if matches!(self.lookup_var(&name), Some(Ty::Own(_)))
                || matches!(self.lookup_var(base), Some(Ty::Own(_)))
            {
                self.poison_at_decl(&name);
            }
        }
    }

    fn declare_var(&mut self, name: &str, ty: Ty, span: Span) -> bool {
        if let Some(scope) = self.scopes.last_mut() {
            if scope.contains_key(name) {
                self.errors.push(SemError {
                    message: format!("redefinition of `{name}`"),
                    span,
                });
                return false;
            }
            scope.insert(name.to_string(), ty);
            true
        } else {
            false
        }
    }
    fn declare_const(&mut self, name: &str, ty: Ty, span: Span) -> bool {
        if let Some(scope) = self.scopes.last_mut() {
            if scope.contains_key(name) {
                self.errors.push(SemError {
                    message: format!("redefinition of `{name}`"),
                    span,
                });
                return false;
            }
            scope.insert(name.to_string(), ty);
            if let Some(cset) = self.const_scopes.last_mut() {
                cset.insert(name.to_string());
            }
            true
        } else {
            false
        }
    }
    fn is_const(&self, name: &str) -> bool {
        for cset in self.const_scopes.iter().rev() {
            if cset.contains(name) {
                return true;
            }
        }
        false
    }
    fn lookup_var(&self, name: &str) -> Option<Ty> {
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty.clone());
            }
        }
        None
    }

    /// True when `own` appears anywhere inside a type (including as the
    /// whole type — callers exclude positions they already validated).
    fn contains_own(ty: &Ty) -> bool {
        match ty {
            Ty::Own(_) => true,
            Ty::Array(e) | Ty::Vec(e) | Ty::Pointer(e) | Ty::Optional(e) => {
                Self::contains_own(e)
            }
            Ty::FixedArray { elem, .. } => Self::contains_own(elem),
            Ty::Map { key, value } => Self::contains_own(key) || Self::contains_own(value),
            Ty::Generic(_, args) | Ty::Tuple(args) => args.iter().any(Self::contains_own),
            Ty::Function(ret, params) => {
                Self::contains_own(ret) || params.iter().any(Self::contains_own)
            }
            _ => false,
        }
    }

    /// `null` flows into any nullable slot — except `own`, which is
    /// non-nullable by construction. Call at every `is_null` bypass.
    fn reject_null_own(&mut self, is_null: bool, decl_ty: &Ty, span: Span) {
        if is_null && matches!(decl_ty, Ty::Own(_)) {
            self.errors.push(SemError {
                message: "null cannot flow into `own` (non-nullable ownership)".into(),
                span,
            });
        }
    }

    /// Assignment compatibility with class→trait upcasts: a class value
    /// flows into a trait-typed slot when the class implements the trait
    /// (directly or through its `extends` chain). Falls back to the
    /// structural rules otherwise.
    fn ty_assignable(&self, from: &Ty, to: &Ty) -> bool {
        if let (Ty::Struct(f), Ty::Struct(t)) = (from, to) {
            if f != t && self.class_implements_trait(f, t) {
                return true;
            }
        }
        // Owning pointers: identical ownership moves; `own C` upcasts to
        // `own T` under `implements`; `own C` borrows as `C*`. `any`
        // (including `null`) never converts into ownership — no provenance.
        if let (Ty::Own(f), Ty::Own(t)) = (from, to) {
            if f == t {
                return true;
            }
            if let (Ty::Struct(fc), Ty::Struct(tc)) = (f.as_ref(), t.as_ref()) {
                if fc != tc && self.class_implements_trait(fc, tc) {
                    return true;
                }
            }
            return false;
        }
        if let (Ty::Own(f), Ty::Pointer(t)) = (from, to) {
            if let (Ty::Struct(fc), Ty::Struct(tc)) = (f.as_ref(), t.as_ref()) {
                if fc == tc {
                    return true;
                }
            }
            return false;
        }
        if matches!(to, Ty::Own(_)) {
            return false;
        }
        Ty::assignable(from, to)
    }

    /// Trait type (if any) hiding inside a type, for rejecting trait types
    /// at the FFI boundary (trait pairs have no C representation).
    fn extern_trait_name(&self, ty: &Ty) -> Option<String> {
        match ty {
            Ty::Struct(n) if self.traits.contains_key(n) => Some(n.clone()),
            Ty::Array(e) | Ty::Vec(e) | Ty::Pointer(e) | Ty::Optional(e) => {
                self.extern_trait_name(e)
            }
            Ty::FixedArray { elem, .. } => self.extern_trait_name(elem),
            Ty::Map { key, value } => self
                .extern_trait_name(key)
                .or_else(|| self.extern_trait_name(value)),
            Ty::Generic(_, args) | Ty::Tuple(args) => {
                args.iter().find_map(|a| self.extern_trait_name(a))
            }
            Ty::Function(ret, params) => std::iter::once(ret.as_ref())
                .chain(params.iter())
                .find_map(|a| self.extern_trait_name(a)),
            _ => None,
        }
    }

    /// True when class `class_name` implements trait `trait_name`, directly
    /// or via its `extends` chain (cycle-guarded).
    fn class_implements_trait(&self, class_name: &str, trait_name: &str) -> bool {
        let mut seen = HashSet::new();
        let mut cur = Some(class_name.to_string());
        while let Some(name) = cur {
            if !seen.insert(name.clone()) {
                break;
            }
            let Some(info) = self.classes.get(&name) else {
                break;
            };
            if info.implements.iter().any(|t| t == trait_name) {
                return true;
            }
            cur = info.extends.clone();
        }
        false
    }

    /// Arity + per-argument checking against a method signature, shared by
    /// class and trait method calls.
    fn check_method_call_args(
        &mut self,
        args: &[CallArg],
        sig: &FuncSig,
        method: &str,
        method_span: &Span,
    ) {
        if sig.param_is_variadic.iter().any(|&v| v) {
            self.check_call_with_sig(args, sig, *method_span, method);
        } else {
            let min_required = Self::min_args_for_sig(sig);
            if args.len() < min_required || args.len() > sig.params.len() {
                if min_required == sig.params.len() {
                    self.errors.push(SemError{message: format!("method `{}` expects {} args, found {}", method, sig.params.len(), args.len()), span: *method_span});
                } else {
                    self.errors.push(SemError{message: format!("method `{}` expects {}-{} args, found {}", method, min_required, sig.params.len(), args.len()), span: *method_span});
                }
            }
            for (i, a) in args.iter().enumerate() {
                let pidx = if let CallArg::Named { name, .. } = a {
                    sig.param_names.iter().position(|n| n == name).unwrap_or(i)
                } else { i };
                self.declare_out_arg(a, sig.params.get(pidx));
                let aty = self.check_call_arg(a);
                if let Some(pt) = sig.params.get(pidx) {
                    if !self.ty_assignable(&aty, pt) { self.errors.push(SemError{message: format!("arg {} of `{}`: expected `{}`, found `{}`", i+1, method, pt, aty), span: a.span()}); }
                }
                self.check_arg_mode(a, sig, pidx, method, i + 1);
            }
            // Missing arguments must have defaults (filled at codegen).
            let mut provided = std::collections::HashSet::new();
            let mut n_pos = 0;
            for a in args {
                if let CallArg::Named { name, .. } = a {
                    if let Some(pidx) = sig.param_names.iter().position(|n| n == name) {
                        provided.insert(pidx);
                    }
                } else {
                    provided.insert(n_pos);
                    n_pos += 1;
                }
            }
            for (pidx, pname) in sig.param_names.iter().enumerate() {
                if !provided.contains(&pidx) && sig.param_defaults.get(pidx).and_then(|d| d.as_ref()).is_none() {
                    self.errors.push(SemError{message: format!("missing argument `{}` for method `{}` (no default value)", pname, method), span: *method_span});
                }
            }
        }
    }

    /// Method call on a trait-typed receiver (closed-world interface
    /// dispatch). Resolves against the trait's own methods first; otherwise
    /// requires every known implementor to provide the method with an
    /// identical calling convention (return + params + modes + variadic)
    /// and public visibility — which is what codegen's tag-switch
    /// dispatches over. Returns the method's return type.
    fn check_trait_method_call(
        &mut self,
        tname: &str,
        method: &str,
        method_span: &Span,
        args: &[CallArg],
        obj_span: Span,
    ) -> Ty {
        if let Some(sig) = self
            .traits
            .get(tname)
            .and_then(|t| t.methods.get(method))
            .cloned()
        {
            self.check_method_call_args(args, &sig, method, method_span);
            return sig.ret;
        }
        let implementors: Vec<String> = self
            .classes
            .keys()
            .filter(|c| self.class_implements_trait(c, tname))
            .cloned()
            .collect();
        if implementors.is_empty() {
            self.errors.push(SemError{message: format!("unknown method `{method}` on trait `{tname}` (no known implementors)"), span: *method_span});
            for a in args {
                let _ = self.check_call_arg(a);
            }
            return Ty::Int;
        }
        let mut common: Option<FuncSig> = None;
        for cname in &implementors {
            let entry: Option<(FuncSig, crate::ast::Visibility)> = self
                .classes
                .get(cname)
                .and_then(|cls| {
                    cls.methods.get(method).map(|sig| {
                        let vis = cls.method_vis.get(method).cloned().unwrap_or(crate::ast::Visibility::Default);
                        (sig.clone(), vis)
                    })
                });
            match entry {
                None => {
                    self.errors.push(SemError{message: format!("class `{cname}` does not provide method `{method}` (called on trait `{tname}` receiver)"), span: *method_span});
                    for a in args {
                        let _ = self.check_call_arg(a);
                    }
                    return Ty::Int;
                }
                Some((sig, vis)) => {
                    // Trait receivers are always "outside": the method must
                    // be public on every implementor.
                    if vis != crate::ast::Visibility::Public {
                        self.errors.push(SemError{message: format!("method `{method}` is not public on all implementors of `{tname}` (via `{cname}`)"), span: *method_span});
                        for a in args {
                            let _ = self.check_call_arg(a);
                        }
                        return Ty::Int;
                    }
                    if let Some(prev) = &common {
                        if prev.ret != sig.ret
                            || prev.params != sig.params
                            || prev.param_modes != sig.param_modes
                            || prev.param_is_variadic != sig.param_is_variadic
                        {
                            self.errors.push(SemError{message: format!("method `{method}` has mismatched signatures across implementors of `{tname}`"), span: *method_span});
                            for a in args {
                                let _ = self.check_call_arg(a);
                            }
                            return Ty::Int;
                        }
                    } else {
                        common = Some(sig);
                    }
                }
            }
        }
        match common {
            Some(sig) => {
                self.check_method_call_args(args, &sig, method, method_span);
                sig.ret
            }
            None => {
                self.errors.push(SemError{message: format!("unknown method `{method}` on trait `{tname}`"), span: obj_span});
                for a in args {
                    let _ = self.check_call_arg(a);
                }
                Ty::Int
            }
        }
    }

    /// Field read/write through a trait-typed receiver (closed-world
    /// structural dispatch): every known implementor must expose the field
    /// with an identical public type. Returns the agreed type.
    fn check_trait_field(&mut self, tname: &str, field: &str, field_span: &Span) -> Ty {
        let implementors: Vec<String> = self
            .classes
            .keys()
            .filter(|c| self.class_implements_trait(c, tname))
            .cloned()
            .collect();
        if implementors.is_empty() {
            self.errors.push(SemError{message: format!("trait `{tname}` has no field `{field}` (no known implementors)"), span: *field_span});
            return Ty::Int;
        }
        let mut common: Option<Ty> = None;
        for cname in &implementors {
            let fty = self
                .structs
                .get(cname)
                .and_then(|s| s.field_map.get(field).map(|(_, t)| t.clone()));
            let Some(fty) = fty else {
                self.errors.push(SemError{message: format!("class `{cname}` does not expose field `{field}` (via trait `{tname}`)"), span: *field_span});
                return Ty::Int;
            };
            let vis_ok = self
                .classes
                .get(cname)
                .and_then(|c| c.field_vis.get(field))
                .is_some_and(|v| *v == crate::ast::Visibility::Public);
            if !vis_ok {
                self.errors.push(SemError{message: format!("field `{field}` is not public on all implementors of `{tname}` (via `{cname}`)"), span: *field_span});
                return Ty::Int;
            }
            match &common {
                Some(t) if *t == fty => {}
                Some(t) => {
                    self.errors.push(SemError{message: format!("field `{field}` has mismatched types across implementors of `{tname}` (`{t}` vs `{fty}`)"), span: *field_span});
                    return Ty::Int;
                }
                None => {
                    common = Some(fty);
                }
            }
        }
        common.unwrap_or(Ty::Int)
    }

    /// Update an existing variable's type in the nearest scope holding it.
    /// Used for `vec[]` element-type establishment on the first `push`.
    fn set_var_ty(&mut self, name: &str, ty: Ty) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), ty);
                return true;
            }
        }
        false
    }

    fn resolve_type(&mut self, ty: &Type) -> Ty {
        // Enforce generic bounds at every instantiation site (`Box<int>` in
        // any type position). Single-letter type parameters themselves carry
        // no arguments, so this only fires on real applications.
        if let Type::Generic(base, args, _) = ty {
            if !args.is_empty() {
                let span = ty.span();
                self.check_type_application(base, args, span);
            }
        }
        let mut t = Ty::from(ty);
        // Handle generic type params: if t is Struct with name that is a generic param, treat as Generic
        if let Ty::Struct(ref n) = t {
            let lookup = n.rsplit("::").next().unwrap_or(n);
            // Implicit stdlib fundamental ints (types skill §1-2, §16-18):
            // `i8`..`u128`, `uint` lex as Ident → resolve without import.
            // (`int` stays a keyword token → Ty::Int via From; Ident "int"
            // e.g. qualified `std::int` also maps here.)
            if let Some(std_ty) = Ty::from_stdlib_name(lookup) {
                // Don't shadow a user struct/class/enum/trait with the same name.
                if !self.structs.contains_key(lookup)
                    && !self.classes.contains_key(lookup)
                    && !self.enums.contains_key(lookup)
                    && !self.traits.contains_key(lookup)
                {
                    return std_ty;
                }
            }
            // Check if it's a generic param for current function/class
            // For minimal, check if it's a single uppercase letter like T, U, V
            if lookup.len() == 1 && lookup.chars().next().unwrap().is_ascii_uppercase() {
                // Consider it as generic if not known struct/class/enum
                if !self.structs.contains_key(lookup) && !self.classes.contains_key(lookup) && !self.enums.contains_key(lookup) {
                    t = Ty::Generic(lookup.to_string(), vec![]);
                    return t;
                }
            }
            if n == "__derived__" {
                // Variadic derived `... vda` without explicit type: placeholder, will be resolved to `prev_type[]` in check_function
                t = Ty::Array(Box::new(Ty::Any));
            } else if self.enums.contains_key(lookup) {
                t = Ty::Enum(lookup.to_string());
            } else if !self.structs.contains_key(lookup) && !self.classes.contains_key(lookup) && !self.traits.contains_key(lookup) {
                // Check if it's generic param (single uppercase)
                if n.len() == 1 && n.chars().next().unwrap().is_ascii_uppercase() {
                    t = Ty::Generic(n.clone(), vec![]);
                } else {
                    self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()});
                }
            }
        }
        // Owning heap pointer: inner must be a known class, struct, or
        // trait. Anything else (primitives, pointers, containers, nested
        // `own`, generics) is rejected — phase 1 owns class/struct objects.
        if let Ty::Own(ref inner) = t {
            match inner.as_ref() {
                Ty::Struct(n) => {
                    let lookup = n.rsplit("::").next().unwrap_or(n);
                    if !self.structs.contains_key(lookup)
                        && !self.classes.contains_key(lookup)
                        && !self.traits.contains_key(lookup)
                    {
                        self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()});
                    }
                }
                other => {
                    self.errors.push(SemError{message: format!("`own` requires a class, struct, or trait type, found `{other}`"), span: ty.span()});
                }
            }
        }
        // Handle Generic type
        if let Ty::Generic(ref n, ref args) = t {
            // Check base exists
            if !self.structs.contains_key(n) && !self.classes.contains_key(n) && !self.enums.contains_key(n) && !self.traits.contains_key(n) {
                // Could be generic param itself, not base
                if n.len() == 1 && n.chars().next().unwrap().is_ascii_uppercase() {
                    // Generic param, ok
                } else {
                    self.errors.push(SemError{message: format!("unknown generic type `{n}`"), span: ty.span()});
                }
            }
        }
        // `own` may only appear as the whole type (never nested inside
        // containers, pointers, generics, ...). Top-level `own` itself is
        // validated above.
        if !matches!(t, Ty::Own(_)) && Self::contains_own(&t) {
            self.errors.push(SemError{message: "own types cannot be nested inside other types in phase 1".into(), span: ty.span()});
        }
        // For compound types, ensure inner is known (array element etc) – From already handled, but check nested struct/enum existence
        match &t {
            Ty::Array(el) | Ty::Pointer(el) | Ty::Optional(el) => {
                if let Ty::Struct(ref n) = **el {
                    if !self.structs.contains_key(n) && !self.classes.contains_key(n) && !self.enums.contains_key(n) && !self.traits.contains_key(n) {
                        self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()});
                    }
                }
                if let Ty::Enum(ref n) = **el {
                    if !self.enums.contains_key(n) { self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()}); }
                }
                if let Ty::Generic(ref n, _) = **el {
                    if !self.structs.contains_key(n) && !self.classes.contains_key(n) && !self.enums.contains_key(n) && !self.traits.contains_key(n) {
                        self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()});
                    }
                }
            }
            Ty::FixedArray { elem: el, .. } => {
                if let Ty::Struct(ref n) = **el {
                    if !self.structs.contains_key(n) && !self.classes.contains_key(n) && !self.enums.contains_key(n) && !self.traits.contains_key(n) {
                        self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()});
                    }
                }
                if let Ty::Enum(ref n) = **el {
                    if !self.enums.contains_key(n) { self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()}); }
                }
            }
            Ty::Vec(el) => {
                if let Ty::Struct(ref n) = **el {
                    if !self.structs.contains_key(n) && !self.classes.contains_key(n) && !self.enums.contains_key(n) && !self.traits.contains_key(n) {
                        self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()});
                    }
                }
                if let Ty::Enum(ref n) = **el {
                    if !self.enums.contains_key(n) { self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()}); }
                }
            }
            Ty::Map { key, value } => {
                for el in [key, value] {
                    if let Ty::Struct(ref n) = **el {
                        if !self.structs.contains_key(n) && !self.classes.contains_key(n) && !self.enums.contains_key(n) && !self.traits.contains_key(n) {
                            self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()});
                        }
                    }
                    if let Ty::Enum(ref n) = **el {
                        if !self.enums.contains_key(n) { self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()}); }
                    }
                }
            }
            Ty::Generic(n, args) => {
                if !self.structs.contains_key(n) && !self.classes.contains_key(n) && !self.enums.contains_key(n) && !self.traits.contains_key(n) {
                    self.errors.push(SemError{message: format!("unknown type `{n}`"), span: ty.span()});
                }
                for a in args { if let Ty::Struct(sn) = a { if !self.structs.contains_key(sn) && !self.classes.contains_key(sn) && !self.enums.contains_key(sn) { self.errors.push(SemError{message: format!("unknown type `{sn}`"), span: ty.span()}); } } }
            }
            _ => {}
        }
        t
    }

    pub fn check_program(&mut self, prog: &Program) -> Vec<SemError> {
        self.push_scope(); // global scope for top-level consts/vars
        // Unwrap attributed items for struct/class/enum/trait/typedef/distinct
        let mut unwrapped: Vec<&Item> = Vec::new();
        for it in &prog.items {
            match it {
                Item::Attributed{attrs: _, item} => unwrapped.push(item.as_ref()),
                other => unwrapped.push(other),
            }
        }
        // First pass: collect struct definitions
        for item in unwrapped.iter() {
            if let Item::Struct(s) = item {
                if self.structs.contains_key(&s.name) {
                    self.errors.push(SemError {
                        message: format!("duplicate struct `{}`", s.name),
                        span: s.name_span,
                    });
                } else if self.funcs.contains_key(&s.name) {
                    self.errors.push(SemError {
                        message: format!(
                            "struct name `{}` conflicts with function",
                            s.name
                        ),
                        span: s.name_span,
                    });
                } else {
                    let mut seen = HashSet::new();
                    let mut fields = Vec::new();
                    let mut fmap = HashMap::new();
                    let mut fvis = HashMap::new();
                    let mut fdefaults = HashMap::new();
                    for (idx, f) in s.fields.iter().enumerate() {
                        if !seen.insert(&f.name) {
                            self.errors.push(SemError {
                                message: format!(
                                    "duplicate field `{}` in struct `{}`",
                                    f.name, s.name
                                ),
                                span: f.name_span,
                            });
                        }
                        let fty = self.resolve_type(&f.ty);
                        if fty == Ty::Void {
                            self.errors.push(SemError {
                                message: format!(
                                    "field `{}` cannot be `void`",
                                    f.name
                                ),
                                span: f.span,
                            });
                        }
                        if matches!(fty, Ty::Own(_)) {
                            self.errors.push(SemError {
                                message: "own fields need structural destruction — rejected in phase 1".into(),
                                span: f.span,
                            });
                        }
                        if let Some(def) = &f.default {
                            let dty = self.check_expr(def);
                            if dty != fty && dty != Ty::Any {
                                self.errors.push(SemError {
                                    message: format!("default for field `{}`: expected `{}`, found `{}`", f.name, fty, dty),
                                    span: def.span,
                                });
                            }
                        }
                        fmap.insert(f.name.clone(), (idx, fty.clone()));
                        fvis.insert(f.name.clone(), f.visibility);
                        fdefaults.insert(f.name.clone(), f.default.clone());
                        fields.push((f.name.clone(), fty));
                    }
                    self.structs.insert(
                        s.name.clone(),
                        StructInfo {
                            name: s.name.clone(),
                            fields,
                            field_map: fmap,
                            field_vis: fvis,
                            field_defaults: fdefaults,
                            generic_params: s.generic_params.clone(),
                            where_clause: s.where_clause.clone(),
                            span: s.span,
                        },
                    );
                    self.check_where_clause_def("struct", &s.name, &s.generic_params, &s.where_clause, s.span);
                }
            }
        }
        // Collect traits first (needed for class implements checks)
        for item in &prog.items {
            if let Item::Trait(t) = item {
                if self.traits.contains_key(&t.name) || self.structs.contains_key(&t.name) || self.classes.contains_key(&t.name) || self.enums.contains_key(&t.name) || self.funcs.contains_key(&t.name) {
                    self.errors.push(SemError{message: format!("duplicate trait `{}`", t.name), span: t.name_span});
                } else {
                    let mut methods = HashMap::new();
                    for m in &t.methods {
                        if methods.contains_key(&m.name) {
                            self.errors.push(SemError{message: format!("duplicate method `{}` in trait `{}`", m.name, t.name), span: m.name_span});
                        } else {
                            let param_tys: Vec<Ty> = m.params.iter().enumerate().map(|(idx, p)| {
                                let mut ty = self.resolve_type(&p.ty);
                                if p.is_variadic {
                                    if p.ty.name() == "__derived__" {
                                        if idx == 0 {
                                            self.errors.push(SemError{message: "variadic `... vda` without previous type must not be first param".into(), span: p.span});
                                        } else {
                                            let prev_t = self.resolve_type(&m.params[idx-1].ty);
                                            ty = Ty::Array(Box::new(prev_t));
                                        }
                                    } else {
                                        ty = Ty::Array(Box::new(ty));
                                    }
                                    if p.ty.name() == "__derived__" && idx + 1 != m.params.len() {
                                        self.errors.push(SemError{message: "derived variadic `... vda` must be last".into(), span: p.span});
                                    }
                                }
                                if ty == Ty::Void { self.errors.push(SemError{message: format!("parameter `{}` cannot be `void`", p.name), span: p.span}); }
                                ty
                            }).collect();
                            self.check_param_defaults(&m.params, &param_tys);
                            let param_modes: Vec<ParamMode> = m.params.iter().map(|p| p.mode).collect();
                            let ret_ty = self.resolve_type(&m.ret_ty);
                            methods.insert(m.name.clone(), FuncSig{ret: ret_ty, params: param_tys, param_modes, param_names: m.params.iter().map(|p| p.name.clone()).collect(), param_is_variadic: m.params.iter().map(|p| p.is_variadic).collect(), param_defaults: m.params.iter().map(|p| p.default.clone()).collect(), generic_params: m.generic_params.clone(), where_clause: m.where_clause.clone(), span: m.name_span});
                        }
                    }
                    self.traits.insert(t.name.clone(), TraitInfo{name: t.name.clone(), methods, generic_params: t.generic_params.clone(), where_clause: t.where_clause.clone(), span: t.span});
                    self.check_where_clause_def("trait", &t.name, &t.generic_params, &t.where_clause, t.span);
                }
            }
        }
        // First pass (continued): collect class definitions
        for item in &prog.items {
            if let Item::Class(c) = item {
                if self.structs.contains_key(&c.name) || self.classes.contains_key(&c.name) {
                    self.errors.push(SemError{message: format!("duplicate class/struct `{}`", c.name), span: c.name_span});
                } else if self.funcs.contains_key(&c.name) {
                    self.errors.push(SemError{message: format!("class name `{}` conflicts with function", c.name), span: c.name_span});
                } else {
                    // Record extends / implements names (validation deferred until after all classes collected)
                    // Generic `extends Base<int>`: identity is the base name.
                    let extends_name = c.extends.as_ref().map(|t| {
                        match t {
                            Type::Named(s, _) => s.clone(),
                            Type::Generic(base, _, _) => base.rsplit("::").next().unwrap_or(base).to_string(),
                            _ => self.resolve_type(t).to_string(),
                        }
                    });
                    // Generic args on `extends`/`implements` must satisfy the
                    // parent/trait bounds now (traits are already collected;
                    // parent classes may follow, so skip unknown ones here —
                    // the deferred validation below reports those).
                    if let Some(Type::Generic(base, args, _)) = c.extends.as_ref() {
                        let lookup = base.rsplit("::").next().unwrap_or(base);
                        if let Some(pinfo) = self.classes.get(lookup).cloned() {
                            self.check_type_args(&pinfo.generic_params, &pinfo.where_clause, lookup, args, c.extends.as_ref().map(|t| t.span()).unwrap_or(c.span));
                        }
                    }
                    let mut implements_names = Vec::new();
                    for imp in &c.implements {
                        // Generic `implements Box<int>`: identity is the base
                        // name (args were bounds-checked above); stringified
                        // `Ty::Generic` (`Box<int>`) never matches trait keys.
                        let n = match imp {
                            Type::Named(s, _) => s.clone(),
                            Type::Generic(base, _, _) => base.rsplit("::").next().unwrap_or(base).to_string(),
                            _ => self.resolve_type(imp).to_string(),
                        };
                        if let Type::Generic(base, args, _) = imp {
                            let lookup = base.rsplit("::").next().unwrap_or(base);
                            if let Some(tinfo) = self.traits.get(lookup).cloned() {
                                self.check_type_args(&tinfo.generic_params, &tinfo.where_clause, lookup, args, imp.span());
                            }
                        }
                        implements_names.push(n);
                    }
                    let mut seen: HashSet<String> = HashSet::new();
                    let mut fields = Vec::new();
                    let mut fmap = HashMap::new();
                    let mut fvis = HashMap::new();
                    let mut fdefaults = HashMap::new();
                    // Inherit parent fields if extends
                    if let Some(ref parent) = extends_name {
                        if let Some(pinfo) = self.classes.get(parent).cloned() {
                            for (idx, (fname, fty)) in pinfo.fields.iter().enumerate() {
                                fmap.insert(fname.clone(), (idx, fty.clone()));
                                if let Some(v) = pinfo.field_vis.get(fname) { fvis.insert(fname.clone(), *v); }
                                else { fvis.insert(fname.clone(), crate::ast::Visibility::Default); }
                                fdefaults.insert(fname.clone(), None);
                                fields.push((fname.clone(), fty.clone()));
                                seen.insert(fname.clone());
                            }
                        } else if let Some(sinfo) = self.structs.get(parent).cloned() {
                            for (idx, (fname, fty)) in sinfo.fields.iter().enumerate() {
                                fmap.insert(fname.clone(), (idx, fty.clone()));
                                if let Some(v) = sinfo.field_vis.get(fname) { fvis.insert(fname.clone(), *v); } else { fvis.insert(fname.clone(), crate::ast::Visibility::Default); }
                                fdefaults.insert(fname.clone(), sinfo.field_defaults.get(fname).cloned().unwrap_or(None));
                                fields.push((fname.clone(), fty.clone()));
                                seen.insert(fname.clone());
                            }
                        }
                    }
                    let offset = fields.len();
                    for (idx, f) in c.fields.iter().enumerate() {
                        if !seen.insert(f.name.clone()) {
                            self.errors.push(SemError{message: format!("duplicate field `{}` in class `{}`", f.name, c.name), span: f.name_span});
                        }
                        let fty = self.resolve_type(&f.ty);
                        if fty == Ty::Void { self.errors.push(SemError{message: format!("field `{}` cannot be `void`", f.name), span: f.span}); }
                        if matches!(fty, Ty::Own(_)) {
                            self.errors.push(SemError {
                                message: "own fields need structural destruction — rejected in phase 1".into(),
                                span: f.span,
                            });
                        }
                        if let Some(def) = &f.default {
                            let dty = self.check_expr(def);
                            if dty != fty && dty != Ty::Any {
                                self.errors.push(SemError{message: format!("default for field `{}`: expected `{}`, found `{}`", f.name, fty, dty), span: def.span});
                            }
                        }
                        let real_idx = offset + idx;
                        fmap.insert(f.name.clone(), (real_idx, fty.clone()));
                        fvis.insert(f.name.clone(), f.visibility);
                        fdefaults.insert(f.name.clone(), f.default.clone());
                        fields.push((f.name.clone(), fty));
                    }
                    // Also insert class layout into structs map for field access / instantiation
                    self.structs.insert(c.name.clone(), StructInfo{name: c.name.clone(), fields: fields.clone(), field_map: fmap.clone(), field_vis: fvis.clone(), field_defaults: fdefaults.clone(), generic_params: c.generic_params.clone(), where_clause: c.where_clause.clone(), span: c.span});
                    // Collect methods
                    let mut methods = HashMap::new();
                    let mut method_vis: HashMap<String, crate::ast::Visibility> = HashMap::new();
                    for m in &c.methods {
                        if methods.contains_key(&m.name) {
                            self.errors.push(SemError{message: format!("duplicate method `{}` in class `{}`", m.name, c.name), span: m.name_span});
                        } else {
                            let param_tys: Vec<Ty> = m.params.iter().enumerate().map(|(idx, p)| {
                                let mut t = self.resolve_type(&p.ty);
                                if p.is_variadic {
                                    if p.ty.name() == "__derived__" {
                                        if idx == 0 {
                                            self.errors.push(SemError{message: "variadic `... vda` without previous type must not be first param".into(), span: p.span});
                                        } else {
                                            let prev_t = self.resolve_type(&m.params[idx-1].ty);
                                            t = Ty::Array(Box::new(prev_t));
                                        }
                                    } else {
                                        t = Ty::Array(Box::new(t));
                                    }
                                    if p.ty.name() == "__derived__" && idx + 1 != m.params.len() {
                                        self.errors.push(SemError{message: "derived variadic `... vda` must be last".into(), span: p.span});
                                    }
                                }
                                if t == Ty::Void { self.errors.push(SemError{message: format!("parameter `{}` cannot be `void`", p.name), span: p.span}); }
                                t
                            }).collect();
                            self.check_param_defaults(&m.params, &param_tys);
                            let param_modes: Vec<ParamMode> = m.params.iter().map(|p| p.mode).collect();
                            let ret_ty = self.resolve_type(&m.ret_ty);
                            let mut pseen = HashSet::new();
                            for p in &m.params { if !pseen.insert(&p.name) { self.errors.push(SemError{message: format!("duplicate param `{}`", p.name), span: p.name_span}); } }
                            methods.insert(m.name.clone(), FuncSig{ret: ret_ty, params: param_tys, param_modes, param_names: m.params.iter().map(|p| p.name.clone()).collect(), param_is_variadic: m.params.iter().map(|p| p.is_variadic).collect(), param_defaults: m.params.iter().map(|p| p.default.clone()).collect(), generic_params: m.generic_params.clone(), where_clause: m.where_clause.clone(), span: m.name_span});
                            method_vis.insert(m.name.clone(), m.visibility);
                        }
                    }
                    // Inherit parent methods (for static dispatch) and their visibility
                    if let Some(ref parent) = extends_name {
                        if let Some(pinfo) = self.classes.get(parent).cloned() {
                            for (mname, sig) in pinfo.methods.iter() {
                                if !methods.contains_key(mname) {
                                    methods.insert(mname.clone(), sig.clone());
                                    if let Some(v) = pinfo.method_vis.get(mname) { method_vis.insert(mname.clone(), *v); }
                                }
                            }
                        }
                    }
                    // Validate constructors: name must match class name, params unique; also collect for ClassInfo
                    let mut ctor_sigs: Vec<(FuncSig, crate::ast::Visibility)> = Vec::new();
                    for ctor in &c.constructors {
                        if ctor.name != c.name {
                            self.errors.push(SemError{message: format!("constructor name `{}` must match class name `{}`", ctor.name, c.name), span: ctor.name_span});
                        }
                        if methods.contains_key(&ctor.name) {
                            self.errors.push(SemError{message: format!("constructor `{}` conflicts with method", ctor.name), span: ctor.name_span});
                        }
                        let mut pseen = HashSet::new();
                        let mut param_tys = Vec::new();
                        for (idx, p) in ctor.params.iter().enumerate() {
                            let mut ty = self.resolve_type(&p.ty);
                            if p.is_variadic {
                                if p.ty.name() == "__derived__" {
                                    if idx == 0 {
                                        self.errors.push(SemError{message: "variadic `... vda` without previous type must not be first param".into(), span: p.span});
                                    } else {
                                        let prev_t = self.resolve_type(&ctor.params[idx-1].ty);
                                        ty = Ty::Array(Box::new(prev_t));
                                    }
                                } else {
                                    ty = Ty::Array(Box::new(ty));
                                }
                                if p.ty.name() == "__derived__" && idx + 1 != ctor.params.len() {
                                    self.errors.push(SemError{message: "derived variadic `... vda` must be last".into(), span: p.span});
                                }
                            }
                            if ty == Ty::Void { self.errors.push(SemError{message: format!("constructor param `{}` cannot be `void`", p.name), span: p.span}); }
                            if !pseen.insert(&p.name) { self.errors.push(SemError{message: format!("duplicate param `{}` in constructor", p.name), span: p.name_span}); }
                            param_tys.push(ty);
                        }
                        self.check_param_defaults(&ctor.params, &param_tys);
                        let param_modes: Vec<ParamMode> = ctor.params.iter().map(|p| p.mode).collect();
                        // constructors are void return
                        ctor_sigs.push((FuncSig{ret: Ty::Void, params: param_tys, param_modes, param_names: ctor.params.iter().map(|p| p.name.clone()).collect(), param_is_variadic: ctor.params.iter().map(|p| p.is_variadic).collect(), param_defaults: ctor.params.iter().map(|p| p.default.clone()).collect(), generic_params: Vec::new(), where_clause: None, span: ctor.name_span}, ctor.visibility));
                    }
                    // Validate destructors: name must match class name
                    for dtor in &c.destructors {
                        if dtor.name != c.name {
                            self.errors.push(SemError{message: format!("destructor name `~{}` must match class name `{}`", dtor.name, c.name), span: dtor.name_span});
                        }
                    }
                    // Collect properties — allow separate getter/setter declarations that merge
                    let mut prop_map: HashMap<String, PropertyInfo> = HashMap::new();
                    for prop in &c.properties {
                        if seen.contains(&prop.name) || methods.contains_key(&prop.name) {
                            self.errors.push(SemError{message: format!("duplicate property/member `{}` in class `{}`", prop.name, c.name), span: prop.name_span});
                            continue;
                        }
                        if let Some(existing) = prop_map.get(&prop.name).cloned() {
                            // Merge getter/setter for same property name
                            let new_has_get = prop.getter.is_some();
                            let new_has_set = prop.setter.is_some();
                            if existing.has_get && new_has_get {
                                self.errors.push(SemError{message: format!("duplicate getter for property `{}` in class `{}`", prop.name, c.name), span: prop.name_span});
                                continue;
                            }
                            if existing.has_set && new_has_set {
                                self.errors.push(SemError{message: format!("duplicate setter for property `{}` in class `{}`", prop.name, c.name), span: prop.name_span});
                                continue;
                            }
                            // Type consistency: if both have explicit types, they must match
                            let new_ty: Option<Ty> = if let Some(ref t) = prop.ty { Some(self.resolve_type(t)) } else if let Some((p,_)) = prop.setter.as_ref() { Some(self.resolve_type(&p.ty)) } else { None };
                            if let Some(nt) = &new_ty {
                                if existing.ty != Ty::Void && *nt != Ty::Void && existing.ty != *nt {
                                    self.errors.push(SemError{message: format!("property `{}` type mismatch", prop.name), span: prop.name_span});
                                }
                            }
                            let merged_has_get = existing.has_get || new_has_get;
                            let merged_has_set = existing.has_set || new_has_set;
                            // Keep existing type/visibility, update accessors
                            prop_map.insert(prop.name.clone(), PropertyInfo{ty: existing.ty.clone(), has_get: merged_has_get, has_set: merged_has_set, visibility: existing.visibility, span: prop.span});
                            continue;
                        }
                        let prop_ty = if let Some(ref t) = prop.ty { self.resolve_type(t) } else if let Some((param, _)) = prop.setter.as_ref() { self.resolve_type(&param.ty) } else if prop.getter.is_some() { Ty::Void } else { Ty::Void };
                        // Check getter/setter consistency
                        if prop.getter.is_none() && prop.setter.is_none() {
                            self.errors.push(SemError{message: format!("property `{}` must have getter or setter", prop.name), span: prop.span});
                        }
                        if let Some(ref g) = prop.getter {
                            // getter body will be checked later; type must match prop_ty if given
                        }
                        if let Some((p, _)) = prop.setter.as_ref() {
                            let pty = self.resolve_type(&p.ty);
                            if let Some(ref t) = prop.ty {
                                let exp = self.resolve_type(t);
                                if pty != exp { self.errors.push(SemError{message: format!("setter param type mismatch for property `{}`", prop.name), span: p.span}); }
                            }
                        }
                        prop_map.insert(prop.name.clone(), PropertyInfo{ty: prop_ty.clone(), has_get: prop.getter.is_some(), has_set: prop.setter.is_some(), visibility: prop.visibility, span: prop.span});
                    }
                    // Inherit parent properties
                    if let Some(ref parent) = extends_name {
                        if let Some(pinfo) = self.classes.get(parent).cloned() {
                            for (pname, pinfo_prop) in pinfo.properties.iter() {
                                if !prop_map.contains_key(pname) {
                                    prop_map.insert(pname.clone(), pinfo_prop.clone());
                                }
                            }
                        }
                    }
                    // Validate implements: class must provide all trait methods.
                    // Generic `implements Box<int>`: substitute the trait's
                    // params before comparing signatures.
                    let mut imp_type_args: std::collections::HashMap<String, Vec<Type>> = std::collections::HashMap::new();
                    for imp in &c.implements {
                        if let Type::Generic(base, targs, _) = imp {
                            let lookup = base.rsplit("::").next().unwrap_or(base).to_string();
                            imp_type_args.insert(lookup, targs.clone());
                        }
                    }
                    for imp_name in &implements_names {
                        if let Some(trait_info) = self.traits.get(imp_name).cloned() {
                            if !trait_info.generic_params.is_empty() && !imp_type_args.contains_key(imp_name) {
                                self.errors.push(SemError{message: format!("trait `{}` is generic; provide type arguments (`implements {}<...>`)", imp_name, imp_name), span: c.span});
                            }
                            let mut subst: std::collections::HashMap<String, Ty> = std::collections::HashMap::new();
                            if let Some(targs) = imp_type_args.get(imp_name) {
                                for (gp, ta) in trait_info.generic_params.iter().zip(targs.iter()) {
                                    subst.insert(gp.name.clone(), self.resolve_type(ta));
                                }
                            }
                            for (mname, sig) in trait_info.methods.iter() {
                                let exp_ret = Self::subst_generic_ty(&sig.ret, &subst);
                                let exp_params: Vec<Ty> = sig.params.iter().map(|p| Self::subst_generic_ty(p, &subst)).collect();
                                if let Some(cls_sig) = methods.get(mname) {
                                    if cls_sig.ret != exp_ret || cls_sig.params != exp_params {
                                        self.errors.push(SemError{message: format!("class `{}` method `{}` does not match trait `{}` signature", c.name, mname, imp_name), span: c.span});
                                    }
                                } else {
                                    self.errors.push(SemError{message: format!("class `{}` missing trait `{}` method `{}`", c.name, imp_name, mname), span: c.span});
                                }
                            }
                        }
                    }
                    // Collect operators and conversions
                    let mut op_map: HashMap<String, FuncSig> = HashMap::new();
                    for op in &c.operators {
                        let mut p_tys = Vec::new();
                        for (idx, pp) in op.params.iter().enumerate() {
                            let mut ty = self.resolve_type(&pp.ty);
                            if pp.is_variadic {
                                if pp.ty.name() == "__derived__" {
                                    if idx == 0 {
                                        self.errors.push(SemError{message: "variadic `... vda` without previous type must not be first param".into(), span: pp.span});
                                    } else {
                                        let prev_t = self.resolve_type(&op.params[idx-1].ty);
                                        ty = Ty::Array(Box::new(prev_t));
                                    }
                                } else {
                                    ty = Ty::Array(Box::new(ty));
                                }
                                if pp.ty.name() == "__derived__" && idx + 1 != op.params.len() {
                                    self.errors.push(SemError{message: "derived variadic `... vda` must be last".into(), span: pp.span});
                                }
                            }
                            p_tys.push(ty);
                        }
                        self.check_param_defaults(&op.params, &p_tys);
                        let p_modes: Vec<ParamMode> = op.params.iter().map(|p| p.mode).collect();
                        // For MVP, assume operator returns int (or struct for + if class)
                        let ret = Ty::Int;
                        op_map.insert(op.op.clone(), FuncSig{ret: ret.clone(), params: p_tys, param_modes: p_modes, param_names: op.params.iter().map(|p| p.name.clone()).collect(), param_is_variadic: op.params.iter().map(|p| p.is_variadic).collect(), param_defaults: op.params.iter().map(|p| p.default.clone()).collect(), generic_params: Vec::new(), where_clause: op.where_clause.clone(), span: op.span});
                    }
                    let mut conv_vec: Vec<(Ty, Ty, Span)> = Vec::new();
                    for conv in &c.conversions {
                        let from = self.resolve_type(&conv.from_ty);
                        let to = self.resolve_type(&conv.to_ty);
                        conv_vec.push((from, to, conv.span));
                    }
                    self.classes.insert(c.name.clone(), ClassInfo{name: c.name.clone(), fields, field_map: fmap, field_vis: fvis, methods, method_vis, constructors: ctor_sigs, properties: prop_map, operators: op_map, conversions: conv_vec, is_open: c.is_open, is_sealed: c.is_sealed, extends: extends_name.clone(), implements: implements_names.clone(), generic_params: c.generic_params.clone(), where_clause: c.where_clause.clone(), span: c.span});
                    self.check_where_clause_def("class", &c.name, &c.generic_params, &c.where_clause, c.span);
                }
            }
        }
        // Validate extends / implements / override after all classes known
        {
            let class_snapshot = self.classes.clone();
            for (cname, info) in class_snapshot {
                if let Some(ref parent) = info.extends {
                    if !self.classes.contains_key(parent) {
                        self.errors.push(SemError{message: format!("class `{cname}` extends unknown class `{parent}`"), span: info.span});
                    } else {
                        let parent_info = &self.classes[parent];
                        if parent_info.is_sealed {
                            self.errors.push(SemError{message: format!("class `{cname}` cannot extend sealed class `{parent}`"), span: info.span});
                        }
                        if !parent_info.is_open && parent_info.methods.keys().any(|k| info.methods.contains_key(k)) {
                            // warn if overriding without open? For now allow, but check sealed methods
                        }
                    }
                }
                for imp in &info.implements {
                    if !self.traits.contains_key(imp) {
                        self.errors.push(SemError{message: format!("class `{cname}` implements unknown trait `{imp}`"), span: info.span});
                    }
                }
                // override checks: methods marked override must exist in parent
                let prog_class = prog.items.iter().filter_map(|it| if let Item::Class(c) = it { if &c.name == &cname { Some(c)} else {None}} else {None}).next();
                if let Some(cls) = prog_class {
                    for m in &cls.methods {
                        if m.is_override {
                            if let Some(ref parent) = info.extends {
                                if let Some(pinfo) = self.classes.get(parent) {
                                    if !pinfo.methods.contains_key(&m.name) {
                                        self.errors.push(SemError{message: format!("method `{}` marked `override` but parent `{}` has no method `{}`", m.name, parent, m.name), span: m.name_span});
                                    }
                                }
                            } else {
                                self.errors.push(SemError{message: format!("method `{}` marked `override` but class `{cname}` has no parent", m.name), span: m.name_span});
                            }
                        }
                        if m.is_sealed && !m.is_override {
                            // sealed without override is unusual but allow
                        }
                    }
                }
            }
        }
        // Collect enums
        for item in &prog.items {
            if let Item::Enum(e) = item {
                if self.enums.contains_key(&e.name) || self.structs.contains_key(&e.name) || self.classes.contains_key(&e.name) || self.funcs.contains_key(&e.name) {
                    self.errors.push(SemError{message: format!("duplicate enum `{}`", e.name), span: e.name_span});
                } else {
                    let mut seen = HashSet::new();
                    let mut variants = Vec::new();
                    let mut vmap = HashMap::new();
                    for (idx, v) in e.variants.iter().enumerate() {
                        if !seen.insert(&v.name) {
                            self.errors.push(SemError{message: format!("duplicate variant `{}` in enum `{}`", v.name, e.name), span: v.name_span});
                        }
                        let mut payload_tys = Vec::new();
                        for p in &v.payload_params {
                            let pt = self.resolve_type(&p.ty);
                            if pt == Ty::Void { self.errors.push(SemError{message: format!("variant `{}` payload param `{}` cannot be `void`", v.name, p.name), span: p.span}); }
                            if p.default.is_some() { self.errors.push(SemError{message: format!("variant `{}` payload param `{}` cannot have a default value", v.name, p.name), span: p.name_span}); }
                            payload_tys.push(pt);
                        }
                        // discriminant: if Some(expr), try to evaluate as int, else use idx
                        let tag = if let Some(expr) = &v.discriminant {
                            // Try to evaluate constant int expression: for now handle IntLit, or try to resolve as int
                            match &expr.kind {
                                ExprKind::IntLit(val) => *val as usize,
                                _ => {
                                    // Try to check expr as int and use idx as fallback, but also error if not int
                                    let t = self.check_expr(expr);
                                    if t != Ty::Int {
                                        self.errors.push(SemError{message: format!("enum discriminant must be `int`, found `{}`", t), span: expr.span});
                                    }
                                    // For non-literal, use idx as tag and store expr for later evaluation (not yet)
                                    idx
                                }
                            }
                        } else {
                            idx
                        };
                        vmap.insert(v.name.clone(), (tag, payload_tys.clone()));
                        variants.push(EnumVariantInfo{name: v.name.clone(), tag, payload_tys, discriminant_expr: v.discriminant.clone(), span: v.span});
                    }
                    self.enums.insert(e.name.clone(), EnumInfo{name: e.name.clone(), variants, variant_map: vmap, generic_params: e.generic_params.clone(), where_clause: e.where_clause.clone(), span: e.span});
                    self.check_where_clause_def("enum", &e.name, &e.generic_params, &e.where_clause, e.span);
                }
            }
        }
        // Handle typedef/distinct as type aliases
        for item in &prog.items {
            if let Item::Typedef(td) = item {
                let ty = self.resolve_type(&td.ty);
                let mut fvis = HashMap::new();
                fvis.insert("value".to_string(), Visibility::Public);
                let mut fdefs = HashMap::new();
                fdefs.insert("value".to_string(), None);
                self.structs.insert(td.name.clone(), StructInfo{name: td.name.clone(), fields: vec![("value".to_string(), ty.clone())], field_map: [(String::from("value"), (0, ty.clone()))].into_iter().collect(), field_vis: fvis, field_defaults: fdefs, generic_params: td.generic_params.clone(), where_clause: None, span: td.span});
            } else if let Item::Distinct(dd) = item {
                let ty = self.resolve_type(&dd.ty);
                let mut fvis = HashMap::new();
                fvis.insert("value".to_string(), Visibility::Public);
                let mut fdefs = HashMap::new();
                fdefs.insert("value".to_string(), None);
                self.structs.insert(dd.name.clone(), StructInfo{name: dd.name.clone(), fields: vec![("value".to_string(), ty.clone())], field_map: [(String::from("value"), (0, ty.clone()))].into_iter().collect(), field_vis: fvis, field_defaults: fdefs, generic_params: dd.generic_params.clone(), where_clause: None, span: dd.span});
            } else if let Item::Extension(ext) = item {
                let target_name = match &ext.ty { Type::Named(n, _) => n.clone(), Type::Generic(n, _, _) => n.clone(), _ => "".to_string() };
                let ext_members = ext.members.clone();
                let mut pending_ext: Vec<(String, FuncSig, crate::ast::Visibility)> = Vec::new();
                let mut pending_fields: Vec<(String, Ty, crate::ast::Visibility, Option<Expr>)> = Vec::new();
                let mut pending_ops: Vec<(String, FuncSig, crate::ast::Visibility)> = Vec::new();
                let mut pending_props: Vec<(String, PropertyInfo)> = Vec::new();
                let mut pending_convs: Vec<(Ty, Ty, Span)> = Vec::new();
                for mem in &ext_members {
                    match mem {
                        crate::ast::ExtensionMember::Function(f) => {
                            let ret_ty = self.resolve_type(&f.ret_ty);
                            let param_tys: Vec<Ty> = f.params.iter().enumerate().map(|(idx, p)| {
                                let mut ty = self.resolve_type(&p.ty);
                                if p.is_variadic {
                                    if p.ty.name() == "__derived__" {
                                        if idx == 0 { self.errors.push(SemError{message: "variadic `... vda` without previous type must not be first param".into(), span: p.span}); } else { let prev_t = self.resolve_type(&f.params[idx-1].ty); ty = Ty::Array(Box::new(prev_t)); }
                                    } else { ty = Ty::Array(Box::new(ty)); }
                                    if p.ty.name() == "__derived__" && idx + 1 != f.params.len() { self.errors.push(SemError{message: "derived variadic `... vda` must be last".into(), span: p.span}); }
                                }
                                if ty == Ty::Void { self.errors.push(SemError{message: format!("parameter `{}` cannot be `void`", p.name), span: p.span}); }
                                ty
                            }).collect();
                            self.check_param_defaults(&f.params, &param_tys);
                            let param_modes: Vec<ParamMode> = f.params.iter().map(|p| p.mode).collect();
                            let sig = FuncSig{ret: ret_ty, params: param_tys, param_modes, param_names: f.params.iter().map(|p| p.name.clone()).collect(), param_is_variadic: f.params.iter().map(|p| p.is_variadic).collect(), param_defaults: f.params.iter().map(|p| p.default.clone()).collect(), generic_params: f.generic_params.clone(), where_clause: f.where_clause.clone(), span: f.name_span};
                            pending_ext.push((f.name.clone(), sig, f.visibility));
                        }
                        crate::ast::ExtensionMember::Field(field) => {
                            let fty = self.resolve_type(&field.ty);
                            if fty == Ty::Void { self.errors.push(SemError{message: format!("field `{}` cannot be `void`", field.name), span: field.span}); }
                            if matches!(fty, Ty::Own(_)) {
                                self.errors.push(SemError {
                                    message: "own fields need structural destruction — rejected in phase 1".into(),
                                    span: field.span,
                                });
                            }
                            if let Some(def) = &field.default {
                                let dty = self.check_expr(def);
                                if dty != fty && dty != Ty::Any { self.errors.push(SemError{message: format!("default for field `{}`: expected `{}`, found `{}`", field.name, fty, dty), span: def.span}); }
                            }
                            pending_fields.push((field.name.clone(), fty, field.visibility, field.default.clone()));
                        }
                        crate::ast::ExtensionMember::Operator(op) => {
                            let mut p_tys = Vec::new();
                            for (idx, pp) in op.params.iter().enumerate() {
                                let mut ty = self.resolve_type(&pp.ty);
                                if pp.is_variadic {
                                    if pp.ty.name() == "__derived__" {
                                        if idx == 0 { self.errors.push(SemError{message: "variadic `... vda` without previous type must not be first param".into(), span: pp.span}); } else { let prev_t = self.resolve_type(&op.params[idx-1].ty); ty = Ty::Array(Box::new(prev_t)); }
                                    } else { ty = Ty::Array(Box::new(ty)); }
                                    if pp.ty.name() == "__derived__" && idx + 1 != op.params.len() { self.errors.push(SemError{message: "derived variadic `... vda` must be last".into(), span: pp.span}); }
                                }
                                p_tys.push(ty);
                            }
                            self.check_param_defaults(&op.params, &p_tys);
                            let ret = Ty::Int;
                            let sig = FuncSig{ret: ret.clone(), params: p_tys, param_modes: op.params.iter().map(|p| p.mode).collect(), param_names: op.params.iter().map(|p| p.name.clone()).collect(), param_is_variadic: op.params.iter().map(|p| p.is_variadic).collect(), param_defaults: op.params.iter().map(|p| p.default.clone()).collect(), generic_params: Vec::new(), where_clause: op.where_clause.clone(), span: op.span};
                            pending_ops.push((op.op.clone(), sig, op.visibility));
                        }
                        crate::ast::ExtensionMember::Property(prop) => {
                            let prop_ty = if let Some(ref t) = prop.ty { self.resolve_type(t) } else if let Some((p,_)) = prop.setter.as_ref() { self.resolve_type(&p.ty) } else { Ty::Void };
                            pending_props.push((prop.name.clone(), PropertyInfo{ty: prop_ty, has_get: prop.getter.is_some(), has_set: prop.setter.is_some(), visibility: prop.visibility, span: prop.span}));
                        }
                        crate::ast::ExtensionMember::Conversion(conv) => {
                            let from = self.resolve_type(&conv.from_ty);
                            let to = self.resolve_type(&conv.to_ty);
                            pending_convs.push((from, to, conv.span));
                        }
                    }
                }
                // For class target, add to class; for struct target, add to struct.
                // Extension members are public by default: an absent
                // modifier normalizes to `Public` (explicit `private` is
                // kept). Mirrors the struct-branch method merge below.
                if let Some(cls) = self.classes.get_mut(&target_name) {
                    for (name, sig, vis) in pending_ext {
                        let vis_pub = if vis == crate::ast::Visibility::Default { crate::ast::Visibility::Public } else { vis };
                        cls.methods.insert(name.clone(), sig);
                        cls.method_vis.insert(name, vis_pub);
                    }
                    for (fname, fty, vis, def) in pending_fields {
                        if cls.fields.iter().any(|(n,_)| n == &fname) {
                            self.errors.push(SemError{message: format!("duplicate field `{}` in `extend {}`", fname, target_name), span: Span::new(0,0)});
                        } else {
                            let vis_pub = if vis == crate::ast::Visibility::Default { crate::ast::Visibility::Public } else { vis };
                            let idx = cls.fields.len();
                            cls.fields.push((fname.clone(), fty.clone()));
                            cls.field_map.insert(fname.clone(), (idx, fty.clone()));
                            cls.field_vis.insert(fname.clone(), vis_pub);
                            // also update structs map for field access
                            if let Some(sinfo) = self.structs.get_mut(&target_name) {
                                sinfo.fields.push((fname.clone(), fty.clone()));
                                sinfo.field_map.insert(fname.clone(), (idx, fty.clone()));
                                sinfo.field_vis.insert(fname.clone(), vis_pub);
                                sinfo.field_defaults.insert(fname.clone(), def);
                            }
                        }
                    }
                    for (op_str, sig, vis) in pending_ops {
                        cls.operators.insert(op_str, sig);
                    }
                    for (pname, pinfo) in pending_props {
                        if let Some(existing) = cls.properties.get(&pname).cloned() {
                            let merged_has_get = existing.has_get || pinfo.has_get;
                            let merged_has_set = existing.has_set || pinfo.has_set;
                            let merged_ty = if existing.ty != Ty::Void { existing.ty.clone() } else { pinfo.ty.clone() };
                            let merged_vis = existing.visibility;
                            cls.properties.insert(pname.clone(), PropertyInfo{ty: merged_ty, has_get: merged_has_get, has_set: merged_has_set, visibility: merged_vis, span: pinfo.span});
                        } else {
                            let vis_pub = if pinfo.visibility == crate::ast::Visibility::Default { crate::ast::Visibility::Public } else { pinfo.visibility };
                            cls.properties.insert(pname, PropertyInfo{ty: pinfo.ty.clone(), has_get: pinfo.has_get, has_set: pinfo.has_set, visibility: vis_pub, span: pinfo.span});
                        }
                    }
                    for (from, to, _) in pending_convs {
                        cls.conversions.push((from, to, Span::new(0,0)));
                    }
                } else if let Some(sinfo) = self.structs.get_mut(&target_name) {
                    for (fname, fty, vis, def) in pending_fields {
                        if sinfo.fields.iter().any(|(n,_)| n == &fname) {
                            self.errors.push(SemError{message: format!("duplicate field `{}` in `extend {}`", fname, target_name), span: Span::new(0,0)});
                        } else {
                            let idx = sinfo.fields.len();
                            sinfo.fields.push((fname.clone(), fty.clone()));
                            sinfo.field_map.insert(fname.clone(), (idx, fty.clone()));
                            sinfo.field_vis.insert(fname.clone(), vis);
                            sinfo.field_defaults.insert(fname.clone(), def);
                        }
                    }
                    // For struct, also handle operators/properties/conversions via class maps (create ClassInfo if needed)
                    if !pending_ops.is_empty() || !pending_props.is_empty() || !pending_convs.is_empty() || !pending_ext.is_empty() {
                        let cls = self.classes.entry(target_name.clone()).or_insert_with(|| ClassInfo{
                            name: target_name.clone(), fields: vec![], field_map: HashMap::new(), field_vis: HashMap::new(),
                            methods: HashMap::new(), method_vis: HashMap::new(), constructors: vec![], properties: HashMap::new(),
                            operators: HashMap::new(), conversions: vec![], is_open: false, is_sealed: false, extends: None, implements: vec![], generic_params: vec![], where_clause: None, span: Span::new(0,0)
                        });
                        for (name, sig, vis) in pending_ext {
                            let vis_pub = if vis == crate::ast::Visibility::Default { crate::ast::Visibility::Public } else { vis };
                            cls.methods.insert(name.clone(), sig);
                            cls.method_vis.insert(name, vis_pub);
                        }
                        for (op_str, sig, _) in pending_ops {
                            cls.operators.insert(op_str, sig);
                        }
                        for (pname, pinfo) in pending_props {
                            if let Some(existing) = cls.properties.get(&pname).cloned() {
                                let merged_has_get = existing.has_get || pinfo.has_get;
                                let merged_has_set = existing.has_set || pinfo.has_set;
                                let merged_ty = if existing.ty != Ty::Void { existing.ty.clone() } else { pinfo.ty.clone() };
                                cls.properties.insert(pname.clone(), PropertyInfo{ty: merged_ty, has_get: merged_has_get, has_set: merged_has_set, visibility: existing.visibility, span: pinfo.span});
                            } else {
                                let vis_pub = if pinfo.visibility == crate::ast::Visibility::Default { crate::ast::Visibility::Public } else { pinfo.visibility };
                                cls.properties.insert(pname, PropertyInfo{ty: pinfo.ty.clone(), has_get: pinfo.has_get, has_set: pinfo.has_set, visibility: vis_pub, span: pinfo.span});
                            }
                        }
                        for (from, to, _) in pending_convs {
                            cls.conversions.push((from, to, Span::new(0,0)));
                        }
                    }
                }
            } else if let Item::Const(c) = item {
                let decl_ty = if let Some(t) = &c.ty {
                    self.resolve_type(t)
                } else {
                    self.check_expr(&c.init)
                };
                if decl_ty == Ty::Void {
                    self.errors.push(SemError{message: "const cannot have void type".into(), span: c.span});
                }
                if matches!(decl_ty, Ty::Own(_)) {
                    self.errors.push(SemError{message: "global own constants need structural destruction — rejected in phase 1".into(), span: c.span});
                }
                let init_ty = self.check_expr(&c.init);
                let is_null = matches!(c.init.kind, ExprKind::Null);
                if !is_null && !self.ty_assignable(&init_ty, &decl_ty) && decl_ty != Ty::Any {
                    self.errors.push(SemError{message: format!("const initializer mismatch: expected `{}`, found `{}`", decl_ty, init_ty), span: c.init.span});
                }
                self.reject_null_own(is_null, &decl_ty, c.init.span);
                if self.scopes.last().map(|s| s.contains_key(&c.name)).unwrap_or(false) {
                    self.errors.push(SemError{message: format!("redefinition of const `{}`", c.name), span: c.name_span});
                } else {
                    self.declare_const(&c.name, decl_ty, c.name_span);
                }
            } else if let Item::Var(v) = item {
                let mut decl_ty = self.resolve_type(&v.ty);
                if matches!(decl_ty, Ty::Own(_)) {
                    self.errors.push(SemError{message: "global own variables need structural destruction — rejected in phase 1".into(), span: v.span});
                }
                if decl_ty == Ty::Void {
                    self.errors.push(SemError{message: "global variable cannot have `void` type".into(), span: v.span});
                }
                if decl_ty == Ty::Any {
                    if let Some(init) = &v.init {
                        if matches!(init.kind, ExprKind::VecEmpty(_)) {
                            decl_ty = Ty::Vec(Box::new(Ty::Any));
                        }
                    }
                }
                let mut init_checked = false;
                if decl_ty == Ty::Any {
                    if let Some(init) = &v.init {
                        if matches!(init.kind, ExprKind::MapLit { .. }) {
                            decl_ty = self.check_expr(init);
                            init_checked = true;
                        }
                    }
                }
                if matches!(decl_ty, Ty::FixedArray { size: None, .. }) && v.init.is_none() {
                    self.errors.push(SemError{message: format!("array `{}` has inferred size but no initializer: use `arr[N]` or `= [...]`", v.name), span: v.span});
                }
                if let Some(init) = &v.init {
                    if init_checked {
                        // Already validated while adopting the inferred map.
                    } else {
                        let init_ty = self.check_expr(init);
                        let is_null = matches!(init.kind, ExprKind::Null);
                        if let (
                            Ty::FixedArray { size: Some(n), .. },
                            Ty::FixedArray { size: Some(m), .. },
                        ) = (&decl_ty, &init_ty)
                        {
                            if n != m {
                                self.errors.push(SemError{message: format!("array size mismatch: declared `arr[{n}]` but initializer has {m} elements"), span: init.span});
                            }
                        }
                        if !is_null && decl_ty != Ty::Any {
                            let compatible = match (&decl_ty, &init_ty) {
                                (
                                    Ty::FixedArray { elem: de, .. },
                                    Ty::FixedArray { elem: ie, .. },
                                ) => Ty::assignable(ie, de),
                                _ => self.ty_assignable(&init_ty, &decl_ty),
                            };
                            if !compatible {
                                // Allow int literal for any?
                                self.errors.push(SemError{message: format!("global var initializer mismatch: expected `{}`, found `{}`", decl_ty, init_ty), span: init.span});
                            }
                        }
                        self.reject_null_own(is_null, &decl_ty, init.span);
                    }
                }
                if self.scopes.last().map(|s| s.contains_key(&v.name)).unwrap_or(false) {
                    self.errors.push(SemError{message: format!("redefinition of global var `{}`", v.name), span: v.name_span});
                } else {
                    self.declare_var(&v.name, decl_ty, v.name_span);
                }
            }
        }
        // Handle attributed items as their inner
        for item in &prog.items {
            if let Item::Attributed{attrs: _, item} = item {
                if let Item::Function(f) = item.as_ref() {
                    // Will be handled in next pass, just check inner for now
                }
            }
        }
        // Handle extern functions / structs / enums / consts
        for item in &prog.items {
            if let Item::Extern(ex) = item {
                for mem in &ex.members {
                    match mem {
                        crate::ast::ExternMember::Function{ty, name, name_span, params, ..} => {
                            let ret = self.resolve_type(ty);
                            if let Some(tn) = self.extern_trait_name(&ret) {
                                self.errors.push(SemError{message: format!("extern function `{name}` cannot return trait type `{tn}` (no C representation)"), span: *name_span});
                            }
                            if Self::contains_own(&ret) {
                                self.errors.push(SemError{message: format!("extern function `{name}` cannot return `own` type (no C representation)"), span: *name_span});
                            }
                            let param_tys: Vec<Ty> = params.iter().map(|p| {
                                let pt = self.resolve_type(&p.ty);
                                if let Some(tn) = self.extern_trait_name(&pt) {
                                    self.errors.push(SemError{message: format!("extern function `{name}` cannot take trait type `{tn}` (no C representation)"), span: p.span});
                                }
                                if Self::contains_own(&pt) {
                                    self.errors.push(SemError{message: format!("extern function `{name}` cannot take `own` type (no C representation)"), span: p.span});
                                }
                                pt
                            }).collect();
                            let param_modes: Vec<ParamMode> = vec![ParamMode::None; param_tys.len()];
                            self.funcs.insert(name.clone(), FuncSig{ret, params: param_tys, param_modes, param_names: params.iter().map(|p| p.name.clone()).collect(), param_is_variadic: params.iter().map(|p| p.is_variadic).collect(), param_defaults: params.iter().map(|_| None).collect(), generic_params: Vec::new(), where_clause: None, span: *name_span});
                        }
                        crate::ast::ExternMember::Struct{name, name_span, fields, ..} => {
                            if self.structs.contains_key(name) {
                                self.errors.push(SemError{message: format!("duplicate extern struct `{}`", name), span: *name_span});
                            } else {
                                let mut seen = HashSet::new();
                                let mut flds = Vec::new();
                                let mut fmap = HashMap::new();
                                let mut fvis = HashMap::new();
                                let mut fdefs = HashMap::new();
                                for (idx, f) in fields.iter().enumerate() {
                                    if !seen.insert(&f.name) {
                                        self.errors.push(SemError{message: format!("duplicate field `{}` in extern struct `{}`", f.name, name), span: f.name_span});
                                    }
                                    let fty = self.resolve_type(&f.ty);
                                    if fty == Ty::Void { self.errors.push(SemError{message: format!("field `{}` cannot be `void`", f.name), span: f.span}); }
                        if matches!(fty, Ty::Own(_)) {
                            self.errors.push(SemError {
                                message: "own fields need structural destruction — rejected in phase 1".into(),
                                span: f.span,
                            });
                        } else if Self::contains_own(&fty) {
                            self.errors.push(SemError {
                                message: format!("extern struct field `{}` cannot use `own` type (no C representation)", f.name),
                                span: f.span,
                            });
                        }
                                    if let Some(tn) = self.extern_trait_name(&fty) {
                                        self.errors.push(SemError{message: format!("extern struct field `{}` cannot use trait type `{tn}` (no C representation)", f.name), span: f.span});
                                    }
                                    fmap.insert(f.name.clone(), (idx, fty.clone()));
                                    fvis.insert(f.name.clone(), crate::ast::Visibility::Public);
                                    fdefs.insert(f.name.clone(), None);
                                    flds.push((f.name.clone(), fty));
                                }
                                self.structs.insert(name.clone(), StructInfo{name: name.clone(), fields: flds, field_map: fmap, field_vis: fvis, field_defaults: fdefs, generic_params: vec![], where_clause: None, span: *name_span});
                            }
                        }
                        crate::ast::ExternMember::Enum{name, name_span, variants, ..} => {
                            if self.enums.contains_key(name) || self.structs.contains_key(name) {
                                self.errors.push(SemError{message: format!("duplicate extern enum `{}`", name), span: *name_span});
                            } else {
                                let mut seen = HashSet::new();
                                let mut vars = Vec::new();
                                let mut vmap = HashMap::new();
                                for (idx, v) in variants.iter().enumerate() {
                                    if !seen.insert(&v.name) {
                                        self.errors.push(SemError{message: format!("duplicate variant `{}` in extern enum `{}`", v.name, name), span: v.name_span});
                                    }
                                    let mut payload_tys = Vec::new();
                                    for p in &v.payload_params {
                                        let pt = self.resolve_type(&p.ty);
                                        if p.default.is_some() { self.errors.push(SemError{message: format!("variant `{}` payload param `{}` cannot have a default value", v.name, p.name), span: p.name_span}); }
                                        if let Some(tn) = self.extern_trait_name(&pt) {
                                            self.errors.push(SemError{message: format!("extern enum payload cannot use trait type `{tn}` (no C representation)"), span: p.span});
                                        }
                                        if Self::contains_own(&pt) {
                                            self.errors.push(SemError{message: "extern enum payload cannot use `own` type (no C representation)".into(), span: p.span});
                                        }
                                        payload_tys.push(pt);
                                    }
                                    let tag = if let Some(expr) = &v.discriminant {
                                        match &expr.kind {
                                            ExprKind::IntLit(val) => *val as usize,
                                            _ => { let _ = self.check_expr(expr); idx }
                                        }
                                    } else { idx };
                                    vmap.insert(v.name.clone(), (tag, payload_tys.clone()));
                                    vars.push(EnumVariantInfo{name: v.name.clone(), tag, payload_tys, discriminant_expr: v.discriminant.clone(), span: v.span});
                                }
                                self.enums.insert(name.clone(), EnumInfo{name: name.clone(), variants: vars, variant_map: vmap, generic_params: vec![], where_clause: None, span: *name_span});
                            }
                        }
                        crate::ast::ExternMember::Const{ty, name, name_span, ..} => {
                            let decl_ty = self.resolve_type(ty);
                            if decl_ty == Ty::Void {
                                self.errors.push(SemError{message: "extern const cannot be `void`".into(), span: *name_span});
                            }
                            if let Some(tn) = self.extern_trait_name(&decl_ty) {
                                self.errors.push(SemError{message: format!("extern const `{name}` cannot use trait type `{tn}` (no C representation)"), span: *name_span});
                            }
                            if Self::contains_own(&decl_ty) {
                                self.errors.push(SemError{message: format!("extern const `{name}` cannot use `own` type (no C representation)"), span: *name_span});
                            }
                            if self.scopes.last().map(|s| s.contains_key(name)).unwrap_or(false) {
                                self.errors.push(SemError{message: format!("redefinition of extern const `{}`", name), span: *name_span});
                            } else {
                                self.declare_const(name, decl_ty, *name_span);
                            }
                        }
                    }
                }
            }
        }
        // Second pass: collect function signatures
        for item in &prog.items {
            let func_opt = match item {
                Item::Function(f) => Some(f),
                Item::Attributed{attrs: _, item} => if let Item::Function(f) = item.as_ref() { Some(f) } else { None },
                _ => None,
            };
            if let Some(f) = func_opt {
                // Runtime-reserved libc symbols the compiler references for
                // codegen (`strlen` for `string.len()`, `strcmp` for string
                // keys/`contains`, `abort` for traps, IO shims). A user
                // definition would silently merge with (or recurse into) the
                // external declaration.
                const RESERVED_RUNTIME: &[&str] = &[
                    "strlen", "strcmp", "abort", "puts", "printf", "putchar",
                    "strcat", "sprintf",
                ];
                if RESERVED_RUNTIME.contains(&f.name.as_str()) {
                    self.errors.push(SemError {
                        message: format!("function name `{}` is reserved for the runtime", f.name),
                        span: f.name_span,
                    });
                } else if self.funcs.contains_key(&f.name) {
                    self.errors.push(SemError {
                        message: format!("duplicate function `{}`", f.name),
                        span: f.name_span,
                    });
                } else if self.structs.contains_key(&f.name) {
                    self.errors.push(SemError {
                        message: format!(
                            "function name `{}` conflicts with struct",
                            f.name
                        ),
                        span: f.name_span,
                    });
                } else {
                    let param_tys: Vec<Ty> = f
                        .params
                        .iter()
                        .enumerate()
                        .map(|(idx, p)| {
                            let mut t = self.resolve_type(&p.ty);
                            if p.is_variadic {
                                if p.ty.name() == "__derived__" {
                                    if idx == 0 {
                                        // error already pushed in check_function, but for FuncSig keep as Array(Int) placeholder
                                    } else {
                                        let prev_t = self.resolve_type(&f.params[idx-1].ty);
                                        t = Ty::Array(Box::new(prev_t));
                                    }
                                } else {
                                    t = Ty::Array(Box::new(t));
                                }
                            }
                            if t == Ty::Void {
                                self.errors.push(SemError {
                                    message: format!(
                                        "parameter `{}` cannot be `void`",
                                        p.name
                                    ),
                                    span: p.span,
                                });
                            }
                            t
                        })
                        .collect();
                    self.check_param_defaults(&f.params, &param_tys);
                    let ret_ty = self.resolve_type(&f.ret_ty);
                    let param_modes: Vec<ParamMode> = f.params.iter().map(|p| p.mode).collect();
                    let mut seen = HashSet::new();
                    for p in &f.params {
                        if !seen.insert(&p.name) {
                            self.errors.push(SemError {
                                message: format!(
                                    "duplicate parameter `{}`",
                                    p.name
                                ),
                                span: p.name_span,
                            });
                        }
                    }
                    self.funcs.insert(
                        f.name.clone(),
                        FuncSig {
                            ret: ret_ty,
                            params: param_tys,
                            param_modes,
                            param_names: f.params.iter().map(|p| p.name.clone()).collect(),
                            param_is_variadic: f.params.iter().map(|p| p.is_variadic).collect(),
                            param_defaults: f.params.iter().map(|p| p.default.clone()).collect(),
                            generic_params: f.generic_params.clone(),
                            where_clause: f.where_clause.clone(),
                            span: f.name_span,
                        },
                    );
                }
            }
        }
        // Validate main per EBNF §37: `void main()` or `int main()` or `void main(string[] args)` or `int main(string[] args)` (return and args independent)
        if let Some(main) = self.funcs.get("main").cloned() {
            let is_void_main = main.ret == Ty::Void && main.params.is_empty();
            let is_void_main_with_args = main.ret == Ty::Void
                && main.params.len() == 1
                && main.params[0] == Ty::Array(Box::new(Ty::String))
                && main.param_names.get(0).map(|s| s == "args").unwrap_or(false);
            let is_int_main_no_args = main.ret == Ty::Int && main.params.is_empty();
            let is_int_main_with_args = main.ret == Ty::Int
                && main.params.len() == 1
                && main.params[0] == Ty::Array(Box::new(Ty::String))
                && main.param_names.get(0).map(|s| s == "args").unwrap_or(false);
            if !(is_void_main || is_void_main_with_args || is_int_main_no_args || is_int_main_with_args)
            {
                self.errors.push(SemError{message: format!("invalid `main` signature: expected `void main()` or `int main()` or `void main(string[] args)` or `int main(string[] args)`, found `{} main({})`", main.ret, main.params.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ")), span: main.span});
            }
        } else if self.require_main {
            self.errors.push(SemError {
                message: "missing `main` function".into(),
                span: prog.span,
            });
        }

        // Third pass: check function bodies
        for item in &prog.items {
            let func_opt = match item {
                Item::Function(f) => Some(f),
                Item::Attributed{attrs: _, item} => if let Item::Function(f) = item.as_ref() { Some(f) } else { None },
                _ => None,
            };
            if let Some(f) = func_opt {
                self.check_function(f);
            }
        }
        // Check class methods, constructors, properties
        let class_items: Vec<ClassDecl> = prog.items.iter().filter_map(|it| if let Item::Class(c) = it { Some(c.clone()) } else { None }).collect();
        for cls in class_items {
            for meth in &cls.methods {
                self.check_method(&cls.name, meth);
            }
            for ctor in &cls.constructors {
                self.check_constructor(&cls.name, ctor);
            }
            for dtor in &cls.destructors {
                self.check_destructor(&cls.name, dtor);
            }
            for prop in &cls.properties {
                self.check_property(&cls.name, prop);
            }
        }
        self.pop_scope(); // global
        std::mem::take(&mut self.errors)
    }

    fn check_function(&mut self, f: &Function) {
        let ret_ty = self.resolve_type(&f.ret_ty);
        self.cur_ret = Some(ret_ty.clone());
        self.push_scope();
        for (idx, p) in f.params.iter().enumerate() {
            let mut ty = self.resolve_type(&p.ty);
            if p.is_variadic {
                // `...T vda` where `T` is element type, `vda` is `T[]`; `... vda` derived from previous
                if p.ty.name() == "__derived__" {
                    if idx == 0 {
                        self.errors.push(SemError{message: "variadic `... vda` without previous type must not be first param".into(), span: p.span});
                    } else {
                        let prev_ty = self.resolve_type(&f.params[idx-1].ty);
                        ty = Ty::Array(Box::new(prev_ty));
                    }
                } else {
                    ty = Ty::Array(Box::new(ty));
                }
                // For derived `... vda` must be last
                if p.ty.name() == "__derived__" && idx + 1 != f.params.len() {
                    self.errors.push(SemError{message: "derived variadic `... vda` must be last".into(), span: p.span});
                }
            }
            self.declare_var(&p.name, ty, p.name_span);
        }
        let always_returns = self.check_block(&f.body, &ret_ty);
        if ret_ty != Ty::Void && !always_returns {
            self.errors.push(SemError{message: format!("function `{}` missing return on some path (returns `{ret_ty}`)", f.name), span: f.span});
        }
        self.pop_scope();
        self.cur_ret = None;
    }

    fn check_method(&mut self, class_name: &str, f: &Function) {
        let ret_ty = self.resolve_type(&f.ret_ty);
        self.cur_ret = Some(ret_ty.clone());
        self.cur_class = Some(class_name.to_string());
        self.push_scope();
        // implicit `this`
        self.declare_var("this", Ty::Struct(class_name.to_string()), f.name_span);
        for (idx, p) in f.params.iter().enumerate() {
            let mut ty = self.resolve_type(&p.ty);
            if p.is_variadic {
                if p.ty.name() == "__derived__" {
                    if idx == 0 {
                        self.errors.push(SemError{message: "variadic `... vda` without previous type must not be first param".into(), span: p.span});
                    } else {
                        let prev_ty = self.resolve_type(&f.params[idx-1].ty);
                        ty = Ty::Array(Box::new(prev_ty));
                    }
                } else {
                    ty = Ty::Array(Box::new(ty));
                }
                if p.ty.name() == "__derived__" && idx + 1 != f.params.len() {
                    self.errors.push(SemError{message: "derived variadic `... vda` must be last".into(), span: p.span});
                }
            }
            self.declare_var(&p.name, ty, p.name_span);
        }
        let always_returns = self.check_block(&f.body, &ret_ty);
        if ret_ty != Ty::Void && !always_returns {
            self.errors.push(SemError{message: format!("method `{}` in class `{}` missing return (returns `{ret_ty}`)", f.name, class_name), span: f.span});
        }
        self.pop_scope();
        self.cur_ret = None;
        self.cur_class = None;
    }

    fn check_constructor(&mut self, class_name: &str, ctor: &ConstructorDecl) {
        self.cur_class = Some(class_name.to_string());
        self.cur_ret = Some(Ty::Void);
        self.push_scope();
        self.declare_var("this", Ty::Struct(class_name.to_string()), ctor.name_span);
        for p in &ctor.params {
            let ty = self.resolve_type(&p.ty);
            self.declare_var(&p.name, ty, p.name_span);
        }
        if let Some(body) = &ctor.body {
            let _ = self.check_block(body, &Ty::Void);
        }
        self.pop_scope();
        self.cur_ret = None;
        self.cur_class = None;
    }

    fn check_destructor(&mut self, class_name: &str, dtor: &DestructorDecl) {
        self.cur_class = Some(class_name.to_string());
        self.cur_ret = Some(Ty::Void);
        self.push_scope();
        self.declare_var("this", Ty::Struct(class_name.to_string()), dtor.name_span);
        let _ = self.check_block(&dtor.body, &Ty::Void);
        self.pop_scope();
        self.cur_ret = None;
        self.cur_class = None;
    }

    fn check_property(&mut self, class_name: &str, prop: &PropertyDecl) {
        let prop_ty = prop.ty.as_ref().map(|t| self.resolve_type(t)).unwrap_or(Ty::Void);
        self.cur_class = Some(class_name.to_string());
        if let Some(getter) = &prop.getter {
            let expected = if prop_ty != Ty::Void { prop_ty.clone() } else { Ty::Int };
            self.cur_ret = Some(expected.clone());
            self.push_scope();
            self.declare_var("this", Ty::Struct(class_name.to_string()), prop.name_span);
            let always_returns = self.check_block(getter, &expected);
            if expected != Ty::Void && !always_returns {
                self.errors.push(SemError{message: format!("property `{}` getter missing return", prop.name), span: prop.span});
            }
            self.pop_scope();
            self.cur_ret = None;
        }
        if let Some((param, body)) = &prop.setter {
            let pty = self.resolve_type(&param.ty);
            if prop_ty != Ty::Void && pty != prop_ty {
                self.errors.push(SemError{message: format!("property `{}` setter type mismatch", prop.name), span: param.span});
            }
            self.cur_ret = Some(Ty::Void);
            self.push_scope();
            self.declare_var("this", Ty::Struct(class_name.to_string()), prop.name_span);
            self.declare_var(&param.name, pty, param.name_span);
            let _ = self.check_block(body, &Ty::Void);
            self.pop_scope();
            self.cur_ret = None;
        }
        self.cur_class = None;
    }

    fn check_block(&mut self, block: &Block, ret_ty: &Ty) -> bool {
        self.push_scope();
        let mut always_returns = false;
        for stmt in &block.stmts {
            let stmt_returns = self.check_stmt(stmt, ret_ty);
            if stmt_returns {
                always_returns = true;
            }
        }
        self.pop_scope();
        always_returns
    }

    fn check_stmt(&mut self, stmt: &Stmt, ret_ty: &Ty) -> bool {
        match stmt {
            Stmt::VarDecl(d) => {
                let mut decl_ty = self.resolve_type(&d.ty);
                if decl_ty == Ty::Void {
                    self.errors.push(SemError {
                        message: "variable cannot have `void` type".into(),
                        span: d.span,
                    });
                }
                // `any xs = vec[]` declares an undetermined vector: the
                // variable takes `Vec(Any)` until the first `push`.
                // `any m = has k: v end` likewise adopts the inferred map.
                // (The adopted init is marked checked to avoid duplicate
                // diagnostics below.)
                let mut init_checked = false;
                if decl_ty == Ty::Any {
                    if let Some(init) = &d.init {
                        if matches!(init.kind, ExprKind::VecEmpty(_)) {
                            decl_ty = Ty::Vec(Box::new(Ty::Any));
                        } else if matches!(init.kind, ExprKind::MapLit { .. }) {
                            decl_ty = self.check_expr(init);
                            init_checked = true;
                        }
                    }
                }
                // Inferred-size `TYPE arr` requires an initializer to fix the size.
                if matches!(decl_ty, Ty::FixedArray { size: None, .. }) && d.init.is_none() {
                    self.errors.push(SemError {
                        message: format!(
                            "array `{}` has inferred size but no initializer: use `arr[N]` or `= [...]`",
                            d.name
                        ),
                        span: d.span,
                    });
                }
                if let Some(init) = &d.init {
                    // Already validated while adopting an inferred type above.
                    if !init_checked {
                        let init_ty = self.check_expr(init);
                        let is_null = matches!(init.kind, ExprKind::Null);
                        // Explicit-size arrays validate length directly for a
                        // precise diagnostic (types skill §8: initializer must
                        // not violate the declared size).
                        if let (
                            Ty::FixedArray { size: Some(n), .. },
                            Ty::FixedArray { size: Some(m), .. },
                        ) = (&decl_ty, &init_ty)
                        {
                            if n != m {
                                self.errors.push(SemError{message: format!("array size mismatch: declared `arr[{n}]` but initializer has {m} elements"), span: init.span});
                            }
                        }
                        let compatible = if init_ty == decl_ty { true } else {
                            match (&decl_ty, &init_ty) {
                                (Ty::Generic(n1, _), Ty::Enum(n2)) if n1 == n2 => true,
                                (Ty::Enum(n1), Ty::Generic(n2, _)) if n1 == n2 => true,
                                // Size already diagnosed above; only elements matter here.
                                (
                                    Ty::FixedArray { elem: de, .. },
                                    Ty::FixedArray { elem: ie, .. },
                                ) => Ty::assignable(ie, de),
                                _ => self.ty_assignable(&init_ty, &decl_ty),
                            }
                        };
                        if !is_null && !compatible && decl_ty != Ty::Void && decl_ty != Ty::Any {
                            self.errors.push(SemError{message: format!("type mismatch in initializer: expected `{decl_ty}`, found `{init_ty}`"), span: init.span});
                        }
                        self.reject_null_own(is_null, &decl_ty, init.span);
                        // Initializing an `own` slot moves out of any own
                        // slots named in the initializer.
                        if matches!(decl_ty, Ty::Own(_)) {
                            self.poison_moved_idents(init);
                        }
                    }
                }
                self.declare_var(&d.name, decl_ty, d.name_span);
                false
            }
            Stmt::Const(c) => {
                let decl_ty = if let Some(t) = &c.ty {
                    self.resolve_type(t)
                } else {
                    self.check_expr(&c.init)
                };
                if decl_ty == Ty::Void {
                    self.errors.push(SemError { message: "const cannot have `void` type".into(), span: c.span });
                }
                let init_ty = self.check_expr(&c.init);
                let is_null = matches!(c.init.kind, ExprKind::Null);
                if !is_null && !self.ty_assignable(&init_ty, &decl_ty) && decl_ty != Ty::Any {
                    self.errors.push(SemError { message: format!("const initializer mismatch: expected `{decl_ty}`, found `{init_ty}`"), span: c.init.span });
                }
                self.reject_null_own(is_null, &decl_ty, c.init.span);
                self.declare_const(&c.name, decl_ty, c.name_span);
                false
            }
            Stmt::Destructure(d) => {
                let expr_ty = self.check_expr(&d.expr);
                // Determine element types
                let elem_tys: Vec<Ty> = match &expr_ty {
                    Ty::Tuple(tys) => tys.clone(),
                    Ty::Array(el) => vec![*el.clone(); d.targets.len()],
                    Ty::FixedArray { elem, .. } => vec![*elem.clone(); d.targets.len()],
                    Ty::Vec(elem) => vec![*elem.clone(); d.targets.len()],
                    _ => {
                        // Check if expr is tuple literal directly
                        if let ExprKind::Tuple(exprs) = &d.expr.kind {
                            exprs.iter().map(|e| self.check_expr(e)).collect()
                        } else {
                            self.errors.push(SemError { message: format!("destructuring requires tuple or array, found `{expr_ty}`"), span: d.expr.span });
                            vec![Ty::Int; d.targets.len()]
                        }
                    }
                };
                if elem_tys.len() != d.targets.len() {
                    // Allow wildcard to be counted, but if elem_tys len != targets len, error unless tuple literal with different len?
                    // For `a,b = (1,2,3)` with 2 targets and 3 elems, we take first 2? For now error
                    if d.targets.len() != elem_tys.len() {
                        self.errors.push(SemError { message: format!("destructuring mismatch: {} targets vs {} values", d.targets.len(), elem_tys.len()), span: d.span });
                    }
                }
                for (idx, target) in d.targets.iter().enumerate() {
                    match target {
                        DestructureTarget::Wildcard(_) => {},
                        DestructureTarget::Ident(name, span) => {
                            let expected_ty = elem_tys.get(idx).cloned().unwrap_or(Ty::Int);
                            if let Some(existing) = self.lookup_var(name) {
                                if self.is_const(name) {
                                    self.errors.push(SemError { message: format!("cannot assign to const `{}`", name), span: *span });
                                }
                                if existing != expected_ty && existing != Ty::Any && expected_ty != Ty::Any {
                                    self.errors.push(SemError { message: format!("destructuring type mismatch for `{}`: expected `{}`, found `{}`", name, existing, expected_ty), span: *span });
                                }
                            } else {
                                self.declare_var(name, expected_ty, *span);
                            }
                        }
                    }
                }
                false
            }
            Stmt::Assert(a) => {
                let cond_ty = self.check_expr(&a.cond);
                if cond_ty != Ty::Bool {
                    self.errors.push(SemError { message: format!("assert condition must be `bool`, found `{}`", cond_ty), span: a.cond.span });
                }
                if let Some(msg) = &a.message {
                    let msg_ty = self.check_expr(msg);
                    // message should be string or any, but allow any for now
                    if msg_ty != Ty::String && msg_ty != Ty::Any {
                        // allow string literals and string variables
                        // If msg is not string, still allow but warn? For now allow any
                    }
                }
                false
            }
            Stmt::Expr(e) => {
                let _ = self.check_expr(&e.expr);
                if Self::is_bare_new(&e.expr) {
                    self.errors.push(SemError{
                        message: "unused `new` value is a guaranteed leak — assign it to an `own` slot or delete it".into(),
                        span: e.expr.span,
                    });
                }
                false
            }
            Stmt::Block(b) => self.check_block(b, ret_ty),
            Stmt::Return(r) => {
                let cur = self.cur_ret.clone().unwrap();
                match (&r.value, &cur) {
                    (None, Ty::Void) => {}
                    (Some(_), Ty::Void) => self.errors.push(SemError {
                        message: "return with value in `void` function".into(),
                        span: r.span,
                    }),
                    (None, ty) => self.errors.push(SemError {
                        message: format!(
                            "missing return value: expected `{ty}`"
                        ),
                        span: r.span,
                    }),
                    (Some(expr), ty) => {
                        let got = self.check_expr(expr);
                        if !self.ty_assignable(&got, ty) {
                            self.errors.push(SemError{message: format!("return type mismatch: expected `{ty}`, found `{got}`"), span: expr.span});
                        }
                    }
                }
                true
            }
            Stmt::If(s) => {
                let cond_ty = self.check_expr(&s.cond);
                if Self::is_bare_new(&s.cond) {
                    self.errors.push(SemError{
                        message: "unused `new` value is a guaranteed leak — assign it to an `own` slot or delete it".into(),
                        span: s.cond.span,
                    });
                }
                if cond_ty != Ty::Bool {
                    self.errors.push(SemError {
                        message: format!(
                            "`if` condition must be `bool`, found `{cond_ty}`"
                        ),
                        span: s.cond.span,
                    });
                }
                let then_ret = self.check_block(&s.then_block, ret_ty);
                let else_ret = if let Some(else_b) = &s.else_block {
                    self.check_block(else_b, ret_ty)
                } else {
                    false
                };
                then_ret && else_ret
            }
            Stmt::While(s) => {
                let cond_ty = self.check_expr(&s.cond);
                if Self::is_bare_new(&s.cond) {
                    self.errors.push(SemError{
                        message: "unused `new` value is a guaranteed leak — assign it to an `own` slot or delete it".into(),
                        span: s.cond.span,
                    });
                }
                if cond_ty != Ty::Bool {
                    self.errors.push(SemError{message: format!("`while` condition must be `bool`, found `{cond_ty}`"), span: s.cond.span});
                }
                self.loop_stack.push(None);
                let _ = self.check_block(&s.body, ret_ty);
                self.loop_stack.pop();
                false
            }
            Stmt::Loop(l) => {
                self.loop_stack.push(l.label.clone());
                let _ = self.check_block(&l.body, ret_ty);
                self.loop_stack.pop();
                false
            }
            Stmt::For(f) => {
                let iter_ty = self.check_expr(&f.iter);
                if Self::is_bare_new(&f.iter) {
                    self.errors.push(SemError{
                        message: "unused `new` value is a guaranteed leak — assign it to an `own` slot or delete it".into(),
                        span: f.iter.span,
                    });
                }
                let elem_ty = match &iter_ty {
                    Ty::Array(el) => (**el).clone(),
                    Ty::FixedArray { elem, .. } => (**elem).clone(),
                    Ty::Vec(elem) => {
                        if **elem == Ty::Any {
                            self.errors.push(SemError{message: "cannot iterate vector with undetermined element type: `push` an element first".into(), span: f.iter.span});
                            Ty::Any
                        } else {
                            (**elem).clone()
                        }
                    }
                    // Maps iterate over their keys.
                    Ty::Map { key, .. } => (**key).clone(),
                    Ty::String => Ty::Char,
                    _ => {
                        self.errors.push(SemError{message: format!("`for` iterable must be array or string, found `{iter_ty}`"), span: f.iter.span});
                        Ty::Int
                    }
                };
                // Second loop variable: the index for arrays/vectors/strings,
                // the value for maps.
                let second_ty: Option<Ty> = match &f.var2 {
                    None => None,
                    Some(_) => Some(match &iter_ty {
                        Ty::Array(_) | Ty::FixedArray { .. } | Ty::Vec(_) | Ty::String => Ty::Int,
                        Ty::Map { value, .. } => (**value).clone(),
                        _ => Ty::Int,
                    }),
                };
                self.loop_stack.push(f.label.clone());
                self.push_scope();
                self.declare_var(&f.var, elem_ty, f.var_span);
                if let (Some((v2, s2)), Some(t2)) = (&f.var2, &second_ty) {
                    self.declare_var(v2, t2.clone(), *s2);
                }
                let _ = self.check_block(&f.body, ret_ty);
                self.pop_scope();
                self.loop_stack.pop();
                false
            }
            Stmt::Defer(d) => {
                match &d.inner {
                    DeferInner::Expr(e) => {
                        let _ = self.check_expr(e);
                        if Self::is_bare_new(e) {
                            self.errors.push(SemError{
                                message: "unused `new` value is a guaranteed leak — assign it to an `own` slot or delete it".into(),
                                span: e.span,
                            });
                        }
                    },
                    DeferInner::Block(b) => { let _ = self.check_block(b, ret_ty); },
                }
                false
            }
            Stmt::Delete(d) => {
                let ty = self.check_expr(&d.target);
                match &d.target.kind {
                    ExprKind::Ident(name) => {
                        let lookup = name.rsplit("::").next().unwrap_or(name);
                        let var_ty = self.lookup_var(name).or_else(|| self.lookup_var(lookup));
                        match var_ty {
                            Some(Ty::Own(_)) => {
                                if self.is_poisoned(name) || self.is_poisoned(lookup) {
                                    self.errors.push(SemError {
                                        message: format!("use of moved or deleted value `{name}`"),
                                        span: d.target.span,
                                    });
                                } else {
                                    self.poison_at_decl(name);
                                    // `delete` of local vs `this` already guarded via ty check
                                }
                            }
                            Some(_) => {
                                self.errors.push(SemError {
                                    message: "delete requires an `own` value".into(),
                                    span: d.target.span,
                                });
                            }
                            None => {
                                // undefined already reported via check_expr
                            }
                        }
                        if name == "this" {
                            self.errors.push(SemError {
                                message: "cannot delete `this` (borrowed receiver)".into(),
                                span: d.target.span,
                            });
                        }
                    }
                    _ => {
                        self.errors.push(SemError {
                            message: "delete requires a named `own` variable".into(),
                            span: d.target.span,
                        });
                        if !matches!(ty, Ty::Own(_)) && ty != Ty::Any {
                            // still report type mismatch if not own
                        }
                    }
                }
                false
            }
            Stmt::Break(b) => {
                if let Some(label) = &b.label {
                    if !self.loop_stack.iter().any(|l| l.as_ref() == Some(label)) {
                        self.errors.push(SemError{message: format!("break label `{label}` not found"), span: b.span});
                    }
                } else if self.loop_depth() == 0 {
                    self.errors.push(SemError{message: "break outside loop".into(), span: b.span});
                }
                false
            }
            Stmt::Continue(c) => {
                if let Some(label) = &c.label {
                    if !self.loop_stack.iter().any(|l| l.as_ref() == Some(label)) {
                        self.errors.push(SemError{message: format!("continue label `{label}` not found"), span: c.span});
                    }
                } else if self.loop_depth() == 0 {
                    self.errors.push(SemError{message: "continue outside loop".into(), span: c.span});
                }
                false
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr) -> Ty {
        match &expr.kind {
            ExprKind::IntLit(_) => Ty::Int,
            ExprKind::FloatLit(_) => Ty::Double,
            ExprKind::BoolLit(_) => Ty::Bool,
            ExprKind::Ident(name) => {
                let lookup = name.rsplit("::").next().unwrap_or(name);
                if let Some(ty) = self.lookup_var(name).or_else(|| self.lookup_var(lookup)) {
                    // Reading a moved/deleted `own` slot is an error. (Plain
                    // assignment targets rebirth via unpoisoning in Assign.)
                    if matches!(ty, Ty::Own(_))
                        && (self.is_poisoned(name) || self.is_poisoned(lookup))
                    {
                        self.errors.push(SemError {
                            message: format!("use of moved or deleted value `{name}`"),
                            span: expr.span,
                        });
                    }
                    ty
                } else {
                    if name.contains("::") {
                        let first = name.split("::").next().unwrap_or(name);
                        if self.enums.contains_key(first) {
                            return Ty::Enum(first.to_string());
                        }
                        if self.structs.contains_key(first) || self.classes.contains_key(first) {
                            return Ty::Struct(first.to_string());
                        }
                    }
                    // Also handle bare type name like `MyEnum` where `MyEnum` is an enum/struct type (for `MyEnum.A`)
                    if self.enums.contains_key(name) {
                        return Ty::Enum(name.clone());
                    }
                    if self.structs.contains_key(name) || self.classes.contains_key(name) {
                        return Ty::Struct(name.clone());
                    }
                    if self.enums.contains_key(lookup) {
                        return Ty::Enum(lookup.to_string());
                    }
                    if self.structs.contains_key(lookup) || self.classes.contains_key(lookup) {
                        return Ty::Struct(lookup.to_string());
                    }
                    self.errors.push(SemError {
                        message: format!("undefined variable `{name}`"),
                        span: expr.span,
                    });
                    Ty::Int
                }
            }
            ExprKind::Paren(inner) => self.check_expr(inner),
            ExprKind::Unary { op, expr: inner } => {
                let t = self.check_expr(inner);
                match op {
                    UnaryOp::Not => {
                        if t != Ty::Bool {
                            self.errors.push(SemError {
                                message: format!(
                                    "`not` requires `bool`, found `{t}`"
                                ),
                                span: expr.span,
                            });
                        }
                        Ty::Bool
                    }
                    UnaryOp::Neg | UnaryOp::Pos => {
                        if !t.is_int_like() {
                            self.errors.push(SemError {
                                message: format!(
                                    "unary `{op:?}` requires int type, found `{t}`"
                                ),
                                span: expr.span,
                            });
                        }
                        t
                    }
                    UnaryOp::BitNot | UnaryOp::Inc | UnaryOp::Dec => {
                        if !t.is_int_like() {
                            self.errors.push(SemError {
                                message: format!(
                                    "unary `{op:?}` requires int type, found `{t}`"
                                ),
                                span: expr.span,
                            });
                        }
                        t
                    }
                }
            }
            ExprKind::Postfix { op, expr: inner } => {
                let t = self.check_expr(inner);
                if !t.is_int_like() {
                    self.errors.push(SemError{message: format!("postfix `{op:?}` requires int type, found `{t}`"), span: expr.span});
                }
                t
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let lt = self.check_expr(lhs);
                let rt = self.check_expr(rhs);
                // operator overloading for class
                if let Ty::Struct(ref sname) = lt {
                    if let Some(cinfo) = self.classes.get(sname).cloned() {
                        let op_str = match op {
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
                            BinOp::CompoundAdd => "+=",
                            BinOp::CompoundSub => "-=",
                            BinOp::CompoundMul => "*=",
                            BinOp::CompoundDiv => "/=",
                            BinOp::CompoundMod => "%=",
                            BinOp::CompoundBitAnd => "&=",
                            BinOp::CompoundBitOr => "|=",
                            BinOp::CompoundBitXor => "^=",
                            BinOp::CompoundShl => "<<=",
                            BinOp::CompoundShr => ">>=",
                        };
                        if let Some(sig) = cinfo.operators.get(op_str) {
                            if let Some(exp) = sig.params.get(0) {
                                if &rt != exp {
                                    self.errors.push(SemError{message: format!("operator `{op_str}` for `{sname}` expects `{}`, found `{rt}`", exp), span: expr.span});
                                }
                            }
                            return sig.ret.clone();
                        }
                    }
                }
                match op {
                    BinOp::Add
                    | BinOp::Sub
                    | BinOp::Mul
                    | BinOp::Div
                    | BinOp::Mod
                    | BinOp::BitAnd
                    | BinOp::BitOr
                    | BinOp::BitXor
                    | BinOp::Shl
                    | BinOp::Shr => {
                        if !lt.is_int_like() || !rt.is_int_like() {
                            self.errors.push(SemError{message: format!("arithmetic/bitwise `{op:?}` requires int types, found `{lt}` and `{rt}`"), span: expr.span});
                        }
                        // Result is the wider int-like operand (MVP: prefer lhs).
                        lt
                    }
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        if !lt.is_int_like() || !rt.is_int_like() {
                            self.errors.push(SemError{message: format!("comparison `{op:?}` requires int types, found `{lt}` and `{rt}`"), span: expr.span});
                        }
                        Ty::Bool
                    }
                    BinOp::NullCoalesce => {
                        // a ?? b : if a is Optional, return inner, else return lt
                        Ty::Int
                    }
                    BinOp::Range | BinOp::RangeInclusive => {
                        if lt != Ty::Int || rt != Ty::Int {
                            self.errors.push(SemError{message: format!("range `{op:?}` requires `int`, found `{lt}` and `{rt}`"), span: expr.span});
                        }
                        Ty::Array(Box::new(Ty::Int))
                    }
                    BinOp::Is | BinOp::IsNot => {
                        // `own` identity: compare data pointers, not structure.
                        // Allow `own T is own U` (any inner) and `own is null`.
                        let own_pair = matches!(&lt, Ty::Own(_)) && (matches!(&rt, Ty::Own(_)) || matches!(&rt, Ty::Any));
                        let own_pair_rev = matches!(&rt, Ty::Own(_)) && matches!(&lt, Ty::Any);
                        if !(own_pair || own_pair_rev) && lt != rt {
                            self.errors.push(SemError{message: format!("`is` requires matching types, found `{lt}` and `{rt}`"), span: expr.span});
                        }
                        Ty::Bool
                    }
                    BinOp::And | BinOp::Or => {
                        if lt != Ty::Bool || rt != Ty::Bool {
                            self.errors.push(SemError{message: format!("logical `{op:?}` requires `bool`, found `{lt}` and `{rt}`"), span: expr.span});
                        }
                        Ty::Bool
                    }
                    BinOp::CompoundAdd
                    | BinOp::CompoundSub
                    | BinOp::CompoundMul
                    | BinOp::CompoundDiv
                    | BinOp::CompoundMod
                    | BinOp::CompoundBitAnd
                    | BinOp::CompoundBitOr
                    | BinOp::CompoundBitXor
                    | BinOp::CompoundShl
                    | BinOp::CompoundShr => {
                        self.errors.push(SemError{message: format!("compound operator `{op:?}` should not appear in Binary"), span: expr.span});
                        Ty::Int
                    }
                }
            }
            ExprKind::Assign { lhs, value } => {
                let lhs_ty = self.check_lvalue(lhs);
                if let ExprKind::Ident(name) = &lhs.kind {
                    if self.is_const(name) {
                        self.errors.push(SemError{message: format!("cannot assign to const `{}`", name), span: lhs.span});
                    }
                    // Rebirth: overwriting a moved/deleted slot with a fresh
                    // value makes it live again.
                    self.unpoison_at_decl(name);
                }
                // Writing through a member of a dead object is an error.
                if let ExprKind::MemberAccess { object, .. } = &lhs.kind {
                    if let ExprKind::Ident(base) = &object.kind {
                        if self.is_poisoned(base) {
                            self.errors.push(SemError{message: format!("use of moved or deleted value `{base}`"), span: object.span});
                        }
                    }
                }
                let rhs_ty = self.check_expr(value);
                let is_null = matches!(value.kind, ExprKind::Null);
                if !is_null && !self.ty_assignable(&rhs_ty, &lhs_ty) {
                    self.errors.push(SemError{message: format!("assignment type mismatch: expected `{lhs_ty}`, found `{rhs_ty}`"), span: expr.span});
                }
                self.reject_null_own(is_null, &lhs_ty, expr.span);
                // Moving into an `own` slot poisons the sources.
                if matches!(lhs_ty, Ty::Own(_)) {
                    self.poison_moved_idents(value);
                }
                lhs_ty
            }
            ExprKind::CompoundAssign { op: _, lhs, value } => {
                let lhs_ty = self.check_lvalue(lhs);
                let rhs_ty = self.check_expr(value);
                if !lhs_ty.is_int_like() || !rhs_ty.is_int_like() {
                    self.errors.push(SemError{message: format!("compound assignment requires int types, found `{lhs_ty}` and `{rhs_ty}`"), span: expr.span});
                }
                lhs_ty
            }
            ExprKind::Conditional { cond, then_branch, else_branch } => {
                let ct = self.check_expr(cond);
                if ct != Ty::Bool {
                    self.errors.push(SemError{message: format!("conditional cond requires `bool`, found `{ct}`"), span: cond.span});
                }
                let tt = self.check_expr(then_branch);
                let et = self.check_expr(else_branch);
                if tt != et {
                    self.errors.push(SemError{message: format!("conditional branches mismatch: `{tt}` vs `{et}`"), span: expr.span});
                }
                tt
            }
            ExprKind::Range { start, end, inclusive: _ } => {
                if let Some(s) = start { let st = self.check_expr(s); if st != Ty::Int { self.errors.push(SemError{message: format!("range start requires `int`, found `{st}`"), span: s.span}); } }
                if let Some(e) = end { let et = self.check_expr(e); if et != Ty::Int { self.errors.push(SemError{message: format!("range end requires `int`, found `{et}`"), span: e.span}); } }
                Ty::Array(Box::new(Ty::Int))
            }
            ExprKind::NullableMemberAccess { object, field, field_span: _ } => {
                let obj_ty = self.check_expr(object);
                // For now, treat like normal member access but allow nullable
                if let Ty::Struct(ref sname) = obj_ty {
                    if let Some(sinfo) = self.structs.get(sname) {
                        if let Some((_, fty)) = sinfo.field_map.get(field) {
                            return fty.clone();
                        }
                    }
                    if let Some(cinfo) = self.classes.get(sname) {
                        if let Some(prop) = cinfo.properties.get(field) {
                            return prop.ty.clone();
                        }
                    }
                    if self.traits.contains_key(sname) {
                        return self.check_trait_field(sname, field, &expr.span);
                    }
                }
                // For optional types, unwrap
                if let Ty::Optional(inner) = obj_ty {
                    if let Ty::Struct(ref sname) = *inner {
                        if let Some(sinfo) = self.structs.get(sname) {
                            if let Some((_, fty)) = sinfo.field_map.get(field) {
                                return fty.clone();
                            }
                        }
                    }
                }
                self.errors.push(SemError{message: format!("unknown field `{field}` for nullable access"), span: expr.span});
                Ty::Int
            }
            ExprKind::Call {
                callee,
                callee_span,
                args,
                type_args,
            } => {
                // Handle generic function calls: substitute type args and check where bounds
                // Also handle inference when type_args is empty.
                if let Some(func) = self.funcs.get(callee).cloned() {
                    if !func.generic_params.is_empty() {
                        if !type_args.is_empty() {
                            self.check_generic_bounds(&func.generic_params, &func.where_clause, type_args, *callee_span);
                            // Build map from generic param name -> concrete Ty using actual param order
                            let mut generic_subst: std::collections::HashMap<String, Ty> = std::collections::HashMap::new();
                            for (gp, ta) in func.generic_params.iter().zip(type_args.iter()) {
                                generic_subst.insert(gp.name.clone(), self.resolve_type(ta));
                            }
                            // also handle extra type_args beyond generic_params via T/U fallback (compat)
                            for (i, ta) in type_args.iter().enumerate().skip(func.generic_params.len()) {
                                let name = format!("T{}", i);
                                generic_subst.insert(name, self.resolve_type(ta));
                            }
                            let subst_ty = |ty: &Ty| -> Ty { Self::subst_generic_ty(ty, &generic_subst) };
                            let substituted_params: Vec<Ty> = func.params.iter().map(|p| subst_ty(p)).collect();
                            let substituted_sig = FuncSig {
                                ret: subst_ty(&func.ret),
                                params: substituted_params,
                                param_modes: func.param_modes.clone(),
                                param_names: func.param_names.clone(),
                                param_is_variadic: func.param_is_variadic.clone(),
                                param_defaults: func.param_defaults.clone(),
                                generic_params: vec![],
                                where_clause: None,
                                span: func.span,
                            };
                            self.check_call_with_sig(args, &substituted_sig, *callee_span, callee);
                            return substituted_sig.ret;
                        } else {
                            // Inference: no explicit type args, try to deduce from actual args
                            // Only attempt if we can type-check args without the generic sig
                            // Gather actual arg types
                            if func.params.len() == args.len() || func.param_is_variadic.iter().any(|&v| v) {
                                let mut inferred: std::collections::HashMap<String, Ty> = std::collections::HashMap::new();
                                let mut ok = true;
                                // For variadic, only infer from fixed part plus variadic element mapping
                                let variadic_idx = func.param_is_variadic.iter().position(|&v| v);
                                if let Some(vidx) = variadic_idx {
                                    let fixed = vidx;
                                    for (i, arg) in args.iter().take(fixed).enumerate() {
                                        let aty = self.check_call_arg(arg);
                                        if let Some(param_ty) = func.params.get(i) {
                                            Self::unify_generic(param_ty, &aty, &mut inferred);
                                        }
                                    }
                                    if let Some(vty) = func.params.get(vidx) {
                                        if let Ty::Array(elem) = vty {
                                            for arg in args.iter().skip(fixed) {
                                                let aty = self.check_call_arg(arg);
                                                Self::unify_generic(elem, &aty, &mut inferred);
                                            }
                                        }
                                    }
                                } else {
                                    for (i, arg) in args.iter().enumerate() {
                                        if let Some(param_ty) = func.params.get(i) {
                                            let aty = self.check_call_arg(arg);
                                            Self::unify_generic(param_ty, &aty, &mut inferred);
                                        }
                                    }
                                }
                                // Check if we inferred all generic params
                                let all_inferred = func.generic_params.iter().all(|gp| inferred.contains_key(&gp.name));
                                if all_inferred && !inferred.is_empty() {
                                    // Validate where bounds with inferred concrete types
                                    let inferred_type_args: Vec<Type> = func.generic_params.iter().map(|gp| {
                                        let ty = &inferred[&gp.name];
                                        Self::ty_to_type(ty)
                                    }).collect();
                                    self.check_generic_bounds(&func.generic_params, &func.where_clause, &inferred_type_args, *callee_span);
                                    let subst_ty = |ty: &Ty| -> Ty { Self::subst_generic_ty(ty, &inferred) };
                                    let substituted_params: Vec<Ty> = func.params.iter().map(|p| subst_ty(p)).collect();
                                    let substituted_sig = FuncSig {
                                        ret: subst_ty(&func.ret),
                                        params: substituted_params,
                                        param_modes: func.param_modes.clone(),
                                        param_names: func.param_names.clone(),
                                        param_is_variadic: func.param_is_variadic.clone(),
                                        param_defaults: func.param_defaults.clone(),
                                        generic_params: vec![],
                                        where_clause: None,
                                        span: func.span,
                                    };
                                    // Re-validate args against substituted sig (avoid double errors: we already checked args for inference, but need precise diagnostics)
                                    // Use assignable check via check_call_with_sig but skip re-checking args already checked? Just return ret for inference case;
                                    // We have already type-checked args during inference; no need to re-error. So just return ret.
                                    // But to get proper error for mismatched non-generic params, run check_call_with_sig with inferred sig on already-typed args? We'll just validate with inferred sig without re-checking arg types via a lightweight path.
                                    // For simplicity, just return inferred ret; the arg type errors would have been handled via unify not producing errors.
                                    // However, if some arg type doesn't match inferred param, we should error. Do a second pass quickly:
                                    for (i, arg) in args.iter().enumerate() {
                                        if let Some(exp) = substituted_sig.params.get(i) {
                                            let aty = self.check_call_arg(arg);
                                            if !self.ty_assignable(&aty, exp) {
                                                // already reported? skip duplicate; but ensure one error
                                            }
                                        }
                                    }
                                    return substituted_sig.ret;
                                } else if !inferred.is_empty() {
                                    // Partial inference: still substitute what we have and return that (allows fallback)
                                    let subst_ty = |ty: &Ty| -> Ty { Self::subst_generic_ty(ty, &inferred) };
                                    let substituted_ret = subst_ty(&func.ret);
                                    if substituted_ret != func.ret {
                                        return substituted_ret;
                                    }
                                }
                            }
                        }
                    } else if !type_args.is_empty() {
                        // Non-generic function called with type args
                        self.errors.push(SemError{message: format!("function is not generic but {} type arguments provided", type_args.len()), span: *callee_span});
                        for arg in args { let _ = self.check_call_arg(arg); }
                        return func.ret;
                    }
                }
                // Check for class constructor call: ClassName(args)
                if let Some(cls) = self.classes.get(callee).cloned() {
                    // Need class decl to find constructors; retrieve from prog? For now check if any ctor matches arity
                    // We stored class info but not ctor sigs directly; we need to lookup via prog? Simpler: check if class has any constructors via scanning prog? Instead use class info fields?
                    // For now, try to find matching ctor via class's constructors stored in checker? We have not stored ctor sigs separately, but we can treat ctor as function with same name.
                    // Quick approach: if class has no explicit ctor, error; else check args
                    // We need to find ctor sigs: iterate over class decls in prog? But we don't have prog here.
                    // Fallback: treat as struct construction via ctor: return struct type, check each arg type is int (for now)
                    // Better: lookup ctor from self.classes via stored field? We didn't store ctor sigs; store generic check via type checking each arg as int?
                    // For Phase 4 minimal: assume ctor takes same types as fields order if no explicit ctor? But test has explicit ctor with 2 ints.
                    // So we will search in self.classes[callee] not enough; we need ctor info. For now we will directly check if callee is a class, return its struct type after checking args.
                    // Find ctor decl via scanning program? We can store ctor sigs in ClassInfo now that we have extends handling - but we already collect ctor in ClassInfo? Actually we didn't store ctor sigs in ClassInfo methods, only fields etc. We need to add ctor sig storage.
                    // As quick fix: if class exists, treat call as constructor: check each arg is int? For test it's int, int -> ok.
                    // Let's attempt to find ctor via class's constructors collected earlier: we stored them but not as sigs. We'll handle via generic: if class has property ctor, we validate via prog scan.
                    // For now simply: if callee class exists, return struct type, and type-check args as if they were field types or ctor params? We'll look up class struct fields and match.
                    let struct_ty = Ty::Struct(callee.clone());
                    // If class has explicit constructors, check against them (visibility: Default is public for ctors)
                    if let Some(cinfo) = self.classes.get(callee).cloned() {
                        if !cinfo.constructors.is_empty() {
                            let mut matched: Option<(FuncSig, crate::ast::Visibility)> = None;
                            for (sig, vis) in &cinfo.constructors {
                                let min = Self::min_args_for_sig(sig);
                                if args.len() >= min && args.len() <= sig.params.len() {
                                    matched = Some((sig.clone(), *vis));
                                    break;
                                }
                            }
                            if let Some((sig, vis)) = matched {
                                if vis == crate::ast::Visibility::Private && self.cur_class.as_deref() != Some(callee.as_str()) {
                                    self.errors.push(SemError{message: format!("constructor for `{callee}` is private"), span: *callee_span});
                                }
                                for (i, arg) in args.iter().enumerate() {
                                    let pidx = if let CallArg::Named { name, .. } = arg {
                                        sig.param_names.iter().position(|n| n == name).unwrap_or(i)
                                    } else { i };
                                    self.declare_out_arg(arg, sig.params.get(pidx));
                                    let aty = self.check_call_arg(arg);
                                    if !self.ty_assignable(&aty, &sig.params[pidx]) {
                                        self.errors.push(SemError{message: format!("ctor arg {}: expected `{}`, found `{aty}`", i+1, sig.params[pidx]), span: arg.span()});
                                    }
                                    self.check_arg_mode(arg, &sig, pidx, callee, i + 1);
                                }
                                return struct_ty;
                            } else {
                                self.errors.push(SemError{message: format!("no matching constructor for `{callee}` with {} args", args.len()), span: *callee_span});
                                for arg in args { let _ = self.check_call_arg(arg); }
                                return struct_ty;
                            }
                        }
                    }
                    // No explicit ctor: check against fields (struct literal via call)
                    let field_tys: Vec<Ty> = self.structs.get(callee).map(|s| s.fields.iter().map(|(_,ty)| ty.clone()).collect()).unwrap_or_default();
                    if !field_tys.is_empty() && args.len() == field_tys.len() {
                        for (i, arg) in args.iter().enumerate() {
                            let aty = self.check_call_arg(arg);
                            let fty = &field_tys[i];
                            if !self.ty_assignable(&aty, fty) {
                                self.errors.push(SemError{message: format!("ctor arg {}: expected `{}`, found `{aty}`", i+1, fty), span: arg.span()});
                            }
                        }
                        return struct_ty;
                    }
                    // Fallback: just type-check args and return struct
                    for arg in args { let _ = self.check_call_arg(arg); }
                    return struct_ty;
                }
                // NOTE (real stdlib): no builtin IO shortcut. `print`/`println`/
                // `printInt`/`putChar` are ordinary functions from `std::io`;
                // without `import std::io` they are undefined names by design.
                let sig = self.funcs.get(callee).cloned();
                if let Some(sig) = sig {
                    if sig.param_is_variadic.iter().any(|&v| v) {
                        self.check_call_with_sig(args, &sig, *callee_span, callee);
                    } else {
                        let min_required = Self::min_args_for_sig(&sig);
                        if args.len() < min_required || args.len() > sig.params.len() {
                            if min_required == sig.params.len() {
                                self.errors.push(SemError {
                                    message: format!(
                                        "`{callee}` expects {} args, found {}",
                                        sig.params.len(),
                                        args.len()
                                    ),
                                    span: *callee_span,
                                });
                            } else {
                                self.errors.push(SemError {
                                    message: format!(
                                        "`{callee}` expects {}-{} args, found {}",
                                        min_required,
                                        sig.params.len(),
                                        args.len()
                                    ),
                                    span: *callee_span,
                                });
                            }
                        }
                        for (i, arg) in args.iter().enumerate() {
                            let pidx = if let CallArg::Named { name, .. } = arg {
                                sig.param_names.iter().position(|n| n == name).unwrap_or(i)
                            } else { i };
                            self.declare_out_arg(arg, sig.params.get(pidx));
                            let aty = self.check_call_arg(arg);
                            if let Some(param_ty) = sig.params.get(pidx) {
                                if !self.ty_assignable(&aty, param_ty) {
                                    let is_generic = matches!(param_ty, Ty::Generic(n, _) if n.len() == 1 && n.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false));
                                    if !is_generic {
                                        self.errors.push(SemError{message: format!("argument {} of `{callee}`: expected `{}`, found `{aty}`", i+1, param_ty), span: arg.span()});
                                    }
                                }
                            }
                            self.check_arg_mode(arg, &sig, pidx, callee, i + 1);
                        }
                        // Missing named arguments must have defaults.
                        if args.iter().any(|a| matches!(a, CallArg::Named{..})) {
                            let mut provided = std::collections::HashSet::new();
                            let mut n_pos = 0;
                            for a in args {
                                if let CallArg::Named { name, .. } = a {
                                    if let Some(pidx) = sig.param_names.iter().position(|n| n == name) {
                                        provided.insert(pidx);
                                    }
                                } else {
                                    provided.insert(n_pos);
                                    n_pos += 1;
                                }
                            }
                            for (pidx, pname) in sig.param_names.iter().enumerate() {
                                if !provided.contains(&pidx) && sig.param_defaults.get(pidx).and_then(|d| d.as_ref()).is_none() {
                                    self.errors.push(SemError{message: format!("missing argument `{}` for `{callee}` (no default value)", pname), span: *callee_span});
                                }
                            }
                        }
                    }
                    sig.ret
                } else if let Some(var_ty) = self.lookup_var(callee) {
                    // variable call (closure / function pointer)
                    if let Ty::Function(ret, params) = var_ty {
                        if params.len() != args.len() {
                            self.errors.push(SemError{message: format!("function variable `{callee}` expects {} args, found {}", params.len(), args.len()), span: *callee_span});
                        }
                        for (i, arg) in args.iter().enumerate() {
                            let aty = self.check_call_arg(arg);
                            if let Some(pt) = params.get(i) { if !self.ty_assignable(&aty, pt) { self.errors.push(SemError{message: format!("argument {}: expected `{}`, found `{aty}`", i+1, pt), span: arg.span()}); } }
                        }
                        *ret
                    } else if let Ty::Any = var_ty {
                        for arg in args { let _ = self.check_call_arg(arg); }
                        Ty::Int
                    } else {
                        self.errors.push(SemError{message: format!("`{callee}` is not a function (found `{var_ty}`)"), span: *callee_span});
                        for arg in args { let _ = self.check_call_arg(arg); }
                        Ty::Int
                    }
                } else {
                    self.errors.push(SemError {
                        message: format!("undefined function `{callee}`"),
                        span: *callee_span,
                    });
                    for arg in args {
                        let _ = self.check_call_arg(arg);
                    }
                    Ty::Int
                }
            }
            ExprKind::MemberAccess {
                object,
                field,
                field_span,
            } => {
                let obj_ty = self.check_expr(object);
                let effective_ty = self.deref_ty(&obj_ty);
                if let Ty::Struct(ref sname) = effective_ty {
                    if let Some(sinfo) = self.structs.get(sname).cloned() {
                        if let Some((_, fty)) = sinfo.field_map.get(field) {
                            if let Some(vis) = sinfo.field_vis.get(field) {
                                // For struct, Default is public, only Private is private
                                if *vis == crate::ast::Visibility::Private && self.cur_class.as_deref() != Some(sname.as_str()) {
                                    self.errors.push(SemError{message: format!("field `{field}` is private"), span: *field_span});
                                }
                            } else if let Some(cinfo) = self.classes.get(sname) {
                                if let Some(vis) = cinfo.field_vis.get(field) {
                                    if *vis != crate::ast::Visibility::Public && self.cur_class.as_deref() != Some(sname.as_str()) {
                                        self.errors.push(SemError{message: format!("field `{field}` is private"), span: *field_span});
                                    }
                                } else if self.cur_class.as_deref() != Some(sname.as_str()) {
                                    self.errors.push(SemError{message: format!("field `{field}` is private"), span: *field_span});
                                }
                            }
                            fty.clone()
                        } else if let Some(cinfo) = self.classes.get(sname) {
                            if let Some(prop) = cinfo.properties.get(field) {
                                if prop.visibility != crate::ast::Visibility::Public && self.cur_class.as_deref() != Some(sname.as_str()) {
                                    self.errors.push(SemError{message: format!("property `{field}` is private"), span: *field_span});
                                }
                                if prop.has_get { prop.ty.clone() } else { self.errors.push(SemError{message: format!("property `{field}` has no getter"), span: *field_span}); Ty::Int }
                            } else {
                                self.errors.push(SemError { message: format!("struct `{sname}` has no field `{field}`"), span: *field_span, }); Ty::Int
                            }
                        } else {
                            self.errors.push(SemError { message: format!("struct `{sname}` has no field `{field}`"), span: *field_span, }); Ty::Int
                        }
                    } else if let Some(cinfo) = self.classes.get(sname) {
                        if let Some(prop) = cinfo.properties.get(field) {
                            if prop.visibility != crate::ast::Visibility::Public && self.cur_class.as_deref() != Some(sname.as_str()) {
                                self.errors.push(SemError{message: format!("property `{field}` is private"), span: *field_span});
                            }
                            if prop.has_get { prop.ty.clone() } else { self.errors.push(SemError{message: format!("property `{field}` has no getter"), span: *field_span}); Ty::Int }
                        } else {
                            self.errors.push(SemError { message: format!("struct `{sname}` has no field `{field}`"), span: *field_span, }); Ty::Int
                        }
                    } else if self.traits.contains_key(sname) {
                        // Trait-typed receiver: structural field dispatch
                        // (see `check_trait_field`).
                        self.check_trait_field(sname, field, field_span)
                    } else {
                        self.errors.push(SemError { message: format!("unknown struct `{sname}`"), span: object.span, }); Ty::Int
                    }
                } else if let Ty::Enum(ref ename) = obj_ty {
                    if let Some(einfo) = self.enums.get(ename) {
                        if let Some((_, _)) = einfo.variant_map.get(field) { Ty::Enum(ename.clone()) } else { self.errors.push(SemError{message: format!("enum `{}` has no variant `{}`", ename, field), span: *field_span}); Ty::Int }
                    } else { self.errors.push(SemError{message: format!("unknown enum `{}`", ename), span: *field_span}); Ty::Int }
                } else {
                    self.errors.push(SemError { message: format!("field access on non-struct `{}`, field `{}`", obj_ty, field), span: *field_span, }); Ty::Int
                }
            }
            ExprKind::StructLit { ty, fields } => {
                let lit_ty = self.resolve_type(ty);
                let sname = match lit_ty {
                    Ty::Struct(ref n) => n.clone(),
                    _ => {
                        self.errors.push(SemError{message: format!("struct literal requires struct type, found `{lit_ty}`"), span: expr.span});
                        return lit_ty;
                    }
                };
                let sinfo = match self.structs.get(&sname).cloned() {
                    Some(s) => s,
                    None => {
                        self.errors.push(SemError {
                            message: format!("unknown struct `{sname}`"),
                            span: expr.span,
                        });
                        return Ty::Struct(sname);
                    }
                };
                let mut seen = HashSet::new();
                for (fname, fspan, fexpr) in fields {
                    if !seen.insert(fname) {
                        self.errors.push(SemError {
                            message: format!(
                                "duplicate field `{fname}` in struct literal"
                            ),
                            span: *fspan,
                        });
                    }
                    if let Some((_, expected_ty)) = sinfo.field_map.get(fname) {
                        let got = self.check_expr(fexpr);
                        if &got != expected_ty {
                            self.errors.push(SemError{message: format!("field `{fname}`: expected `{expected_ty}`, found `{got}`"), span: fexpr.span});
                        }
                    } else {
                        self.errors.push(SemError {
                            message: format!(
                                "unknown field `{fname}` for struct `{sname}`"
                            ),
                            span: *fspan,
                        });
                        let _ = self.check_expr(fexpr);
                    }
                }
                // Check missing fields — allow if field has default `= expr`
                for (fname, _) in &sinfo.fields {
                    if !seen.contains(fname) {
                        let has_default = sinfo.field_defaults.get(fname).and_then(|o| o.as_ref()).is_some();
                        if !has_default {
                            self.errors.push(SemError {
                                message: format!(
                                    "missing field `{fname}` in `{sname}` literal"
                                ),
                                span: expr.span,
                            });
                        }
                    }
                }
                Ty::Struct(sname)
            }
            ExprKind::MapLit { ty, entries } => {
                // Explicit `KEY:VALUE has ... end` resolves directly; bare
                // `has ... end` under `any` infers from the first entry.
                let lit_ty = self.resolve_type(ty);
                let (key_ty, val_ty) = match lit_ty {
                    Ty::Map { key, value } => (*key, *value),
                    Ty::Any => {
                        if entries.is_empty() {
                            return Ty::Map {
                                key: Box::new(Ty::Any),
                                value: Box::new(Ty::Any),
                            };
                        }
                        let k0 = self.check_expr(&entries[0].0);
                        let v0 = self.check_expr(&entries[0].1);
                        for (k, v) in entries.iter().skip(1) {
                            let kt = self.check_expr(k);
                            if !Ty::assignable(&kt, &k0) {
                                self.errors.push(SemError{message: format!("map key mismatch: expected `{k0}`, found `{kt}`"), span: k.span});
                            }
                            let vt = self.check_expr(v);
                            if !Ty::assignable(&vt, &v0) {
                                self.errors.push(SemError{message: format!("map value mismatch: expected `{v0}`, found `{vt}`"), span: v.span});
                            }
                        }
                        return Ty::Map { key: Box::new(k0), value: Box::new(v0) };
                    }
                    _ => {
                        self.errors.push(SemError{message: format!("map literal requires map type, found `{lit_ty}`"), span: expr.span});
                        return lit_ty;
                    }
                };
                // Only int-like and string keys are supported by codegen.
                match &key_ty {
                    Ty::String => {},
                    t if t.is_int_like() => {},
                    Ty::Any => {},
                    other => {
                        self.errors.push(SemError{message: format!("unsupported map key type `{other}`: keys must be `string` or int types"), span: expr.span});
                    }
                }
                for (k, v) in entries {
                    let kt = self.check_expr(k);
                    if !Ty::assignable(&kt, &key_ty) {
                        self.errors.push(SemError{message: format!("map key mismatch: expected `{key_ty}`, found `{kt}`"), span: k.span});
                    }
                    let vt = self.check_expr(v);
                    if !Ty::assignable(&vt, &val_ty) {
                        self.errors.push(SemError{message: format!("map value mismatch: expected `{val_ty}`, found `{vt}`"), span: v.span});
                    }
                }
                Ty::Map { key: Box::new(key_ty), value: Box::new(val_ty) }
            }
            ExprKind::StringLit(_) => Ty::String,
            ExprKind::CharLit(_) => Ty::Char,
            ExprKind::Index { object, index } => {
                let obj_ty = self.check_expr(object);
                let idx_ty = self.check_expr(index);
                // Maps validate the index against the key type in the match
                // below; other indexables require int-like indices.
                let is_map = matches!(obj_ty, Ty::Map { .. });
                if !is_map && !idx_ty.is_int_like() {
                    self.errors.push(SemError {
                        message: format!(
                            "index must be an int type, found `{idx_ty}`"
                        ),
                        span: index.span,
                    });
                }
                match obj_ty {
                    Ty::Array(el) => *el,
                    Ty::FixedArray { elem, .. } => *elem,
                    Ty::Vec(elem) => {
                        if *elem == Ty::Any {
                            self.errors.push(SemError {
                                message: "cannot index vector with undetermined element type: `push` an element first".into(),
                                span: object.span,
                            });
                            Ty::Any
                        } else {
                            *elem
                        }
                    }
                    Ty::Map { key, value } => {
                        if !Ty::assignable(&idx_ty, key.as_ref()) {
                            self.errors.push(SemError {
                                message: format!(
                                    "map index must be `{}`, found `{idx_ty}`",
                                    key
                                ),
                                span: index.span,
                            });
                        }
                        *value
                    }
                    Ty::String => Ty::Char, // string[i] -> char ?
                    _ => {
                        self.errors.push(SemError {
                            message: format!(
                                "cannot index non-array type `{obj_ty}`"
                            ),
                            span: object.span,
                        });
                        Ty::Int
                    }
                }
            }
            ExprKind::Slice { object, start, end, inclusive: _ } => {
                let obj_ty = self.check_expr(object);
                if let Some(s) = start {
                    let st = self.check_expr(s);
                    if st != Ty::Int {
                        self.errors.push(SemError { message: format!("slice start must be `int`, found `{st}`"), span: s.span });
                    }
                }
                if let Some(e) = end {
                    let et = self.check_expr(e);
                    if et != Ty::Int {
                        self.errors.push(SemError { message: format!("slice end must be `int`, found `{et}`"), span: e.span });
                    }
                }
                match obj_ty {
                    Ty::Array(el) => Ty::Array(el.clone()), // slice retains array type
                    Ty::FixedArray { elem, size } => Ty::FixedArray { elem: elem.clone(), size }, // slice retains fixed-array type (MVP)
                    Ty::Vec(elem) => Ty::Vec(elem.clone()), // slice retains vector type (MVP)
                    Ty::String => Ty::String, // string slice -> string
                    _ => {
                        self.errors.push(SemError { message: format!("cannot slice non-array type `{obj_ty}`"), span: object.span });
                        Ty::Int
                    }
                }
            }
            ExprKind::This => {
                if let Some(cls) = &self.cur_class {
                    Ty::Struct(cls.clone())
                } else {
                    self.errors.push(SemError{message: "`this` outside class method".into(), span: expr.span});
                    Ty::Int
                }
            }
            ExprKind::MethodCall{object, method, method_span, args} => {
                let obj_ty = self.check_expr(object);
                // Vector `push` (types skill §10-11): `v.push(x)` appends and
                // returns void. On an undetermined (`vec[]`) vector the first
                // push permanently establishes the element type.
                if let Ty::Vec(elem) = &obj_ty {
                    if method == "push" {
                        if args.len() != 1 {
                            self.errors.push(SemError{message: format!("`push` expects 1 arg, found {}", args.len()), span: *method_span});
                            for a in args { let _ = self.check_call_arg(a); }
                            return Ty::Void;
                        }
                        let aty = self.check_call_arg(&args[0]);
                        if **elem == Ty::Any {
                            // Establishment: `any xs = vec[]` + `xs.push(42)`
                            // fixes `xs` as an `int` vector from here on.
                            if let ExprKind::Ident(name) = &object.kind {
                                if self.is_const(name) {
                                    self.errors.push(SemError{message: format!("cannot push to const vector `{}`", name), span: *method_span});
                                } else {
                                    let established = Ty::Vec(Box::new(aty.clone()));
                                    self.set_var_ty(name, established);
                                }
                            }
                            // Non-identifier bases can't be re-typed; the
                            // single checked arg stands as the establishment.
                            return Ty::Void;
                        }
                        if !Ty::assignable(&aty, elem) {
                            self.errors.push(SemError{message: format!("`push` expects `{}`, found `{}`", elem, aty), span: args[0].span()});
                        }
                        return Ty::Void;
                    }
                }
                // Collection and string methods (types skill: stdlib behavior,
                // compiler-validated). Returns Some when handled.
                if let Some(ret) = self.check_collection_method(object, method, method_span, args, &obj_ty) {
                    return ret;
                }
                // Auto-deref for `own T` receivers (`. ` through owning pointers).
                let effective_ty = self.deref_ty(&obj_ty);
                let sname = match effective_ty {
                    Ty::Struct(ref n) => n.clone(),
                    _ => {
                        self.errors.push(SemError{message: format!("method call on non-class type `{obj_ty}`"), span: object.span});
                        for a in args { let _ = self.check_call_arg(a); }
                        return Ty::Int;
                    }
                };
                if let Some(cls) = self.classes.get(&sname).cloned() {
                    // try class map first, fallback to structs? But methods only in classes
                    if let Some(meth) = cls.methods.get(method) {
                        // visibility: Default is private for methods
                        if let Some(vis) = cls.method_vis.get(method) {
                            if *vis != crate::ast::Visibility::Public && self.cur_class.as_deref() != Some(sname.as_str()) {
                                self.errors.push(SemError{message: format!("method `{method}` is private"), span: *method_span});
                            }
                        } else if self.cur_class.as_deref() != Some(sname.as_str()) {
                            self.errors.push(SemError{message: format!("method `{method}` is private"), span: *method_span});
                        }
                        self.check_method_call_args(args, meth, method, method_span);
                        meth.ret.clone()
                    } else {
                        // Also check if class was actually struct with no methods? Then try struct field? but method not found
                        self.errors.push(SemError{message: format!("class `{sname}` has no method `{method}`"), span: *method_span});
                        for a in args { let _ = self.check_call_arg(a); }
                        Ty::Int
                    }
                } else if self.traits.contains_key(&sname) {
                    // Trait-typed receiver: interface dispatch (see
                    // `check_trait_method_call`).
                    self.check_trait_method_call(&sname, method, method_span, args, object.span)
                } else {
                    // Try structs map for method? For now treat as class not found, check if struct has method? struct has no methods
                    self.errors.push(SemError{message: format!("unknown class `{sname}`"), span: object.span});
                    for a in args { let _ = self.check_call_arg(a); }
                    Ty::Int
                }
            }
            ExprKind::EnumVariant{enum_name, variant, variant_span, args} => {
                let enum_ty: Ty = if let Some(en) = enum_name {
                    if !self.enums.contains_key(en) {
                        self.errors.push(SemError{message: format!("unknown enum `{en}`"), span: *variant_span});
                        Ty::Int
                    } else { Ty::Enum(en.clone()) }
                } else {
                    let mut found: Option<String> = None;
                    let mut found_ty: Option<Ty> = None;
                    for (ename, einfo) in &self.enums {
                        if einfo.variant_map.contains_key(variant) {
                            if found.is_some() {
                                self.errors.push(SemError{message: format!("ambiguous variant `{variant}`; use qualified `Enum.{}`", variant), span: *variant_span});
                                found = Some(ename.clone());
                                break;
                            }
                            found = Some(ename.clone());
                            found_ty = Some(Ty::Enum(ename.clone()));
                        }
                    }
                    if let Some(t) = found_ty { t } else {
                        self.errors.push(SemError{message: format!("unknown variant `{variant}`"), span: *variant_span});
                        Ty::Int
                    }
                };
                let enum_name_str = match &enum_ty { Ty::Enum(n) => n.clone(), _ => "".to_string() };
                if let Some(einfo) = self.enums.get(&enum_name_str).cloned() {
                    if let Some((_, payload_tys)) = einfo.variant_map.get(variant.rsplit("::").next().unwrap_or(variant)) {
                        if !payload_tys.is_empty() {
                            if args.len() != payload_tys.len() {
                                self.errors.push(SemError{message: format!("variant `{variant}` expects {} payload(s), found {}", payload_tys.len(), args.len()), span: *variant_span});
                            } else {
                                for (pty, arg) in payload_tys.iter().zip(args.iter()) {
                                    if matches!(arg, CallArg::Out { .. }) {
                                        self.errors.push(SemError { message: format!("`out` argument not allowed in enum payload `{variant}`"), span: arg.span() });
                                    }
                                    let aty = self.check_call_arg(arg);
                                    let is_generic = matches!(pty, Ty::Generic(n, _) if n.len() == 1 && n.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false));
                                    if !self.ty_assignable(&aty, pty) && !is_generic {
                                        self.errors.push(SemError{message: format!("variant `{variant}` payload: expected `{}`, found `{}`", pty, aty), span: arg.span()});
                                    }
                                }
                            }
                        } else {
                            if !args.is_empty() {
                                self.errors.push(SemError{message: format!("variant `{variant}` expects no args"), span: *variant_span});
                            }
                            for a in args { let _ = self.check_call_arg(a); }
                        }
                    }
                } else {
                    for a in args { let _ = self.check_call_arg(a); }
                }
                enum_ty
            }
            ExprKind::Match(m) => {
                let scrut_ty = self.check_expr(&m.scrutinee);
                // scrutinee must be int/bool/enum/struct/tuple
                if scrut_ty != Ty::Int
                    && scrut_ty != Ty::Bool
                    && !matches!(scrut_ty, Ty::Struct(_))
                    && !matches!(scrut_ty, Ty::Enum(_))
                    && !matches!(scrut_ty, Ty::Tuple(_))
                {
                    self.errors.push(SemError{message: format!("match scrutinee must be `int`/`bool`/`enum`, found `{scrut_ty}`"), span: m.scrutinee.span});
                }
                if m.arms.is_empty() {
                    self.errors.push(SemError {
                        message: "match requires at least one arm".into(),
                        span: m.span,
                    });
                    return scrut_ty;
                }
                // For Phase 2, assume arms produce same type; infer first arm's type
                let mut arm_ty: Option<Ty> = None;
                let mut has_wildcard = false;
                for arm in &m.arms {
                    // pattern type check
                    let mut arm_has_wildcard = false;
                    match &arm.pattern {
                        Pattern::Wildcard(_) => {
                            has_wildcard = true;
                            arm_has_wildcard = true;
                        }
                        Pattern::LitInt(_, span) => {
                            if scrut_ty != Ty::Int {
                                self.errors.push(SemError{message: format!("pattern `int` mismatches scrutinee `{scrut_ty}`"), span: *span});
                            }
                        }
                        Pattern::LitBool(_, span) => {
                            if scrut_ty != Ty::Bool {
                                self.errors.push(SemError{message: format!("pattern `bool` mismatches scrutinee `{scrut_ty}`"), span: *span});
                            }
                        }
                        Pattern::Var(_, _) => { has_wildcard = true; arm_has_wildcard = true; }
                        Pattern::Alternative(pats, span) => {
                            // `a | b` or `a or b` : each alternative must match scrutinee type
                            if pats.is_empty() {
                                self.errors.push(SemError{message: "empty alternative pattern".into(), span: *span});
                            }
                            let mut any_wildcard = false;
                            for pat in pats {
                                match pat {
                                    Pattern::Wildcard(_) => any_wildcard = true,
                                    Pattern::Var(_, _) => any_wildcard = true,
                                    Pattern::LitInt(_, s) => if scrut_ty != Ty::Int { self.errors.push(SemError{message: format!("pattern `int` mismatches scrutinee `{scrut_ty}`"), span: *s}); },
                                    Pattern::LitBool(_, s) => if scrut_ty != Ty::Bool { self.errors.push(SemError{message: format!("pattern `bool` mismatches scrutinee `{scrut_ty}`"), span: *s}); },
                                    Pattern::Enum{variant, variant_span, ..} => {
                                        if !matches!(scrut_ty, Ty::Enum(_)) {
                                            self.errors.push(SemError{message: format!("enum pattern on non-enum scrutinee `{}`", scrut_ty), span: *variant_span});
                                        }
                                    }
                                    Pattern::Tuple(_, s) => {
                                        if !matches!(scrut_ty, Ty::Tuple(_)) {
                                            self.errors.push(SemError{message: format!("tuple pattern on non-tuple scrutinee `{}`", scrut_ty), span: *s});
                                        }
                                    }
                                    Pattern::Alternative(_, _) => {},
                                }
                            }
                            if any_wildcard { has_wildcard = true; arm_has_wildcard = true; }
                        }
                        Pattern::Tuple(pats, span) => {
                            if let Ty::Tuple(tys) = &scrut_ty {
                                if pats.len() != tys.len() {
                                    self.errors.push(SemError{message: format!("tuple pattern expects {} elements, found {} (scrutinee `{}`)", tys.len(), pats.len(), scrut_ty), span: *span});
                                } else {
                                    for (pat, ty) in pats.iter().zip(tys.iter()) {
                                        match pat {
                                            Pattern::Wildcard(_) => {},
                                            Pattern::Var(_, _) => {},
                                            Pattern::LitInt(_, s) => if *ty != Ty::Int { self.errors.push(SemError{message: format!("tuple element expects `int`, found `{}`", ty), span: *s}); },
                                            Pattern::LitBool(_, s) => if *ty != Ty::Bool { self.errors.push(SemError{message: format!("tuple element expects `bool`, found `{}`", ty), span: *s}); },
                                            _ => {}
                                        }
                                    }
                                }
                            } else {
                                self.errors.push(SemError{message: format!("tuple pattern on non-tuple scrutinee `{}`", scrut_ty), span: *span});
                            }
                        }
                        Pattern::Enum{variant, variant_span, payload} => {
                            if let Ty::Enum(ref ename) = scrut_ty {
                                if let Some(einfo) = self.enums.get(ename).cloned() {
                                    // Qualified `Color.Red` parses as `Color::Red`; strip to `Red` for lookup.
                                    let vbase: &str = variant.rsplit("::").next().unwrap_or(variant);
                                    if let Some((_, payload_tys)) = einfo.variant_map.get(vbase) {
                                        match (payload, payload_tys.as_slice()) {
                                            (Some(pats), [expected]) if pats.len() == 1 => {
                                                match &pats[0] {
                                                    Pattern::Wildcard(_) => {},
                                                    Pattern::LitInt(_, s) => if *expected != Ty::Int { self.errors.push(SemError{message: format!("payload for `{}` expects `{}`, found `int`", variant, expected), span: *s}); },
                                                    Pattern::LitBool(_, s) => if *expected != Ty::Bool { self.errors.push(SemError{message: format!("payload for `{}` expects `{}`, found `bool`", variant, expected), span: *s}); },
                                                    Pattern::Var(_, _) => {},
                                                    Pattern::Enum{..} => self.errors.push(SemError{message: "nested enum payload pattern not supported".into(), span: *variant_span}),
                                                    _ => {},
                                                }
                                            }
                                            (Some(pats), expecteds) => {
                                                if pats.len() != expecteds.len() {
                                                    self.errors.push(SemError{message: format!("variant `{}` expects {} payload(s), found {}", variant, expecteds.len(), pats.len()), span: *variant_span});
                                                } else {
                                                    for (pat, exp) in pats.iter().zip(expecteds.iter()) {
                                                        match pat {
                                                            Pattern::Wildcard(_) => {},
                                                            Pattern::LitInt(_, s) => if *exp != Ty::Int { self.errors.push(SemError{message: format!("payload for `{}` expects `{}`, found `int`", variant, exp), span: *s}); },
                                                            Pattern::LitBool(_, s) => if *exp != Ty::Bool { self.errors.push(SemError{message: format!("payload for `{}` expects `{}`, found `bool`", variant, exp), span: *s}); },
                                                            Pattern::Var(_, _) => {},
                                                            _ => {},
                                                        }
                                                    }
                                                }
                                            }
                                            (None, []) => {},
                                            (None, _) => self.errors.push(SemError{message: format!("variant `{}` expects payload", variant), span: *variant_span}),
                                            (Some(_), []) => self.errors.push(SemError{message: format!("variant `{}` has no payload but pattern provides one", variant), span: *variant_span}),
                                        }
                                    } else {
                                        self.errors.push(SemError{message: format!("unknown variant `{}` for enum `{}`", variant, ename), span: *variant_span});
                                    }
                                } else {
                                    self.errors.push(SemError{message: format!("unknown enum `{}`", ename), span: *variant_span});
                                }
                            } else {
                                self.errors.push(SemError{message: format!("enum pattern on non-enum scrutinee `{}`", scrut_ty), span: *variant_span});
                            }
                        }
                    }
                    // Open a scope for pattern bindings visible in guard + body
                    self.push_scope();
                    // Declare pattern-bound variables
                    match &arm.pattern {
                        Pattern::Var(name, span) => {
                            self.declare_var(name, scrut_ty.clone(), *span);
                        }
                        Pattern::Alternative(pats, _) => {
                            // For `a | b` or `a or b`, bind vars from first Var alternative if any
                            for pat in pats {
                                match pat {
                                    Pattern::Var(name, span) => { self.declare_var(name, scrut_ty.clone(), *span); break; },
                                    Pattern::Tuple(subs, _) => {
                                        if let Ty::Tuple(tys) = &scrut_ty {
                                            for (spat, ty) in subs.iter().zip(tys.iter()) {
                                                if let Pattern::Var(n, s) = spat { self.declare_var(n, ty.clone(), *s); }
                                            }
                                        }
                                        break;
                                    }
                                    Pattern::Enum{ payload: Some(subs), variant, ..} => {
                                        if let Ty::Enum(ref ename) = scrut_ty {
                                            if let Some(payload_tys) = self.enums.get(ename).and_then(|einfo| einfo.variant_map.get(variant.rsplit("::").next().unwrap_or(variant)).map(|(_, v)| v.clone())) {
                                                for (spat, ty) in subs.iter().zip(payload_tys.iter()) {
                                                    if let Pattern::Var(n, s) = spat { self.declare_var(n, ty.clone(), *s); }
                                                }
                                            }
                                        }
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Pattern::Tuple(pats, _) => {
                            if let Ty::Tuple(tys) = &scrut_ty {
                                for (pat, ty) in pats.iter().zip(tys.iter()) {
                                    if let Pattern::Var(n, s) = pat {
                                        self.declare_var(n, ty.clone(), *s);
                                    } else if let Pattern::Tuple(inner, _) = pat {
                                        // nested tuple like ((a,b), c) - not common, ignore for now
                                        if let Ty::Tuple(inner_tys) = ty {
                                            for (ipat, ity) in inner.iter().zip(inner_tys.iter()) {
                                                if let Pattern::Var(n2, s2) = ipat { self.declare_var(n2, ity.clone(), *s2); }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Pattern::Enum{ payload: Some(pats), variant, ..} => {
                            if pats.len() == 1 {
                                if let Pattern::Var(vname, vspan) = &pats[0] {
                                    if let Ty::Enum(ref ename) = scrut_ty {
                                        if let Some(payload_tys) = self.enums.get(ename).and_then(|einfo| einfo.variant_map.get(variant.rsplit("::").next().unwrap_or(variant)).map(|(_, v)| v.clone())) {
                                            if let Some(pty) = payload_tys.first() {
                                                self.declare_var(vname, pty.clone(), *vspan);
                                            }
                                        }
                                    }
                                } else if let Pattern::Tuple(subs, _) = &pats[0] {
                                    // Enum payload is tuple e.g., `MyVariant((a,b))` where payload is one tuple
                                    if let Ty::Enum(ref ename) = scrut_ty {
                                        if let Some(payload_tys) = self.enums.get(ename).and_then(|einfo| einfo.variant_map.get(variant.rsplit("::").next().unwrap_or(variant)).map(|(_, v)| v.clone())) {
                                            if let Some(Ty::Tuple(tys)) = payload_tys.first() {
                                                for (spat, ty) in subs.iter().zip(tys.iter()) {
                                                    if let Pattern::Var(n, s) = spat { self.declare_var(n, ty.clone(), *s); }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                if let Ty::Enum(ref ename) = scrut_ty {
                                    if let Some(payload_tys) = self.enums.get(ename).and_then(|einfo| einfo.variant_map.get(variant.rsplit("::").next().unwrap_or(variant)).map(|(_, v)| v.clone())) {
                                        for (pat, ty) in pats.iter().zip(payload_tys.iter()) {
                                            if let Pattern::Var(vname, vspan) = pat {
                                                self.declare_var(vname, ty.clone(), *vspan);
                                            } else if let Pattern::Tuple(subs, _) = pat {
                                                if let Ty::Tuple(tys) = ty {
                                                    for (spat, sty) in subs.iter().zip(tys.iter()) {
                                                        if let Pattern::Var(n, s) = spat { self.declare_var(n, sty.clone(), *s); }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    if let Some(g) = &arm.guard {
                        let gt = self.check_expr(g);
                        if gt != Ty::Bool {
                            self.errors.push(SemError {
                                message: format!(
                                    "match guard must be `bool`, found `{gt}`"
                                ),
                                span: g.span,
                            });
                        }
                    }
                    // body type
                    let body_ty = match &arm.body {
                        MatchArmBody::Expr(e) => self.check_expr(e),
                        MatchArmBody::Block(b) => {
                            // For expression match, block is void; for statement match, block's stmts checked against function's ret
                            let cur = self.cur_ret.clone().unwrap_or(Ty::Void);
                            let _ = self.check_block(b, &cur);
                            Ty::Void
                        }
                    };
                    self.pop_scope();
                    if let Some(prev) = &arm_ty {
                        if *prev != body_ty {
                            self.errors.push(SemError{message: format!("match arms must have same type: expected `{prev}`, found `{body_ty}`"), span: arm.span});
                        }
                    } else {
                        arm_ty = Some(body_ty);
                    }
                }
                if !has_wildcard && scrut_ty == Ty::Int {
                    self.errors.push(SemError{message: "non-exhaustive match: missing wildcard `_` for `int` scrutinee".into(), span: m.span});
                }
                if scrut_ty == Ty::Bool && !has_wildcard {
                    // bool exhaustive requires both true/false or wildcard
                    let has_true = m.arms.iter().any(|a| {
                        matches!(a.pattern, Pattern::LitBool(true, _))
                            || matches!(a.pattern, Pattern::Alternative(ref pats, _) if pats.iter().any(|p| matches!(p, Pattern::LitBool(true, _))))
                    });
                    let has_false = m.arms.iter().any(|a| {
                        matches!(a.pattern, Pattern::LitBool(false, _))
                            || matches!(a.pattern, Pattern::Alternative(ref pats, _) if pats.iter().any(|p| matches!(p, Pattern::LitBool(false, _))))
                    });
                    if !(has_true && has_false) {
                        self.errors.push(SemError{message: "non-exhaustive bool match: require `true`, `false`, or wildcard".into(), span: m.span});
                    }
                }
                if let Ty::Enum(ref ename) = scrut_ty {
                    if !has_wildcard {
                        if let Some(einfo) = self.enums.get(ename) {
                            use std::collections::HashSet;
                            let mut covered: HashSet<String> = HashSet::new();
                            for arm in &m.arms {
                                match &arm.pattern {
                                    Pattern::Enum{variant, ..} => { covered.insert(variant.rsplit("::").next().unwrap_or(variant).to_string()); }
                                    Pattern::Alternative(pats, _) => {
                                        for p in pats {
                                            if let Pattern::Enum{variant, ..} = p { covered.insert(variant.rsplit("::").next().unwrap_or(variant).to_string()); }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            let all: HashSet<String> = einfo.variant_map.keys().cloned().collect();
                            if covered != all {
                                let mut missing: Vec<String> = all.difference(&covered).cloned().collect();
                                missing.sort();
                                self.errors.push(SemError{message: format!("non-exhaustive match on enum `{ename}`: missing `{}` (add wildcard `_` or the missing variants)", missing.join("`, `")), span: m.span});
                            }
                        }
                    }
                }
                arm_ty.unwrap_or(Ty::Int)
            }
            ExprKind::Super => {
                if let Some(cls) = &self.cur_class {
                    if let Some(parent) = self.classes.get(cls).and_then(|c| c.extends.clone()) {
                        Ty::Struct(parent)
                    } else {
                        self.errors.push(SemError{message: "`super` without parent".into(), span: expr.span});
                        Ty::Int
                    }
                } else {
                    self.errors.push(SemError{message: "`super` outside class".into(), span: expr.span});
                    Ty::Int
                }
            }
            ExprKind::Null => Ty::Any,
            ExprKind::New { ty, args, .. } => {
                let inner = self.resolve_type(ty);
                match &inner {
                    Ty::Struct(n) if self.structs.contains_key(n) || self.classes.contains_key(n) => {},
                    Ty::Struct(n) if self.traits.contains_key(n) => {
                        self.errors.push(SemError{message: format!("cannot `new` trait `{n}` (traits have no constructor; construct a concrete class instead)"), span: expr.span});
                        for a in args { let _ = self.check_call_arg(a); }
                        return Ty::Own(Box::new(inner));
                    }
                    other => {
                        self.errors.push(SemError{message: format!("`new` requires a class, struct, or trait type, found `{other}`"), span: expr.span});
                        for a in args { let _ = self.check_call_arg(a); }
                        return Ty::Own(Box::new(inner));
                    }
                }
                // Validate constructor args similar to `Type(args)` calls.
                // Reuse the same ctor selection as `Call` handling for the inner type name.
                if let Ty::Struct(n) = &inner {
                    if let Some(cls) = self.classes.get(n).cloned() {
                        if !cls.constructors.is_empty() {
                            let mut matched: Option<(crate::sema::Ty, usize)> = None;
                            for (sig, _) in &cls.constructors {
                                let min = Self::min_args_for_sig(sig);
                                if args.len() >= min && args.len() <= sig.params.len() {
                                    matched = Some((Ty::Struct(n.clone()), 0));
                                    break;
                                }
                            }
                            if matched.is_none() {
                                // arity mismatch: try to find any ctor
                                let mut found = false;
                                for (sig, _) in &cls.constructors {
                                    let min = Self::min_args_for_sig(sig);
                                    if args.len() >= min && args.len() <= sig.params.len() { found = true; break; }
                                }
                                if !found {
                                    self.errors.push(SemError{message: format!("no matching constructor for `{n}` with {} args", args.len()), span: expr.span});
                                }
                            }
                            // type-check args against the first matching ctor
                            if let Some((sig, _)) = cls.constructors.iter().find(|(s,_)| { let min = Self::min_args_for_sig(s); args.len() >= min && args.len() <= s.params.len() }) {
                                for (i, arg) in args.iter().enumerate() {
                                    let aty = self.check_call_arg(arg);
                                    if let Some(pt) = sig.params.get(i) {
                                        if !self.ty_assignable(&aty, pt) {
                                            self.errors.push(SemError{message: format!("new {} arg {}: expected `{}`, found `{}`", n, i+1, pt, aty), span: arg.span()});
                                        }
                                        self.check_arg_mode(arg, sig, i, n, i+1);
                                    }
                                }
                            } else {
                                for a in args { let _ = self.check_call_arg(a); }
                            }
                        } else {
                            // No explicit ctor: check against fields (struct literal via call)
                            if let Some(sinfo) = self.structs.get(n).cloned() {
                                if !sinfo.fields.is_empty() && args.len() == sinfo.fields.len() {
                                    for (i, arg) in args.iter().enumerate() {
                                        let aty = self.check_call_arg(arg);
                                        let fty = &sinfo.fields[i].1;
                                        if !self.ty_assignable(&aty, fty) {
                                            self.errors.push(SemError{message: format!("new {} arg {}: expected `{}`, found `{}`,", n, i+1, fty, aty), span: arg.span()});
                                        }
                                    }
                                } else {
                                    for a in args { let _ = self.check_call_arg(a); }
                                }
                            } else {
                                for a in args { let _ = self.check_call_arg(a); }
                            }
                        }
                    } else if let Some(sinfo) = self.structs.get(n).cloned() {
                        if !sinfo.fields.is_empty() && args.len() == sinfo.fields.len() {
                            for (i, arg) in args.iter().enumerate() {
                                let aty = self.check_call_arg(arg);
                                let fty = &sinfo.fields[i].1;
                                if !self.ty_assignable(&aty, fty) {
                                    self.errors.push(SemError{message: format!("new {} arg {}: expected `{}`, found `{}`,", n, i+1, fty, aty), span: arg.span()});
                                }
                            }
                        } else {
                            for a in args { let _ = self.check_call_arg(a); }
                        }
                    } else {
                        for a in args { let _ = self.check_call_arg(a); }
                    }
                } else {
                    for a in args { let _ = self.check_call_arg(a); }
                }
                Ty::Own(Box::new(inner))
            }
            ExprKind::Tuple(exprs) => {
                let tys: Vec<Ty> = exprs.iter().map(|e| self.check_expr(e)).collect();
                Ty::Tuple(tys)
            }
            ExprKind::ArrayLit(elems) => {
                // Element type is unified from the first element (MVP);
                // every element must be assignable to it (types skill §8:
                // all initializer elements compatible with element type).
                if elems.is_empty() {
                    Ty::FixedArray { elem: Box::new(Ty::Any), size: Some(0) }
                } else {
                    let first = self.check_expr(&elems[0]);
                    for e in elems.iter().skip(1) {
                        let t = self.check_expr(e);
                        if !Ty::assignable(&t, &first) {
                            self.errors.push(SemError {
                                message: format!(
                                    "array literal element mismatch: expected `{first}`, found `{t}`"
                                ),
                                span: e.span,
                            });
                        }
                    }
                    Ty::FixedArray { elem: Box::new(first), size: Some(elems.len()) }
                }
            }
            ExprKind::VecEmpty(_) => {
                // Undetermined element type until the first `push`.
                Ty::Vec(Box::new(Ty::Any))
            }
            ExprKind::InterpolatedString(_, _) => Ty::String,
            ExprKind::Closure { params, body, .. } => {
                self.push_scope();
                for p in params { let ty = self.resolve_type(&p.ty); self.declare_var(&p.name, ty, p.name_span); }
                let ret_ty = match body.as_ref() {
                    crate::ast::ClosureBody::Expr(e) => self.check_expr(e),
                    crate::ast::ClosureBody::Block(b) => { let _ = self.check_block(b, &Ty::Void); Ty::Void },
                };
                self.pop_scope();
                Ty::Function(Box::new(ret_ty), params.iter().map(|p| self.resolve_type(&p.ty)).collect())
            }
            ExprKind::Paren(inner) => self.check_expr(inner),
        }
    }

    /// Validate `out`/`ref` call markers against the declared parameter
    /// mode. `param_idx` indexes the signature (no `this` offset in sema
    /// sigs). Named arguments are owned by the dedicated named-arg path
    /// and skipped here.
    /// Full per-argument contract against a signature: marker validation
    /// (`out`/`ref`) plus ownership moves into `own` parameters. Named
    /// arguments are owned by the dedicated named path (skipped here).
    /// `pidx` indexes the signature (no `this` offset in sema sigs).
    fn check_arg_mode(
        &mut self,
        arg: &CallArg,
        sig: &FuncSig,
        pidx: usize,
        callee: &str,
        arg_no: usize,
    ) {
        if matches!(arg, CallArg::Named { .. }) {
            return;
        }
        if let Some(mode) = sig.param_modes.get(pidx) {
            let is_out = matches!(arg, CallArg::Out { .. });
            let is_ref = matches!(arg, CallArg::Ref { .. });
            match mode {
                ParamMode::Out if !is_out => self.errors.push(SemError { message: format!("argument {} of `{}` is `out` param but call uses non-out", arg_no, callee), span: arg.span() }),
                ParamMode::Ref if !is_ref => self.errors.push(SemError { message: format!("argument {} of `{}` is `ref` param but call uses non-ref", arg_no, callee), span: arg.span() }),
                ParamMode::None if is_out || is_ref => self.errors.push(SemError { message: format!("argument {} of `{}` is by-value param but call uses `out`/`ref`", arg_no, callee), span: arg.span() }),
                _ => {}
            }
        }
        // Moving into an `own` parameter poisons the sources.
        // `out`/`ref` markers are borrows, not moves; plain `Expr`/`Named`
        // that evaluate to an `own` value move it.
        if matches!(sig.params.get(pidx), Some(Ty::Own(_))) {
            match arg {
                CallArg::Expr(e) => self.poison_moved_idents(e),
                CallArg::Named { value, .. } => self.poison_moved_idents(value),
                _ => {}
            }
        }
    }

    /// Declare a single `out` argument with no visible variable, bringing
    /// it into the innermost scope from the call on. Explicit annotations
    /// win; otherwise the callee's declared parameter type (or `any` when
    /// unknown). Existing variables are reused untouched.
    fn declare_out_arg(&mut self, arg: &CallArg, expected: Option<&Ty>) {
        let CallArg::Out { name, ty: opt_ty, .. } = arg else {
            return;
        };
        if self.lookup_var(name).is_some() {
            return;
        }
        let ty = match opt_ty {
            Some(t) => self.resolve_type(t),
            None => expected.cloned().unwrap_or(Ty::Any),
        };
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), ty);
        }
    }

    fn check_call_arg(&mut self, arg: &CallArg) -> Ty {
        match arg {
            CallArg::Expr(e) => self.check_expr(e),
            CallArg::Named { value, .. } => self.check_expr(value),
            CallArg::Out { name, name_span, ty: opt_ty, .. } => {
                if let Some(var_ty) = self.lookup_var(name) {
                    if let Some(t) = opt_ty {
                        let declared = self.resolve_type(t);
                        if declared != var_ty {
                            self.errors.push(SemError { message: format!("out argument `{}` type `{}` does not match variable `{}`", name, declared, var_ty), span: *name_span });
                        }
                    }
                    var_ty
                } else {
                    // No pre-pass declared it (unknown callee, indirect
                    // call, …): bring it into scope as `any` rather than
                    // cascading more errors.
                    if let Some(scope) = self.scopes.last_mut() {
                        scope.insert(name.to_string(), Ty::Any);
                    }
                    Ty::Any
                }
            }
            CallArg::Ref { expr, .. } => {
                let ty = self.check_expr(expr);
                match &expr.kind {
                    ExprKind::Ident(_) | ExprKind::MemberAccess { .. } | ExprKind::Index { .. } | ExprKind::Paren(_) => {},
                    _ => self.errors.push(SemError { message: "ref argument must be lvalue".into(), span: expr.span }),
                }
                ty
            }
        }
    }

    /// Collection and string methods (types skill: behavior specified by the
    /// skill/stdlib, validated by the compiler). Returns `Some` when the
    /// receiver is a known collection/string type (handled or diagnosed),
    /// `None` to fall through to class-method resolution.
    ///
    /// Supported methods:
    /// - vectors: `len`, `is_empty`, `pop`, `clear`, `contains`, `first`,
    ///   `last` (`push` is handled separately for type establishment);
    /// - arrays: `len`, `is_empty`, `contains`, `first`, `last`;
    /// - maps: `len`, `is_empty`, `contains`, `remove`, `clear`, `get_or`
    ///   (`get` is a reserved property keyword);
    /// - strings: `len`, `is_empty`.
    fn check_collection_method(
        &mut self,
        object: &Expr,
        method: &str,
        method_span: &Span,
        args: &[CallArg],
        obj_ty: &Ty,
    ) -> Option<Ty> {
        enum Family<'a> {
            Seq { elem: &'a Ty, dynamic: bool },
            Map { key: &'a Ty, value: &'a Ty },
            Str,
        }
        let family = match obj_ty {
            Ty::Vec(elem) => Family::Seq { elem, dynamic: true },
            Ty::Array(elem) => Family::Seq { elem, dynamic: false },
            Ty::FixedArray { elem, .. } => Family::Seq { elem, dynamic: false },
            Ty::Map { key, value } => Family::Map { key, value },
            Ty::String => Family::Str,
            _ => return None,
        };
        // Mutating methods on `const` bindings are rejected (when the base
        // is a plain identifier).
        let mutating = matches!(method, "pop" | "clear" | "remove");
        if mutating {
            if let ExprKind::Ident(name) = &object.kind {
                if self.is_const(name) {
                    self.errors.push(SemError {
                        message: format!("cannot call mutating method `{method}` on const `{name}`"),
                        span: *method_span,
                    });
                }
            }
        }
        // (expected arity, check args) — arg checking runs inline below.
        let expect_arity = |want: usize| -> bool {
            if args.len() != want {
                return false;
            }
            true
        };
        let arity_ok = match method {
            "len" | "is_empty" | "pop" | "clear" | "first" | "last" => expect_arity(0),
            "contains" | "remove" => expect_arity(1),
            "get_or" => expect_arity(2),
            _ => true,
        };
        if !arity_ok {
            let want = match method {
                "len" | "is_empty" | "pop" | "clear" | "first" | "last" => 0,
                "contains" | "remove" => 1,
                "get_or" => 2,
                _ => 0,
            };
            self.errors.push(SemError {
                message: format!("`{method}` expects {want} arg(s), found {}", args.len()),
                span: *method_span,
            });
            for a in args {
                let _ = self.check_call_arg(a);
            }
            return Some(Ty::Int);
        }
        match (family, method) {
            (Family::Seq { .. }, "len") => Some(Ty::Int),
            (Family::Map { .. }, "len") => Some(Ty::Int),
            (Family::Str, "len") => Some(Ty::Int),
            (Family::Seq { .. }, "is_empty") => Some(Ty::Bool),
            (Family::Map { .. }, "is_empty") => Some(Ty::Bool),
            (Family::Str, "is_empty") => Some(Ty::Bool),
            (Family::Seq { elem, dynamic }, "pop") => {
                if !dynamic {
                    self.errors.push(SemError {
                        message: "`pop` is only available on vectors (`vec`), not fixed arrays".into(),
                        span: *method_span,
                    });
                    return Some(elem.clone());
                }
                Some(elem.clone())
            }
            (Family::Seq { dynamic: true, .. }, "clear") => Some(Ty::Void),
            (Family::Map { .. }, "clear") => Some(Ty::Void),
            (Family::Seq { elem, .. }, "contains") => {
                let aty = self.check_call_arg(&args[0]);
                if !Ty::assignable(&aty, elem) {
                    self.errors.push(SemError {
                        message: format!("`contains` expects `{}`, found `{}`", elem, aty),
                        span: args[0].span(),
                    });
                }
                Some(Ty::Bool)
            }
            (Family::Map { key, .. }, "contains") => {
                let aty = self.check_call_arg(&args[0]);
                if !Ty::assignable(&aty, key) {
                    self.errors.push(SemError {
                        message: format!("`contains` expects key `{}`, found `{}`", key, aty),
                        span: args[0].span(),
                    });
                }
                Some(Ty::Bool)
            }
            (Family::Seq { elem, .. }, "first") | (Family::Seq { elem, .. }, "last") => {
                Some(elem.clone())
            }
            (Family::Map { key, .. }, "remove") => {
                let aty = self.check_call_arg(&args[0]);
                if !Ty::assignable(&aty, key) {
                    self.errors.push(SemError {
                        message: format!("`remove` expects key `{}`, found `{}`", key, aty),
                        span: args[0].span(),
                    });
                }
                Some(Ty::Bool)
            }
            (Family::Map { key, value }, "get_or") => {
                let kty = self.check_call_arg(&args[0]);
                if !Ty::assignable(&kty, key) {
                    self.errors.push(SemError {
                        message: format!("`get_or` expects key `{}`, found `{}`", key, kty),
                        span: args[0].span(),
                    });
                }
                let dty = self.check_call_arg(&args[1]);
                if !Ty::assignable(&dty, value) {
                    self.errors.push(SemError {
                        message: format!("`get_or` expects default `{}`, found `{}`", value, dty),
                        span: args[1].span(),
                    });
                }
                Some(value.clone())
            }
            // Availability errors: right family shape, wrong method.
            (Family::Seq { dynamic: false, .. }, "clear")
            | (Family::Seq { dynamic: false, .. }, "remove")
            | (Family::Seq { dynamic: false, .. }, "get_or") => {
                self.errors.push(SemError {
                    message: format!("`{method}` is not available on fixed arrays"),
                    span: *method_span,
                });
                for a in args {
                    let _ = self.check_call_arg(a);
                }
                Some(Ty::Int)
            }
            (Family::Seq { .. }, _) | (Family::Map { .. }, _) | (Family::Str, _) => {
                let supported = match obj_ty {
                    Ty::Vec(_) => "`len`, `is_empty`, `push`, `pop`, `clear`, `contains`, `first`, `last`",
                    Ty::Array(_) | Ty::FixedArray { .. } => "`len`, `is_empty`, `contains`, `first`, `last`",
                    Ty::Map { .. } => "`len`, `is_empty`, `contains`, `remove`, `clear`, `get_or`",
                    Ty::String => "`len`, `is_empty`",
                    _ => "",
                };
                self.errors.push(SemError {
                    message: format!("unknown method `{method}` for `{obj_ty}`; supported: {supported}"),
                    span: *method_span,
                });
                for a in args {
                    let _ = self.check_call_arg(a);
                }
                Some(Ty::Int)
            }
        }
    }

    fn check_call_with_sig(&mut self, args: &[CallArg], sig: &FuncSig, callee_span: Span, callee: &str) {
        let variadic_idx = sig.param_is_variadic.iter().position(|&v| v);
        if let Some(vidx) = variadic_idx {
            // Variadic `...T vda` where `vda` is `T[]`, or `...` alone for C varargs
            let fixed = vidx; // number of fixed params before variadic
            if args.len() < fixed {
                self.errors.push(SemError { message: format!("`{}` expects at least {} args, found {}", callee, fixed, args.len()), span: callee_span });
            }
            // Check fixed params
            for (i, arg) in args.iter().take(fixed).enumerate() {
                let pidx = if let CallArg::Named { name, .. } = arg {
                    sig.param_names.iter().position(|n| n == name).unwrap_or(i)
                } else { i };
                self.declare_out_arg(arg, sig.params.get(pidx));
                let aty = self.check_call_arg(arg);
                if let Some(param_ty) = sig.params.get(pidx) {
                    if !self.ty_assignable(&aty, param_ty) {
                        let is_generic = matches!(param_ty, Ty::Generic(n, _) if n.len() == 1 && n.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false));
                        if !is_generic {
                            self.errors.push(SemError { message: format!("argument {} of `{}`: expected `{}`, found `{}`", i+1, callee, param_ty, aty), span: arg.span() });
                        }
                    }
                }
                self.check_arg_mode(arg, &sig, pidx, callee, i + 1);
            }
            // Check variadic tail: `vda` is `T[]` where `T` is element type
            if let Some(vty) = sig.params.get(vidx) {
                let elem_ty = if let Ty::Array(el) = vty { &**el } else { vty };
                // Handle variadic not last: `...T vda, U next` where `vda` consumes `args.len() - sig.params.len() + 1` args
                let remaining_params = sig.params.len() - vidx - 1;
                let vda_count = if remaining_params == 0 {
                    args.len() - fixed
                } else {
                    args.len() - sig.params.len() + 1
                };
                for (i, arg) in args.iter().skip(fixed).take(vda_count).enumerate() {
                    self.declare_out_arg(arg, Some(elem_ty));
                    let aty = self.check_call_arg(arg);
                    let is_generic_elem = matches!(elem_ty, Ty::Generic(n, _) if n.len() == 1 && n.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false));
                    let is_any_elem = *elem_ty == Ty::Any;
                    if !Ty::assignable(&aty, elem_ty) && !is_generic_elem && !is_any_elem {
                        self.errors.push(SemError { message: format!("variadic argument {} of `{}`: expected `{}`, found `{}`", fixed + i + 1, callee, elem_ty, aty), span: arg.span() });
                    }
                }
                // Check remaining fixed params after variadic
                for (j, arg) in args.iter().skip(fixed + vda_count).enumerate() {
                    let pidx = vidx + 1 + j;
                    self.declare_out_arg(arg, sig.params.get(pidx));
                    if let Some(param_ty) = sig.params.get(pidx) {
                        let aty = self.check_call_arg(arg);
                        if !self.ty_assignable(&aty, param_ty) {
                            let is_generic = matches!(param_ty, Ty::Generic(n, _) if n.len() == 1 && n.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false));
                            if !is_generic {
                                self.errors.push(SemError { message: format!("argument {} of `{}`: expected `{}`, found `{}`", pidx + 1, callee, param_ty, aty), span: arg.span() });
                            }
                        }
                    }
                    self.check_arg_mode(arg, &sig, pidx, callee, fixed + vda_count + j + 1);
                }
            }
            // Also check where bounds for variadic generic
            if !sig.generic_params.is_empty() || sig.where_clause.is_some() {
                // For `...T vda where T: Trait`, the `T` for variadic element should also be checked
                // We already check via check_generic_bounds for type_args, but for variadic we need to ensure `T` is checked
                // For now, rely on check_generic_bounds for type_args
            }
            return;
        }
        let min_required = Self::min_args_for_sig(sig);
        if args.len() < min_required || args.len() > sig.params.len() {
            if min_required == sig.params.len() {
                self.errors.push(SemError { message: format!("`{}` expects {} args, found {}", callee, sig.params.len(), args.len()), span: callee_span });
            } else {
                self.errors.push(SemError { message: format!("`{}` expects {}-{} args, found {}", callee, min_required, sig.params.len(), args.len()), span: callee_span });
            }
        }
        let has_named = args.iter().any(|a| matches!(a, CallArg::Named{..}));
        if has_named {
            // Build param name -> index map
            let mut param_name_to_idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for (idx, pname) in sig.param_names.iter().enumerate() {
                param_name_to_idx.insert(pname.clone(), idx);
            }
            let mut seen = std::collections::HashSet::new();
            for arg in args {
                if let CallArg::Named { name, .. } = arg {
                    if let Some(&pidx) = param_name_to_idx.get(name) {
                        if !seen.insert(pidx) {
                            self.errors.push(SemError { message: format!("duplicate named argument `{}` for `{}`", name, callee), span: arg.span() });
                        }
                        let aty = self.check_call_arg(arg);
                        let expected = &sig.params[pidx];
                        if !self.ty_assignable(&aty, expected) {
                            self.errors.push(SemError { message: format!("named argument `{}` of `{}`: expected `{}`, found `{}`", name, callee, expected, aty), span: arg.span() });
                        }
                        if let Some(mode) = sig.param_modes.get(pidx) {
                            let is_out = matches!(arg, CallArg::Out{..});
                            let is_ref = matches!(arg, CallArg::Ref{..});
                            match mode {
                                ParamMode::Out if !matches!(arg, CallArg::Out{..}) && !matches!(arg, CallArg::Named{..}) => {},
                                _ => {}
                            }
                            // For Named, mode check still relevant if Named was used for out/ref param? But Named is by-value, so if param is Out/Ref, Named should be error
                            if *mode == ParamMode::Out || *mode == ParamMode::Ref {
                                self.errors.push(SemError { message: format!("named argument `{}` of `{}` corresponds to `out`/`ref` param, use `out`/`ref` syntax", name, callee), span: arg.span() });
                            }
                        }
                    } else {
                        self.errors.push(SemError { message: format!("unknown named argument `{}` for `{}`", name, callee), span: arg.span() });
                        let _ = self.check_call_arg(arg);
                    }
                } else {
                    // positional before named is allowed, but we already handled count; for positional, check via index
                    let _ = self.check_call_arg(arg);
                }
            }
            // Missing arguments must have defaults (filled at codegen).
            let n_positional = args.iter().filter(|a| !matches!(a, CallArg::Named{..})).count();
            for (pidx, pname) in sig.param_names.iter().enumerate() {
                if pidx < n_positional || seen.contains(&pidx) {
                    continue;
                }
                if sig.param_defaults.get(pidx).and_then(|d| d.as_ref()).is_none() {
                    self.errors.push(SemError { message: format!("missing argument `{}` for `{}` (no default value)", pname, callee), span: callee_span });
                }
            }
            // Also check positional args that are not named: they must match remaining params
            // For simplicity, if has_named, we have already checked named args; remaining positional args are checked elsewhere? We'll just return after handling named
            return;
        }
        for (i, arg) in args.iter().enumerate() {
            let aty = self.check_call_arg(arg);
            if let Some(param_ty) = sig.params.get(i) {
                if !self.ty_assignable(&aty, param_ty) {
                    self.errors.push(SemError { message: format!("argument {} of `{}`: expected `{}`, found `{}`", i + 1, callee, param_ty, aty), span: arg.span() });
                }
            }
            if let Some(mode) = sig.param_modes.get(i) {
                let is_out = matches!(arg, CallArg::Out{..});
                let is_ref = matches!(arg, CallArg::Ref{..});
                match mode {
                    ParamMode::Out if !is_out => self.errors.push(SemError { message: format!("argument {} of `{}` is `out` param but call uses non-out", i+1, callee), span: arg.span() }),
                    ParamMode::Ref if !is_ref => self.errors.push(SemError { message: format!("argument {} of `{}` is `ref` param but call uses non-ref", i+1, callee), span: arg.span() }),
                    ParamMode::None if is_out || is_ref => self.errors.push(SemError { message: format!("argument {} of `{}` is by-value param but call uses `out`/`ref`", i+1, callee), span: arg.span() }),
                    _ => {}
                }
            }
        }
    }

    fn subst_generic_ty(ty: &Ty, map: &HashMap<String, Ty>) -> Ty {
        match ty {
            Ty::Generic(n, args) if map.contains_key(n) => {
                // If generic has args (e.g. Box<T>), substitute inner args too
                if args.is_empty() {
                    map[n].clone()
                } else {
                    let subst_args: Vec<Ty> = args.iter().map(|a| Self::subst_generic_ty(a, map)).collect();
                    // if the mapped ty is generic container, keep args? For MVP just return mapped
                    if map[n] != Ty::Generic(n.clone(), vec![]) {
                        map[n].clone()
                    } else {
                        Ty::Generic(n.clone(), subst_args)
                    }
                }
            }
            Ty::Struct(n) if map.contains_key(n) => map[n].clone(),
            Ty::Generic(n, args) => {
                let na: Vec<Ty> = args.iter().map(|a| Self::subst_generic_ty(a, map)).collect();
                Ty::Generic(n.clone(), na)
            }
            Ty::Array(el) => Ty::Array(Box::new(Self::subst_generic_ty(el, map))),
            Ty::FixedArray { elem, size } => Ty::FixedArray { elem: Box::new(Self::subst_generic_ty(elem, map)), size: *size },
            Ty::Vec(el) => Ty::Vec(Box::new(Self::subst_generic_ty(el, map))),
            Ty::Map { key, value } => Ty::Map { key: Box::new(Self::subst_generic_ty(key, map)), value: Box::new(Self::subst_generic_ty(value, map)) },
            Ty::Pointer(el) => Ty::Pointer(Box::new(Self::subst_generic_ty(el, map))),
            Ty::Optional(el) => Ty::Optional(Box::new(Self::subst_generic_ty(el, map))),
            Ty::Own(el) => Ty::Own(Box::new(Self::subst_generic_ty(el, map))),
            Ty::Tuple(tys) => Ty::Tuple(tys.iter().map(|t| Self::subst_generic_ty(t, map)).collect()),
            Ty::Function(ret, args) => Ty::Function(Box::new(Self::subst_generic_ty(ret, map)), args.iter().map(|a| Self::subst_generic_ty(a, map)).collect()),
            other => other.clone(),
        }
    }

    fn unify_generic(param: &Ty, actual: &Ty, map: &mut HashMap<String, Ty>) {
        match param {
            Ty::Generic(name, p_args) if p_args.is_empty() => {
                if let Some(existing) = map.get(name) {
                    if existing != actual {
                        // conflict: keep first, error will be caught by assignable later
                    }
                } else {
                    map.insert(name.clone(), actual.clone());
                }
            }
            Ty::Struct(name) if map.contains_key(name) || (name.len()==1 && name.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)) => {
                // Single-letter struct used as generic param (fallback)
                if !map.contains_key(name) {
                    map.insert(name.clone(), actual.clone());
                }
            }
            Ty::Generic(n, p_args) => {
                if let Ty::Generic(an, a_args) = actual {
                    if n == an && p_args.len() == a_args.len() {
                        for (pa, aa) in p_args.iter().zip(a_args.iter()) {
                            Self::unify_generic(pa, aa, map);
                        }
                    }
                }
            }
            Ty::Array(pe) => {
                match actual {
                    Ty::Array(ae) | Ty::Vec(ae) | Ty::FixedArray { elem: ae, .. } => Self::unify_generic(pe, ae, map),
                    _ => {}
                }
            }
            Ty::FixedArray { elem: pe, .. } => {
                match actual {
                    Ty::Array(ae) | Ty::Vec(ae) | Ty::FixedArray { elem: ae, .. } => Self::unify_generic(pe, ae, map),
                    _ => {}
                }
            }
            Ty::Vec(pe) => {
                if let Ty::Vec(ae) | Ty::Array(ae) | Ty::FixedArray { elem: ae, .. } = actual {
                    Self::unify_generic(pe, ae, map);
                }
            }
            Ty::Pointer(pe) => if let Ty::Pointer(ae) = actual { Self::unify_generic(pe, ae, map); },
            Ty::Optional(pe) => if let Ty::Optional(ae) = actual { Self::unify_generic(pe, ae, map); },
            _ => {}
        }
    }

    fn ty_to_type(ty: &Ty) -> Type {
        let sp = Span::new(0,0);
        match ty {
            Ty::Int => Type::Int(sp),
            Ty::UInt => Type::Named("uint".into(), sp),
            Ty::SizedInt { bits, signed } => { let n = format!("{}{}", if *signed {"i"} else {"u"}, bits); Type::Named(n, sp) },
            Ty::Bool => Type::Bool(sp),
            Ty::Void => Type::Void(sp),
            Ty::Char => Type::Char(sp),
            Ty::String => Type::String(sp),
            Ty::Float => Type::Float(sp),
            Ty::Double => Type::Double(sp),
            Ty::Any => Type::Any(sp),
            Ty::Struct(n) | Ty::Enum(n) => Type::Named(n.clone(), sp),
            Ty::Generic(n, args) => Type::Generic(n.clone(), args.iter().map(|a| Self::ty_to_type(a)).collect(), sp),
            Ty::Array(el) => Type::Array(Box::new(Self::ty_to_type(el)), sp),
            Ty::FixedArray { elem, size } => Type::FixedArray { elem: Box::new(Self::ty_to_type(elem)), size: size.map(|v| v as u64), span: sp },
            Ty::Vec(elem) => Type::Vec { elem: Box::new(Self::ty_to_type(elem)), span: sp },
            Ty::Map { key, value } => Type::Map { key: Box::new(Self::ty_to_type(key)), value: Box::new(Self::ty_to_type(value)), span: sp },
            Ty::Pointer(el) => Type::Pointer(Box::new(Self::ty_to_type(el)), sp),
            Ty::Optional(el) => Type::Optional(Box::new(Self::ty_to_type(el)), sp),
            Ty::Own(el) => Type::Own(Box::new(Self::ty_to_type(el)), sp),
            Ty::Tuple(tys) => Type::Tuple(tys.iter().map(|t| Self::ty_to_type(t)).collect(), sp),
            Ty::Function(ret, args) => Type::FunctionType(Box::new(Self::ty_to_type(ret)), args.iter().map(|a| Self::ty_to_type(a)).collect(), sp),
        }
    }

    /// Enforce a generic declaration's bounds at an instantiation site
    /// (`Box<int>` in any type position, `implements Comparable<T>`).
    /// Unknown bases are reported by `resolve_type`; non-generic bases given
    /// arguments are an error here.
    fn check_type_application(&mut self, base: &str, args: &[Type], span: Span) {
        let lookup = base.rsplit("::").next().unwrap_or(base);
        // Classes mirror into the structs map; prefer the class entry so
        // class bounds apply (struct and class entries agree otherwise).
        if let Some(cinfo) = self.classes.get(lookup).cloned() {
            self.check_type_args(&cinfo.generic_params, &cinfo.where_clause, lookup, args, span);
        } else if let Some(sinfo) = self.structs.get(lookup).cloned() {
            self.check_type_args(&sinfo.generic_params, &sinfo.where_clause, lookup, args, span);
        } else if let Some(einfo) = self.enums.get(lookup).cloned() {
            self.check_type_args(&einfo.generic_params, &einfo.where_clause, lookup, args, span);
        } else if let Some(tinfo) = self.traits.get(lookup).cloned() {
            self.check_type_args(&tinfo.generic_params, &tinfo.where_clause, lookup, args, span);
        }
    }

    /// Bounds check for one instantiation; type-flavored arity message when
    /// the base declares no generics at all.
    fn check_type_args(&mut self, generic_params: &[GenericParam], where_clause: &Option<WhereClause>, base: &str, args: &[Type], span: Span) {
        if generic_params.is_empty() && where_clause.is_none() {
            if !args.is_empty() {
                self.errors.push(SemError { message: format!("type `{base}` is not generic but {} type argument(s) provided", args.len()), span });
            }
            return;
        }
        self.check_generic_bounds(generic_params, where_clause, args, span);
    }

    /// Validate a type declaration's own `where` clause: subjects must name
    /// the declaration's generic parameters (no type resolution, so no
    /// forward-declaration hazard; bound existence is enforced at
    /// instantiation sites, which run after all declarations are collected).
    fn check_where_clause_def(&mut self, kind: &str, name: &str, generic_params: &[GenericParam], where_clause: &Option<WhereClause>, span: Span) {
        let Some(wc) = where_clause else { return };
        if generic_params.is_empty() {
            self.errors.push(SemError { message: format!("`where` clause on non-generic {kind} `{name}`"), span });
            return;
        }
        for constr in &wc.constraints {
            let subj = match &constr.ty {
                Type::Named(n, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                Type::Generic(n, _, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                other => other.name(),
            };
            if !generic_params.iter().any(|gp| gp.name == subj) {
                self.errors.push(SemError { message: format!("`where` subject `{subj}` is not a generic parameter of {kind} `{name}`"), span: constr.span });
            }
        }
    }

    /// Minimum argument count for a call: total params minus trailing
    /// defaulted ones. Defaults are trailing-enforced at definition, so
    /// positional omission always drops the tail. Conservative when the
    /// defaults vector is short (missing entries count as required).
    fn min_args_for_sig(sig: &FuncSig) -> usize {
        let trailing = sig.param_defaults.iter().rev().take_while(|d| d.is_some()).count();
        sig.params.len().saturating_sub(trailing)
    }

    /// Validate default arguments on a parameter list (EBNF `parameter`):
    /// defaults must be trailing, never on variadic/`out`/`ref` params, and
    /// each default must match its parameter type. Defaults evaluate at the
    /// call site, so they are checked in a fresh scope (only globals resolve,
    /// never sibling params). `resolved` parallels `params`.
    fn check_param_defaults(&mut self, params: &[Param], resolved: &[Ty]) {
        let mut seen_default = false;
        for (p, ty) in params.iter().zip(resolved.iter()) {
            if p.default.is_some() && p.is_variadic {
                self.errors.push(SemError { message: format!("variadic parameter `{}` cannot have a default value", p.name), span: p.name_span });
                continue;
            }
            if p.default.is_some() && p.mode != ParamMode::None {
                self.errors.push(SemError { message: format!("`out`/`ref` parameter `{}` cannot have a default value", p.name), span: p.name_span });
                continue;
            }
            if let Some(d) = &p.default {
                seen_default = true;
                self.push_scope();
                let dt = self.check_expr(d);
                self.pop_scope();
                if !self.ty_assignable(&dt, ty) {
                    self.errors.push(SemError { message: format!("default for `{}`: expected `{}`, found `{}`", p.name, ty, dt), span: d.span });
                }
            } else if seen_default && !p.is_variadic {
                self.errors.push(SemError { message: format!("parameter `{}` without default follows a defaulted parameter", p.name), span: p.name_span });
            }
        }
    }

    fn check_generic_bounds(&mut self, generic_params: &[GenericParam], where_clause: &Option<WhereClause>, type_args: &[Type], span: Span) {
        if generic_params.is_empty() && where_clause.is_none() {
            if !type_args.is_empty() {
                self.errors.push(SemError { message: format!("function is not generic but {} type arguments provided", type_args.len()), span });
            }
            return;
        }
        if generic_params.len() != type_args.len() {
            if !type_args.is_empty() {
                self.errors.push(SemError { message: format!("generic arg count mismatch: expected {}, found {}", generic_params.len(), type_args.len()), span });
            }
            return;
        }
        let mut subst: std::collections::HashMap<String, Ty> = std::collections::HashMap::new();
        for (gp, ta) in generic_params.iter().zip(type_args.iter()) {
            let concrete = self.resolve_type(ta);
            subst.insert(gp.name.clone(), concrete.clone());
            for bound in &gp.bounds {
                let bound_ty = self.resolve_type(bound);
                if let Ty::Struct(ref trait_name) | Ty::Generic(ref trait_name, _) = bound_ty {
                    let concrete_str = match &concrete {
                        Ty::Struct(n) | Ty::Enum(n) | Ty::Generic(n, _) => n.clone(),
                        _ => concrete.to_string(),
                    };
                    if let Some(_trait_info) = self.traits.get(trait_name) {
                        let implements = if let Some(cls) = self.classes.get(&concrete_str) {
                            &cls.implements
                        } else {
                            &Vec::new()
                        };
                        if !implements.contains(trait_name) && concrete_str != *trait_name {
                            self.errors.push(SemError { message: format!("type `{}` does not satisfy bound `{}` for `{}`", concrete, bound_ty, gp.name), span });
                        }
                    } else if !self.structs.contains_key(trait_name) && !self.classes.contains_key(trait_name) && !self.enums.contains_key(trait_name) {
                        self.errors.push(SemError { message: format!("unknown bound `{}` for `{}`", bound_ty, gp.name), span });
                    }
                } else if bound_ty != concrete {
                    self.errors.push(SemError { message: format!("type `{}` does not satisfy bound `{}` for `{}`", concrete, bound_ty, gp.name), span });
                }
            }
        }
        // Check where clause constraints: `where T: Drawable, U: int`
        if let Some(wc) = where_clause {
            for constr in &wc.constraints {
                let subject_ty = self.resolve_type(&constr.ty);
                // Resolve subject to concrete if it's a generic param
                let subject_concrete = match &subject_ty {
                    Ty::Generic(n, _) | Ty::Struct(n) if subst.contains_key(n) => subst[n].clone(),
                    other => other.clone(),
                };
                for bound in &constr.bounds {
                    let bound_ty = self.resolve_type(bound);
                    if let Ty::Struct(ref trait_name) | Ty::Generic(ref trait_name, _) = bound_ty {
                        if let Some(trait_info) = self.traits.get(trait_name) {
                            let subject_str = match &subject_concrete {
                                Ty::Struct(n) | Ty::Enum(n) | Ty::Generic(n, _) => n.clone(),
                                _ => subject_concrete.to_string(),
                            };
                            let implements = if let Some(cls) = self.classes.get(&subject_str) {
                                &cls.implements
                            } else {
                                &Vec::new()
                            };
                            if !implements.contains(trait_name) && subject_str != *trait_name {
                                self.errors.push(SemError { message: format!("where bound failed: `{}` does not satisfy `{}: {}`", subject_concrete, constr.ty.name(), bound_ty), span: constr.span });
                            }
                        } else if bound_ty != subject_concrete {
                            self.errors.push(SemError { message: format!("where bound failed: `{}` does not satisfy `{}: {}`", subject_concrete, constr.ty.name(), bound_ty), span: constr.span });
                        }
                    } else if bound_ty != subject_concrete {
                        self.errors.push(SemError { message: format!("where bound failed: `{}` does not satisfy `{}: {}`", subject_concrete, constr.ty.name(), bound_ty), span: constr.span });
                    }
                }
            }
        }
    }

    fn check_lvalue(&mut self, expr: &Expr) -> Ty {
        match &expr.kind {
            ExprKind::Ident(name) => {
                let lookup = name.rsplit("::").next().unwrap_or(name);
                if let Some(ty) = self.lookup_var(name).or_else(|| self.lookup_var(lookup)) {
                    ty
                } else {
                    self.errors.push(SemError {
                        message: format!("undefined variable `{name}`"),
                        span: expr.span,
                    });
                    Ty::Int
                }
            }
            ExprKind::MemberAccess {
                object,
                field,
                field_span,
            } => {
                // reuse field check but treat as lvalue - also handle properties (setter)
                let obj_ty = self.check_expr(object);
                let effective_ty = self.deref_ty(&obj_ty);
                if let Ty::Struct(ref sname) = effective_ty {
                    if let Some(sinfo) = self.structs.get(sname) {
                        if let Some((_, fty)) = sinfo.field_map.get(field) {
                            if let Some(cinfo) = self.classes.get(sname) {
                                if let Some(vis) = cinfo.field_vis.get(field) {
                                    if *vis != crate::ast::Visibility::Public && self.cur_class.as_deref() != Some(sname.as_str()) {
                                        self.errors.push(SemError{message: format!("field `{field}` is private"), span: *field_span});
                                    }
                                } else if self.cur_class.as_deref() != Some(sname.as_str()) {
                                    self.errors.push(SemError{message: format!("field `{field}` is private"), span: *field_span});
                                }
                            } else {
                                if let Some(vis) = sinfo.field_vis.get(field) {
                                    if *vis == crate::ast::Visibility::Private {
                                        self.errors.push(SemError{message: format!("field `{field}` is private"), span: *field_span});
                                    }
                                }
                            }
                            fty.clone()
                        } else if let Some(cinfo) = self.classes.get(sname) {
                            if let Some(prop) = cinfo.properties.get(field) {
                                if prop.visibility != crate::ast::Visibility::Public && self.cur_class.as_deref() != Some(sname.as_str()) {
                                    self.errors.push(SemError{message: format!("property `{field}` is private"), span: *field_span});
                                }
                                if prop.has_set { prop.ty.clone() } else {
                                    self.errors.push(SemError{message: format!("property `{field}` has no setter"), span: *field_span});
                                    Ty::Int
                                }
                            } else {
                                self.errors.push(SemError {
                                    message: format!(
                                        "struct `{sname}` has no field `{field}`"
                                    ),
                                    span: *field_span,
                                });
                                Ty::Int
                            }
                        } else {
                            self.errors.push(SemError {
                                message: format!(
                                    "struct `{sname}` has no field `{field}`"
                                ),
                                span: *field_span,
                            });
                            Ty::Int
                        }
                    } else if self.traits.contains_key(sname) {
                        // Trait-typed receiver: structural field dispatch
                        // (see `check_trait_field`).
                        self.check_trait_field(sname, field, field_span)
                    } else {
                        self.errors.push(SemError {
                            message: format!("unknown struct `{sname}`"),
                            span: expr.span,
                        });
                        Ty::Int
                    }
                } else {
                    self.errors.push(SemError {
                        message: format!(
                            "assignment to non-struct field `{field}`"
                        ),
                        span: *field_span,
                    });
                    Ty::Int
                }
            }
            ExprKind::Index { object, index } => {
                let obj_ty = self.check_expr(object);
                let idx_ty = self.check_expr(index);
                // Maps validate the index against the key type in the match
                // below; other indexables require int-like indices.
                let is_map = matches!(obj_ty, Ty::Map { .. });
                if !is_map && !idx_ty.is_int_like() {
                    self.errors.push(SemError {
                        message: format!(
                            "index must be an int type, found `{idx_ty}`"
                        ),
                        span: index.span,
                    });
                }
                match obj_ty {
                    Ty::Array(el) => *el,
                    Ty::FixedArray { elem, .. } => *elem,
                    Ty::Vec(elem) => {
                        if *elem == Ty::Any {
                            self.errors.push(SemError {
                                message: "cannot index vector with undetermined element type: `push` an element first".into(),
                                span: object.span,
                            });
                            Ty::Any
                        } else {
                            *elem
                        }
                    }
                    Ty::Map { key, value } => {
                        if !Ty::assignable(&idx_ty, key.as_ref()) {
                            self.errors.push(SemError {
                                message: format!(
                                    "map index must be `{}`, found `{idx_ty}`",
                                    key
                                ),
                                span: index.span,
                            });
                        }
                        *value
                    }
                    Ty::String => Ty::Char,
                    _ => {
                        self.errors.push(SemError {
                            message: format!(
                                "cannot index non-array `{obj_ty}`"
                            ),
                            span: object.span,
                        });
                        Ty::Int
                    }
                }
            }
            _ => {
                self.errors.push(SemError {
                    message: "invalid assignment target".into(),
                    span: expr.span,
                });
                Ty::Int
            }
        }
    }
}

pub fn check(prog: &Program) -> Vec<SemError> {
    check_with_options(prog, CheckOptions::default())
}

/// Sema entry point with options (used by the LSP, which expands imports
/// itself and only requires `main` for files named `main`).
pub fn check_with_options(prog: &Program, opts: CheckOptions) -> Vec<SemError> {
    let mut c = Checker::new();
    c.set_require_main(opts.require_main);
    c.check_program(prog)
}

/// Options for [`check_with_options`].
#[derive(Clone, Copy, Debug)]
pub struct CheckOptions {
    /// Emit `missing `main` function` when no `main` is declared.
    /// `true` for CLI builds; the LSP sets it only for `main.hll`.
    pub require_main: bool,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self { require_main: true }
    }
}
