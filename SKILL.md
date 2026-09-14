---
name: hella-compiler
description: Build or extend a compiler for the Hella programming language in Rust with an LLVM backend (inkwell). Use when the user asks to implement, design, parse, typecheck, or codegen any part of Hella, references the Hella EBNF or language spec, or wants phased compiler construction for this language.
---

# Hella Compiler (Rust + LLVM)

Guide LLMs building a compiler for **Hella** (draft 0.2 EBNF) using Rust and `inkwell`. Workspace `hella` has three crates under `crates/`: `hella-cli` (the `hella build` CLI + progress), `hella-compiler` (library: lexer, parser, sema, codegen), and `hella-lsp` (language server).

## Core Principles

1. **Phase, do not boil the ocean.** Full Hella (classes, traits, generics, match, defer, FFI, interpolation, …) is large. Always implement a working subset that emits native code before expanding. Phases 0–5 are DONE (`references/phases.md`) but many EBNF productions remain stubs — consult the Audit Gap Matrix before claiming 100%.
2. **Spec is authoritative.** The EBNF in `references/ebnf-0.1.txt` (Draft 0.2) is the source of truth for syntax. Do not invent syntax that contradicts it. Recent fixes: `get`/`set` are `public` by default and may be declared in two separate `property-declaration`s that merge; `open` is only for `class` (`open int bar()` is ill-formed); `struct-expression` allows `[type] has` with inferred `Type` for `User u2 = has … end` (not `any`).
3. **Spans everywhere.** Every AST node and diagnostic must carry source spans from day one (`miette` + `token::Span`).
4. **LLVM via inkwell.** Prefer `inkwell` over raw `llvm-sys`. Match the Cargo feature to the installed LLVM (`llvm-config --version` → `llvm21-1` in both `crates/hella-compiler/Cargo.toml:7` and `crates/hella-cli/Cargo.toml:12`).
5. **Monomorphize generics early.** Do not build a full trait solver or lifetime system before a working monomorphizing backend exists. Current MVP erases `T` → `i64` / `ptr` (`crates/hella-compiler/src/codegen/mod.rs:596`) — `where` bounds are parsed but unchecked (`crates/hella-compiler/src/parse/mod.rs:1092` / `sema` ignored).

## Required Reading (check before any edit)

