//! Async runtime requirement analysis (Async-8).
//!
//! The async runtime is linked only when the program can actually reach
//! async code. Merely importing a module that *declares* an unused async
//! function must not pull the runtime in, and a purely synchronous program
//! must acquire no scheduler symbols and no async dependencies — so the
//! decision is a call-graph reachability question, never a textual keyword
//! scan and never a linker dead-strip assumption.
//!
//! Rules:
//! * Roots are `main`, `init` blocks, and global variable initializers —
//!   everything the C runtime can reach on its own.
//! * Edges are call targets (free functions and class/extension methods),
//!   plus bare identifiers that name a function (function pointers and
//!   function-typed values decay to a call the compiler cannot resolve
//!   statically). Generic instances and indirect calls are therefore
//!   conservative: any named function mention is followed.
//! * `spawn`/`await` inside a reachable body require the runtime outright
//!   (they are only legal inside `async` bodies, which already require it).
//! * An `async main` is itself a root requirement: it runs as a root task.
//!
//! The same analysis tells codegen which async declarations to skip: an
//! unreachable async function must not be emitted, or its body would
//! reference runtime symbols in a program that never links the runtime.

use std::collections::{HashMap, HashSet};

use crate::ast::*;

/// Result of the reachability analysis.
#[derive(Debug, Clone, Default)]
pub struct AsyncReach {
    /// The program needs the async runtime linked.
    pub needed: bool,
    /// Free-function names reachable from the roots (used by codegen to
    /// exclude unreachable async declarations before emission).
    pub reachable: HashSet<String>,
}

fn unwrap_attr(item: &Item) -> &Item {
    match item {
        Item::Attributed { item, .. } => item.as_ref(),
        other => other,
    }
}

/// Mutable analysis state.
struct Reach<'a> {
    funcs: HashMap<String, &'a Function>,
    async_methods: HashSet<String>,
    reachable: HashSet<String>,
    worklist: Vec<&'a Function>,
    needed: bool,
}

impl<'a> Reach<'a> {
    fn mark_fn(&mut self, name: &str) {
        if self.reachable.contains(name) {
            return;
        }
        if let Some(f) = self.funcs.get(name) {
            self.reachable.insert(name.to_string());
            self.worklist.push(*f);
        }
    }

    fn visit(&mut self, e: &Expr) {
        match &e.kind {
            // Only legal inside async bodies; both require the runtime.
            ExprKind::Await { .. } | ExprKind::Spawn { .. } => self.needed = true,
            ExprKind::Call { callee, .. } => {
                self.mark_fn(callee);
            }
            ExprKind::MethodCall { method, .. } => {
                if self.async_methods.contains(method) {
                    self.needed = true;
                }
            }
            // Bare mention: function value / function-pointer target.
            ExprKind::Ident(name) => {
                self.mark_fn(name);
                if self.async_methods.contains(name) {
                    self.needed = true;
                }
            }
            _ => {}
        }
    }
}

