---
name: hella-stdlib-real
description: Real Hella standard library — compiler owns only primitives + extern FFI; all user-facing types, methods and terminal IO live in pure Hella under stdlib/. Use when implementing or extending any std:: module, removing compiler intrinsics, or deciding where a builtin belongs.
---

# Real Stdlib — Bare-Minimum Compiler Contract

Companion to `hella-stdlib` (import mechanics, EBNF §32) and `hella-compiler`
(lowerings). This file answers one question: **does this builtin belong in
the compiler or in `stdlib/`?**

## Rule

**The compiler knows only what Hella cannot say.** Everything a user can call
by name — types-as-API, methods, terminal IO — lives in `stdlib/` as pure
Hella on top of `extern "c"` FFI. The compiler never hardcodes a stdlib
function name in sema or codegen.

## Compiler owns (bare minimum)

1. **Core pipeline** — lex / parse / sema / LLVM codegen for EBNF syntax,
   primitive representations:
   `int→i64`, `bool→i1`, `char→i32`, `string→ptr (i8* null-terminated)`,
   `float→f32`, `double→f64`, sized ints/uints, `T[]/vec/map/option` layouts,
   `alloca` locals, globals, string literals, interpolation buffers, `defer`
   stacks, class dispatch scaffolding.
2. **`extern "c"` FFI (§36)** — `declare_extern` lowering, C varargs
   (`...` alone), exact C ABIs (`i32 puts(string s)`,
   `i32 printf(string fmt, ...)`, `i32 putchar(char c)`, `int getchar()`).
3. **Internal libc helpers** — `get_or_declare_puts/printf/putchar/abort/
   strcpy/strcat/sprintf/strdup` exist ONLY for compiler-internal lowering
   (`assert` messages, string interpolation/concat). They must never dispatch
   on user-visible names (`print`, `println`, …).
4. **Reserved runtime names** — `strlen strcmp abort puts printf putchar
   strcat sprintf` are rejected as Hella `function` names (sema
   `RESERVED_RUNTIME`) so user definitions cannot collide with the external
   declarations the compiler emits. Declaring them via `extern` is the only
   legal path, and that path belongs to `stdlib/`.
5. **Import resolver** — textual inlining `qualified-name → <root>/<path>.hll`
   (`crates/hella-compiler/src/modules.rs`, shared by CLI and LSP): project root around
   `main.hll` first (local modules, `mod.hll` directory entries, cycle guard),
   then dev-checkout `stdlib/`, then `~/.hella/lib` (UNIX, via `hella setup`).
   Selective imports always carry the module's `extern` blocks **plus the
   transitive helper closure** (wanted symbols + everything they reference:
   helper functions, consts, globals, nested-import items), so
   `import std::str::{trim}` keeps `substring`/`allocateString`.

## Stdlib owns (pure Hella under `stdlib/`)

- **`std::io`** (`stdlib/std/io.hll`) — `print`, `println`, `eprint`,
  `eprintln`, and dynamically growing `readLine`. Numeric input uses
  `std::num` `parseInt(input, ref value)` with explicit failure/overflow handling.
  Removed typed print/input and fixed-buffer formatting helpers; see
  `stdlib/README.md` for migration, APIs and ownership limitations.
- **`std::types`** (`stdlib/std/types.hll`) — doc-only manifest of the
  implicit environment (`bool string i8…u128 int uint float double`). Declares
  nothing; importing is a no-op.
- **Library modules** — `std::str` / `std::num` / `std::vector` / `std::map` /
  `std::math` / `std::fs` / `std::env` / `std::path` / `std::fmt` / `std::rand` /
  `std::task` / `std::terminal::ansi`: thin Hella wrappers or `extend` blocks over the
  primitive layouts. The compiler provides the layout + indexing/iteration
  mechanics; the *named method surface* (`len`, `push`, …) is documented and
  where possible fronted in stdlib. Full migration of `vec/map/string`
  methods off hardcoded `Family` dispatch is tracked, not attempted in one
  pass (fixed-capacity `[16 x E]` buffers cannot be re-expressed in pure Hella
  today).

## Anti-patterns (real-stdlib era)

- No `is_stdlib_io_intrinsic` / `codegen_stdlib_io_body` / call-site
  shortcuts on user names in `crates/hella-compiler/src/codegen/mod.rs`.
- No sema shortcut that returns `Ty::Void` for `print`-family names without
  resolving a real signature (`crates/hella-compiler/src/sema/mod.rs`).
- No empty-body `void print(string s) do end` stubs in `stdlib/std/io.hll`
  that only typecheck because the compiler replaces the body.
- No example calling `print*` without `import std::io` (rely on import, not
  on ambient intrinsics).
- No new compiler-reserved user-facing name without a stdlib home and a
  migration note here.

## Workflow: moving a builtin to stdlib

1. Write the pure-Hella implementation in `stdlib/std/<mod>.hll` using only
   EBNF-stable syntax + `extern` for what Hella cannot say.
2. Delete the compiler intrinsic (sema shortcut + codegen body + call-site
   fast path). Keep internal helpers used by compiler lowering.
3. Add `import std::<mod>` to every example that uses the moved names.
4. Verify: `cargo build -p hella && ./target/debug/hella build examples/hello_io.hll
   && ./examples/hello_io` (+ `advanced`, `variadic`, `abstraction`), and
   `cargo test -p compiler`.
5. Update `stdlib/README.md` symbol index and this contract if the boundary
   moved.

Base directory for this skill: /Users/rivethorn/Dev/Hella/hella/.opencode/skills/stdlib
Relative paths (`stdlib/`, `compiler/`, `examples/`) are relative to workspace root unless noted.