- Full language grammar — `references/ebnf-0.1.txt` (Draft 0.2: `get`/`set` public default + separate merge, `open` class-only, optional `Type` in `has`)
- Phased implementation plan — `references/phases.md` (Phases 0–5 DONE, but see Gap Matrix: many stubs)
- LLVM lowering map — `references/llvm-mapping.md` (`int→i64`, `bool→i1`, `T[]→[16 x T]`, `T?→{T,bool}`, `T*→ptr`, `defer` stacks)
- Recommended crates and project layout — `references/toolchain.md` (**OUTDATED**: still lists `chumsky 0.10`, `llvm20-1`, `ariadne`; actual is `logos 0.15` + hand Pratt `crates/hella-compiler/src/parse/mod.rs:1`, `miette 7`, `inkwell 0.10 llvm21-1`, workspace `crates/hella-cli`+`crates/hella-compiler`+`crates/hella-lsp`)
- Main driver with progress — `crates/hella-cli/src/main.rs:1` (`hella build <file>` single `ProgressBar` + status lines in brand green #00A693)
- Canonical examples — `examples/advanced.hll` (distinct/extend/init/extern/where/operator/closure/interpolation), `examples/abstraction.hll` (separate accessors, `open` class), `examples/data_control.hll` (omitted `has`), `examples/hello_io.hll` (`import std::io`)
- Audit Gap Matrix — see `TodoWrite` “Hella 100% EBNF” (38 sections: 21% full, ~47% partial, 6 missing)

## Default Workflow

When asked to implement any part of the compiler:

1. Consult `TodoWrite` “Hella 100% EBNF” — it is the single source of truth for remaining work. Never work outside it.
2. Identify the smallest phase / EBNF § that contains the requested feature (see `references/phases.md` + `references/ebnf-0.1.txt`).
3. Confirm the relevant EBNF productions and check the Gap Matrix (Token? Parse? Sema? Codegen? Example?) before touching code.
4. Produce or extend (in order):
   - Token / lexer (`crates/hella-compiler/src/token.rs` — logos, keywords before `Ident`)
   - AST nodes (`crates/hella-compiler/src/ast.rs` — mirrors EBNF, `Span` on every node, `Type::__inferred__` for omitted `has`)
   - Parser rules (`crates/hella-compiler/src/parse/mod.rs` — recursive-descent + Pratt `parse_assignment` → `parse_postfix` → `parse_primary`; `try_parse_struct_literal`, `parse_class_decl`, `parse_extension_decl`, `parse_var_decl` for omitted `has`)
   - Semantic checks (`crates/hella-compiler/src/sema/mod.rs` — `resolve_type`, `check_expr`/`check_stmt`, `ClassInfo{operators, conversions}`, merging `prop_map` for separate accessors, `open`-on-method rejection)
   - `inkwell` lowering (`crates/hella-compiler/src/codegen/mod.rs` — `llvm_ty_for`/`llvm_ty_for_sema`, `declare_*`/`codegen_*`, `class_operators` dispatch, `closure_count`/`hella.init`/`strcat`/`sprintf`)
5. Prefer a compilable, testable increment over a complete but unrunnable design.
6. Emit via `hella build <file>` (single progress bar + brand-green status lines, `--release` for O3 + aggressive codegen via `Codegen::optimize_for_release`), then object `TargetMachine` + `clang` link to a proper binary (extension stripped). Always `module.verify()` before emission.
7. Update `examples/*.hll` to exercise the new production and `references/ebnf-0.1.txt` if grammar changed.

## Architecture Snapshot (actual workspace as of Phase 5 DONE)

```
source (.hll)
  → logos lexer (crates/hella-compiler/src/lexer.rs:1, token.rs:43 skip ws/comments, Newline/Semicolon terminators)
  → parser (hand-rolled recursive-descent + Pratt, crates/hella-compiler/src/parse/mod.rs:1, NOT chumsky — chumsky = dead dep crates/hella-compiler/Cargo.toml:9)
  → AST (mirrors EBNF Draft 0.2, crates/hella-compiler/src/ast.rs:1, Span on every node, Type::__inferred__ for omitted has, ExprKind::Closure/InterpolatedString)
  → sema (scopes, types, ClassInfo{operators, conversions}, prop_map merging, open check, crates/hella-compiler/src/sema/mod.rs:1)
  → codegen (inkwell 0.10 llvm21-1 Context/Module/Builder, crates/hella-compiler/src/codegen/mod.rs:1, llvm_ty_for, declare_*/codegen_*, class_operators dispatch, closure_count, hella.init, defer stacks, monomorph T→i64 erasure)
  → LLVM IR → TargetMachine object → clang link → binary (extension stripped)
          ↖ crates/hella-cli/src/main.rs:1 hella build CLI (subcommands incl. `setup`, single progress bar + status lines, timing; imports via `hella_compiler::modules`)
```

Actual layout:

```
hella/
├── crates/
│   ├── hella-cli/       # `hella build`/`hella run` binary (single progress bar + #00A693 status lines)
│   │   ├── src/main.rs  # Commands::Build/Run/Check/Lsp/Setup/New, single progress bar + status lines (#00A693), generate_ir_string, codegen_to_object (imports via `hella_compiler::modules`; no file arg → project `src/main.hll`, outputs to `out/debug|release/`)
│   │   └── build.rs     # embeds `stdlib/**/*.hll` for `hella setup`
│   ├── hella-compiler/  # compiler library (`hella_compiler`)
│   │   ├── src/lib.rs   # pub mod ast/codegen/error/lexer/modules/parse/sema/token
│   │   ├── src/token.rs, lexer.rs
│   │   ├── src/ast.rs
│   │   ├── src/parse/mod.rs # 2919 lines, parse_program, try_parse_struct_literal (omitted has), parse_class_decl (operator/convert), parse_extension_decl, parse_var_decl
│   │   ├── src/sema/mod.rs  # 1861 lines, check_program, ClassInfo merging
│   │   ├── src/codegen/mod.rs # 2919 lines, llvm_ty_for, compile_program (Attributed unwrapping, Typedef/Distinct/Extension/Extern/Init), Codegen{vars,funcs,struct_types,class_operators,closure_count}
│   │   ├── src/modules.rs   # import resolution (`hella.toml` / `main.hll` project root, `~/.hella/lib`)
│   │   └── src/error.rs     # miette Single/MultiDiagnostic
│   └── hella-lsp/       # language server library (`hella_lsp`, LSP over stdio)
│       └── src/         # server.rs, analysis.rs, diagnostics.rs, document.rs, lib.rs
├── examples/          # top-level .hll (basics, data_control with User omitted has, abstraction with separate accessors, advanced with all Phase 5, hello_io)
├── stdlib/std/io.hll  # pure-Hella stdlib (import std::io)
└── references/        # ebnf-0.1.txt Draft 0.2, phases.md DONE, llvm-mapping.md, toolchain.md (OUTDATED)
```

Do not add new crates without adding them to workspace `members` in the root `Cargo.toml`.

## Critical Language Design Points (do not ignore — recent fixes)

- Blocks are `do` … `end` (not `{}`).
- Variable declarations are type-first — `int x = 1` (no `let`). For `struct` or constructor-less `class`, initializer may be `has field = expr … end` with inferred `Type`: `User u2 = has name = "bbb" end` (`crates/hella-compiler/src/parse/mod.rs:1339`, EBNF §11 `[type] has`, not `any`).
- Statement terminators are newline **or** `;` (`token.rs:48` `Newline`/`Semicolon`, `parse/mod.rs:92` `consume_newlines`/`expect_terminator`).
- Class/struct/enum/trait bodies use `has` … `end`.
- Struct literals: `Type has field = expr … end` **or** omitted `has … end` (see above). `Type::Named("__inferred__")` placeholder resolved in `sema` via `decl_ty`.
- Match is `match expr do arms end` (`parse_match_expr:2149`, `codegen_match:2457`) with wildcard `_` required for `int`, exhaustive `true`/`false` for `bool`.
- `defer` exists and must run on all exit paths (`sema` `defer_stack` `loop_stack defer_depth`, `codegen_block` `emit_current_scope_defers`, `break`/`continue`/`return` `emit_defers_up_to`).
- Strings support interpolation `{ expression }` — handle in the parser (`parse_primary:2001` `InterpolatedString` via `crate::lexer::lex` re-parse inside `{}`), `{{`/`}}` escaped, codegen via `strcpy`/`strcat`/`sprintf`/`strdup` (`codegen/mod.rs:2354`).
- Two legal `main` signatures (`void main()` and `int main()`, EBNF §37 `string[] args` form not yet enforced — `sema/mod.rs:754` only checks `void main()`/`int main()` empty, codegen adapts `main` to `i32` ABI `codegen/mod.rs:734`).
- Keywords are reserved (full list `token.rs:57`, `EBNF §2` includes `convert`/`to`/`open`/`operator`/`distinct`/`typedef`/`float`/`double`/`any`/`Self`).
- **Properties:** `get`/`set` are `public` by default (`parse/mod.rs:500` `if vis==Default {Public}`) overriding class `private` default; constructors also `public` default. Two declarations with same `ident` in one class merge if complementary: `int x get … end` + `int x set(int v) … end` → one `PropertyInfo{has_get,has_set}` (`sema/mod.rs:473`/`codegen/mod.rs:330` merging `HashMap`).
- **`open` is only for `class`:** `open class Foo has …` enables `extend Foo do … end` (`token.rs:125` `Open`, `parse_class_decl:362` `is_open`). `open int bar()` is ill-formed → `method cannot be \`open\`` (`parse/mod.rs:401`), EBNF §22 comment.
- `extend` members: only `Function` currently lowered (`parse_extension_decl:871` `try parse_function else skip`, `sema` `642` `ExtensionMember::Function` → `classes[target].methods`, `codegen` `declare_extension:439` as `Target__method` with `this` ptr). Field/operator/property/conversion in `extend` are parsed as `advance` skip (EBNF §31 declares 4 variants, only `Function` implemented).
- `distinct` wraps as `opaque_struct_type { inner }` with `value:0` (`codegen/mod.rs:425`), `typedef` is no-op alias (`codegen/mod.rs:420`). `generic` `T` erases to `i64` (`llvm_ty_for:596` single-uppercase), `where` parsed but unchecked.
- **`own T` heap ownership:** `own T` lowers to `{data ptr, type tag}` pair (uniform with trait objects, `declare_pair_type` per `T`, tags via `assign_class_tags`). `new T(args)` heap-allocates (`malloc`/`free` via `get_or_declare_malloc/free`), runs the constructor on the heap object, yields the pair. `.` auto-derefs through `own` (field/method codegen unwraps the pair's data pointer, `infer_expr_ty` reports the inner type). `delete x` and scope exit destroy live `own` slots: `emit_own_destroy` guards on non-null data, dispatches destructors by tag (static for concrete, switch for trait), `free`s, and poisons the slot (stores null pair). Moves (`own B = a`, `own` args/returns) copy the pair and null the source slot (`poison_moved_idents` + `build_store` null in `codegen_arg_for_param`, `VarDecl`, `Assign`, `Return`); use-after-move and double-`delete` are sema errors via `poisoned` scopes, `null` never flows into `own`, `own` fields/globals and `own` inside containers are rejected, `extern` signatures cannot mention `own`/`trait` pairs, and bare `new` as a statement is a leak.

## Codegen Ground Rules (as implemented)

- Locals and parameters → `alloca` in the entry block (`create_entry_block_alloca:989` `builder.position_before(first)`), then load/store. `int` is `i64` (consistent, `main` truncates to `i32` for C ABI `codegen/mod.rs:734`, `43` `parent_field_count` warning).
- Prefer opaque pointers (`ptr`) on modern LLVM 21 (`ptr_type` `inkwell::AddressSpace::default()` everywhere, `struct_types` opaque).
- `defer` → per-scope `defer_stack: Vec<Vec<DeferStmt>>` (`codegen/mod.rs:44`) and `loop_stack.defer_depth`; `emit_current_scope_defers` / `emit_all_defers` / `emit_defers_up_to` on `break`/`continue`/`return`/`block` exit.
- Generics → **erasure MVP**: `T` single-uppercase → `i64` (`llvm_ty_for:596`, `llvm_ty_for_sema:658`, `declare_function:720` `Generic -> i64/ptr`), `Generic("MyInt")` wrapper struct for `distinct`, no true monomorph (`foo<int>` reuses `foo` `i64`). `where` parsed `parse_generic_params_opt:1092` but unchecked.
- Classes → explicit `this` as first `ptr` param (`declare_class:176` `param_llvm = vec![this_ty]`), mangled `Class__method` / `Class__op_plus` (`declare_class:263`), `__get_/__set_` for properties (`codegen_property:1129`), `__ctor` for constructors (`310`), `class_operators:44` dispatch for `BinOp` (`codegen/mod.rs:1831`), static `this` load/store. No `extends` vtables — `open`/`override`/`sealed` only validation (`sema` `564-605`), not dispatch. **Trait objects instead lower to `{data ptr, type tag}` pairs with closed-world switch dispatch** (`codegen_trait_method_call`/`codegen_trait_field_ptr`, tags in `assign_class_tags`): class→trait upcast on `implements` (+`extends` chain), trait methods resolve via the trait, anything else needs every implementor to agree (identical public signature) or sema errors.
- `distinct` → `opaque_struct_type { inner }` `Map value:0` (`codegen/mod.rs:425`), `typedef` → no-op.
- `own T` → pair lowering as above; no manual `malloc`/`free` in user code beyond `new`/`delete`.
- `extend` → declare as `Target__method` with `this` (`declare_extension:439`), `codegen_extension:521`.
- `extern` → `declare_extern:496` `module.add_function` with `llvm_ty_for_sema` for `Float->f32`/`Double->f64` etc., `Call` fallback `module.get_function` (`codegen/mod.rs:2147`), link via `clang` without `-lm` (`crates/hella-cli/src/main.rs:277`).
- Strings → `ptr` (`i8*`), `StringLit` via `build_global_string_ptr` (`codegen/mod.rs:586`), `InterpolatedString` via `strcpy`/`strcat`/`sprintf`/`strdup` into 512-byte `alloca` (`codegen/mod.rs:2354`).
- `float` literal → `f64.const_float` (`codegen/mod.rs:1734`, `ast.rs:494` `FloatLit(String)` kept `Eq`), `char` as `i32` but `codegen_expr CharLit` missing → `todo!()` (`codegen/mod.rs:2453` only remaining `todo!`).
- Never skip `module.verify()` before emission (`compile_program:125` / `crates/hella-cli/src/main.rs:256`).

## Error Handling & Progress

Use span-based diagnostics (`miette` `crates/hella-compiler/src/error.rs:1` `SingleDiagnostic`/`MultiDiagnostic`, `Report::new`, `token::Span::to_source_span`). Report:

- Unexpected token / incomplete construct (`lex:64`, `parse:104` `expect_terminator`, `SingleDiagnostic`)
- Undefined name / type (`sema` `undefined variable`, `unknown type`, `unknown struct T` `codegen/mod.rs:596`)
- Type mismatch (`sema` `type mismatch in initializer`, `argument X expects`)
- Missing return on non-void path (`sema` `check_block` `cur_ret`)
- Invalid `break`/`continue` target / label (`sema` `loop_stack`)
- `method cannot be open`, duplicate getter/setter, `field is private` (visibility `sema` `field_vis`/`method_vis`)

Progress output uses the Hella brand green #00A693 for `hella build`/`hella run` (`crates/hella-cli/src/main.rs` single `ProgressBar` + status lines, `Instant::now` timing per phase: `Reading`/`Lexing`/`Parsing`/`Resolving`/`Checking`/`Codegen`/`Linking`/`Compiled`).

## When the User Asks for a Specific Piece

| Request | Start from | Gap to check via Todo |
|---------|------------|---------------------|
| Lexer / tokens | EBNF §2–5, full keyword list `token.rs:57` (93 keywords + `EqEq` diag) | T-1: remove `chumsky` dead dep, add `TripleQuote` multiline body |
| Expressions | EBNF §8 (Pratt `parse_assignment`→`parse_postfix`→`parse_primary`) | T-2: `?:`/`??`/`?.`/`..`/`++`/`--`/`&| ^ ~ << >>` missing in `BinOp` `ast.rs:557` + `codegen` `todo!` |
| Statements / blocks | EBNF §13–20 | T-3: `const`/`destructuring` absent from `Stmt`/`parse_stmt` |
| Functions | EBNF §21 | T-4: `ref`/`out`/`initialize` for free fns, generic `where` unchecked |
| Types | EBNF §6–7 | T-5: `qualified-name ::`, `function<Ret(Args)>`, `tuple` type unreachable |
| Classes / structs / traits | EBNF §22–25 (Phase 4) | T-6: multi-payload enum, trait generic `where` |
| Match | EBNF §10 + §16 (Phase 2+) | T-7: `tuple-pattern`, `\|` alternative chain |
| Defer | EBNF §19 + control-flow exits | Done (see defer stacks) |
| FFI | EBNF §36 | T-8: `extern-struct/enum/const` only `function` parsed |
| Full driver | `hella build` `crates/hella-cli/src/main.rs:18` | T-9: `top-level var/const` not parsed, `int main(string[] args)` rejected, `assert` missing, `super`/`Self` parsing |

## Anti-Patterns (and what already went wrong)

- Implementing the entire language before any executable exists — use `TodoWrite` “Hella 100% EBNF” and do one `T-*` at a time.
- Skipping name resolution / type checking and lowering untyped AST (`sema` must run before `codegen`, see `crates/hella-cli/src/main.rs:197` `check(&program)`).
- Using SSA values for mutable locals instead of allocas (`codegen` `alloca` in entry block `codegen/mod.rs:989`).
- Inventing `{}` blocks or `let` bindings that are not in the EBNF (`do … end`, `int x = 1` type-first).
- Ignoring statement-terminator rules (`Newline`/`Semicolon` `token.rs:48`, `consume_newlines`/`expect_terminator`).
- Matching on string content for keywords instead of a proper keyword set (`logos` `#[token]` before `Ident` `token.rs:313`).
- Adding `open` to methods (`parse_class_decl:401` now correctly rejects `method cannot be open`).
- Making `get`/`set` private by default (now `public` default `parse/mod.rs:500` + merging `sema`/`codegen` `HashMap`).
- Forgetting `has` optional `Type` inference (`parse_var_decl:1339` `ty.clone()` for `User u2 = has …`).
- Leaving dead deps (`chumsky` `crates/hella-compiler/Cargo.toml:9` unused, `cargo` still pulls `Cargo.lock:159`) or stale docs (`references/toolchain.md:9` `llvm20-1` vs `llvm21-1`, `ariadne` vs `miette`).

## Output Expectations & Todo Discipline

- Prefer complete, compilable Rust fragments over pseudocode when implementing a phase.
- Always state which phase / EBNF § the work belongs to and which `Todo` `T-*` it closes.
- Cite the relevant EBNF productions when adding syntax (`references/ebnf-0.1.txt:633` etc.).
- Keep test programs in `examples/` as `.hll` files and verify with `hella build <file>` *and* `cargo test` (4 lexer tests) before marking `Todo` done.
- **Todo is law:** Never work outside `TodoWrite` “Hella 100% EBNF”. If user asks for ad-hoc fix, add it as a `T-*` first, then execute.

### Hella 100% EBNF — Reference Todo (keep in sync with `TodoWrite`)

*Generated from audit 2026-09-08 (38 sections: 21% full, 47% partial, 6 missing). See `references/ebnf-0.1.txt`.*

```
T-1  Toolchain/docs dead code: remove chumsky, sync llvm21-1, fix toolchain.md/README Layout
T-2  Literals: CharLit codegen (i32 const) + TripleQuote multiline body + Raw/interpolated edge
T-3  Types: qualified-name :: for types, function<Ret(Args)> , tuple (T,U) , parenthesized (T) in parse_type
T-4  Expressions: conditional ?: , null-coalesce ?? , nullable ?. , range .. / ..= , bitwise | ^ & ~ << >> , postfix ++/-- , compound assign += etc.
T-5  Call: named-argument ident: expr, out [type] ident, ref expr
T-6  Index: range form [ [expr] .. [expr] ] / [expr .. expr]
T-7  Primary: qualified-expression Ident::Ident, tuple (a,b,), super, Self, Null handling
T-8  Statements: const [type] ident = expr ; (top-level + local) and Stmt::Const
T-9  Statements: destructuring `a,b = expr` / `a,b,_ = expr` (decl + assign)
T-10 Statements: assert / debug_assert expr [,expr] ;
T-11 Functions: ref/out parameter-mode, initialize; for free fns, generic params + where preservation (currently Vec::new())
T-12 Traits: function-signature generic + where
T-13 Enums: multi-param payload (a,b) + discriminant expr (non-int) + generic enum monomorph
T-14 Variadic `...` in all functions — explicit `...T` anywhere, derived `...` must be last (int log(string fmt, ...int vda) / ... vda derived / ...string vda, bool cond + generics where)
T-15 Structs: field visibility + default = expr handling (currently ignored parse_struct_decl:326)
T-16 Where/bounds: enforce generic bounds and where-constraints (currently parsed but unchecked)
T-17 Match: | alternative chains, tuple-pattern (a,b), exhaustive enum handling
T-18 Loops: for over non-array (String iteration already), defer inside for
T-19 Functions: int main(string[] args) signature per EBNF §37 (sema currently rejects)
T-20 FFI: extern-struct has {field} end / extern-enum / extern-const const T N;
T-21 Extensions: field/operator/property/conversion members (currently only Function)
T-22 Top-level: variable-declaration / constant-declaration as top-level (parse_program fallback is Function)
T-23 Docs/examples: keep README Build & Run (hella build) and examples advanced/data_control/abstraction/variadic in sync
```

Check off via `TodoWrite` and `cargo run -p hella -- build examples/<file>` for each.