/// Analyze `prog` for async runtime requirements (see module docs).
pub fn analyze(prog: &Program) -> AsyncReach {
    let mut st = Reach {
        funcs: HashMap::new(),
        async_methods: HashSet::new(),
        reachable: HashSet::new(),
        worklist: Vec::new(),
        needed: false,
    };
    let mut init_blocks: Vec<&Block> = Vec::new();
    let mut global_inits: Vec<&Expr> = Vec::new();

    // ── Index declarations ───────────────────────────────────────────
    for item in &prog.items {
        match unwrap_attr(item) {
            Item::Function(f) => {
                st.funcs.insert(f.name.clone(), f);
            }
            Item::Class(c) => {
                for m in &c.methods {
                    if m.is_async {
                        st.async_methods.insert(m.name.clone());
                    }
                }
            }
            Item::Extension(e) => {
                for m in &e.members {
                    if let ExtensionMember::Function(f) = m {
                        if f.is_async {
                            st.async_methods.insert(f.name.clone());
                        }
                    }
                }
            }
            Item::Init(b) => init_blocks.push(b),
            Item::Var(v) => {
                if let Some(init) = &v.init {
                    global_inits.push(init);
                }
            }
            _ => {}
        }
    }

    // ── Roots ────────────────────────────────────────────────────────
    if let Some(main) = st.funcs.get("main") {
        if main.is_async {
            st.needed = true;
        }
    }
    st.mark_fn("main");

    for b in &init_blocks {
        crate::lint::walk::block_exprs(b, &mut |e| st.visit(e));
    }
    for e in &global_inits {
        st.visit(e);
    }

    // ── Fixpoint over reachable bodies ──────────────────────────────
    while let Some(f) = st.worklist.pop() {
        if f.is_async {
            st.needed = true;
        }
        crate::lint::walk::function_exprs(f, &mut |e| st.visit(e));
    }

    // Belt and braces: any reachable async function implies the runtime.
    if !st.needed {
        st.needed = st
            .reachable
            .iter()
            .any(|n| st.funcs.get(n).map(|f| f.is_async).unwrap_or(false));
    }

    AsyncReach { needed: st.needed, reachable: st.reachable }
}

/// True when `prog` must link the async runtime (Async-8).
pub fn uses_async_runtime(prog: &Program) -> bool {
    analyze(prog).needed
}

/// True when `prog` must link the sync runtime (`runtime/hella_sync.c`,
/// B1 mutex + channels, B2 wall/monotonic-clock + TCP + sleep/yield).
/// Detection is declaration-based: any `extern` function named
/// `hella_mutex_*` / `hella_chan_*` / `hella_wall_*` / `hella_monotonic_*` /
/// `hella_tcp_*` / `hella_sleep_ms` (declared by `std::sync` / `std::chan` /
/// `std::time` / `std::net` / `std::task`) pulls it in. Pure-Hella
/// programs pay nothing.
/// (`hella_task_self`/`hella_task_cancelled` stay async-only: `cancelled()`
/// requires an async context; `sleepMs`/`yieldNow` work everywhere.)
pub fn uses_sync_runtime(prog: &Program) -> bool {
    for item in &prog.items {
        let it = unwrap_attr(item);
        if let Item::Extern(ext) = it {
            for mem in &ext.members {
                if let ExternMember::Function { name, .. } = mem {
                    if name.starts_with("hella_mutex_")
                        || name.starts_with("hella_chan_")
                        || name.starts_with("hella_wall_")
                        || name.starts_with("hella_monotonic_")
                        || name.starts_with("hella_tcp_")
                        || name == "hella_sleep_ms"
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prog(src: &str) -> Program {
        let lexed = crate::lexer::lex(src);
        assert!(lexed.errors.is_empty(), "lex: {:?}", lexed.errors);
        crate::parse::parse(lexed.tokens, src.to_string()).expect("parse")
    }

    #[test]
    fn synchronous_program_needs_no_runtime() {
        let p = prog("void main() do\n    int x = 1\nend\n");
        assert!(!uses_async_runtime(&p));
    }

    #[test]
    fn imported_unused_async_decl_does_not_need_runtime() {
        // The async function exists but nothing in main reaches it.
        let src = "int slow(int n) async do\n    return n\nend\n\nvoid main() do\n    int x = 1\nend\n";
        let p = prog(src);
        assert!(!uses_async_runtime(&p));
        assert!(!analyze(&p).reachable.contains("slow"));
    }

    #[test]
    fn reachable_async_function_needs_runtime() {
        let src = "int slow(int n) async do\n    return n\nend\n\nvoid main() do\n    task<int> t = slow(1)\nend\n";
        let p = prog(src);
        assert!(uses_async_runtime(&p));
        assert!(analyze(&p).reachable.contains("slow"));
    }

    #[test]
    fn async_main_needs_runtime() {
        let src = "void main() async do\n    int x = 1\nend\n";
        assert!(uses_async_runtime(&prog(src)));
    }
}
