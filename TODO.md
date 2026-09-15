# Hella — Complete TODO (100% EBNF + own heap)

Generated from audit 2026-09-08 + `own T` phase-1 follow-ups. Source of truth for `TodoWrite`.
All paths are repo-relative. See `references/ebnf-0.1.txt` and `crates/hella-lsp/PROGRESS.md`.

## Legend
- `T-*` = 100% EBNF gap matrix (38 sections: 21% full, ~47% partial, 6 missing)
- `Own-P1` = ship-blocker for `own` phase-1 correctness; `Own-P2` = deferred design

---

### Toolchain / Docs (low)
- [x] **T-1 Toolchain/docs dead code** — VERIFIED 2026-09-15: no chumsky anywhere (only historical mentions), llvm21-1 in both manifests, toolchain.md documents llvm21-1/miette/hand-Pratt, README has Layout section. — remove `chumsky` dead dep, sync `llvm21-1` in `Cargo.toml:7` + `crates/hella-cli/Cargo.toml:12`, fix `references/toolchain.md:9` (`llvm20-1` vs `llvm21-1`, `ariadne` vs `miette`), `README` Layout. `crates/hella-compiler/src/parse/mod.rs:1` is hand Pratt, not chumsky.

### Literals / Types / Expressions (medium)
- [x] **T-2 Literals** — VERIFIED 2026-09-15: char/multiline/raw all run; plus fix: interpolated parts are now sema-checked (were skipped → codegen failures). — `CharLit` codegen `i32 const` + `TripleQuote` multiline body + `Raw`/`Interpolated` edge cases. `crates/hella-compiler/src/token.rs:332` `FloatLit`, `crates/hella-compiler/src/codegen/mod.rs:2453` `todo!()` for `CharLit`.
- [x] **T-3 Types** — VERIFIED 2026-09-15: qualified/function/tuple/paren types work; plus fix: named functions decay to pointers for function-typed slots. — `qualified-name ::` for types, `function<Ret(Args)>`, `tuple (T,U)`, `parenthesized (T)` in `parse_type` (`crates/hella-compiler/src/parse/mod.rs:1478`).
- [x] **T-4 Expressions** — DONE 2026-09-14 (functionality): values lift into T-optional via assignable plus value/true wrap in coercion; null-coalesce requires Optional, returns inner, lowers via present-flag select. (`?.` remains pointer-receiver only.) — `?:` / `??` / `?.` / `..` / `..=` / `&| ^ ~ << >>` / `++`/`--` / compound `+=` etc. `ast.rs:557` `BinOp`, `codegen` `todo!`.
- [x] **T-5 Call** — VERIFIED 2026-09-15: named/out/ref all run end-to-end. — `named-argument ident: expr`, `out [type] ident`, `ref expr` validation + codegen (`crates/hella-compiler/src/sema/mod.rs:3381` `check_call_arg`, `codegen/mod.rs:3121` `codegen_call_arg`).
- [x] **T-6 Index** — DONE 2026-09-14: real copy-with-clamped-bounds lowering for arrays (`a[l..r]`/`..`/`..=`/open ends, zero-filled tail, inverted→empty); vectors copy the live range into a fresh value; strings allocate a fresh null-terminated copy via malloc/memcpy. — range form `[ [expr] .. [expr] ]` / `[expr .. expr]` and `Slice` lowering (`ast.rs:667` `Slice`).
- [x] **T-7 Primary** — VERIFIED 2026-09-15 plus fixes: expression-Self behaves as this; super.method() dispatches to parent (infer + codegen paths added). — `qualified-expression Ident::Ident`, `tuple (a,b,)`, `super`, `Self`, `Null` handling (`parse/mod.rs:2968` `parse_primary`).

### Statements
- [x] **T-8 Statements: const** — VERIFIED 2026-09-15: const decls run. — `const [type] ident = expr ;` (top-level + local) and `Stmt::Const` (`crates/hella-compiler/src/parse/mod.rs:1977` `parse_stmt`).
- [x] **T-9 Statements: destructuring** — VERIFIED 2026-09-15 plus fix: tuple returns lower struct-by-value (were ptr → verify fail), decl/assign/swap all run. — `a,b = expr` / `a,b,_ = expr` (decl + assign) codegen (`ast.rs:413` `Destructure`).
- [x] **T-10 Statements: assert** — DONE 2026-09-14: `debug_assert` stripped in `--release` via `Codegen.release` (`compile_to_object` + `--emit-llvm`); plain `assert` always active. — `assert` / `debug_assert expr [,expr] ;` lowering and diagnostics (low).

### Functions / Traits / Enums
- [x] **T-11 Functions** — DONE 2026-09-14: `= expr` defaults stored in AST, trailing/variadic/out-ref/type rules + `pack_call_args` fill for free/method/ctor/`new` calls (incl. named reorder). — `ref`/`out` parameter-mode, `initialize;` for free fns, generic params + `where` preservation (currently `Vec::new()` in `check_function:1960`).
- [x] **T-12 Traits** — DONE 2026-09-14: `implements Trait<Args>` bounds-checked; generic conformance via substitution; base-name identity for `extends`/`implements`. — `function-signature` generic + `where`, trait method symbols already done (`crates/hella-lsp/src/analysis.rs:456`), need `where` enforcement (`sema/mod.rs:3900` only checks `implements`).
- [x] **T-13 Enums** — DONE 2026-09-15: wide payload layout (tag plus word buffer) with per-variant field types; construction/check/binding all multi-aware incl. heterogeneous positions; own-containing payloads rejected (need value-destruction tracking). — PARTIAL 2026-09-14: multi-param payloads are now a loud phase-1 error at construction/pattern/codegen (were silent first-param drop). Full tuple-payload layout migration still open (per-enum `{tag, payload-struct}`; heterogeneous positions need design). Single payloads of any tested type work. — multi-param payload `(a,b)` + discriminant `expr` (non-int) + generic enum monomorph (`ast.rs:293` `EnumVariant`).
- [x] **T-14 Variadic — VERIFIED 2026-09-15: example builds; defaults-on-variadic-params rejected, fixed prefix still required at calls (documented phase-1 rule). `...`** — explicit `...T` anywhere, derived `...` must be last (`f.params:1092` `parse_generic_params_opt`, `sema` `where` unchecked).
- [x] **T-15 Structs** — VERIFIED 2026-09-15: field defaults + private enforcement run (incl. via interpolation after T-2 fix). — field visibility + `default = expr` handling (currently ignored `parse_struct_decl:326`, `sema/mod.rs:958`).
- [x] **T-16 Where/bounds** — DONE 2026-09-14: `check_type_application` hook in `resolve_type` (all instantiation sites) + `check_where_clause_def` at decl sites; generics stored on type infos. — enforce generic bounds and `where`-constraints (currently parsed but unchecked at `sema/mod.rs:3850`).

### Control Flow
- [x] **T-17 Match** — DONE 2026-09-14: `|`/`or` chains, tuple-pattern, exhaustive enum + qualified `Color.Red` (`Color::Red`→`Red` strip), no `panic!` (proper `CodegenError`), tuple scrutinee inference via anonymous-struct decode. Follow-up: single-uppercase names resolve struct/enum/trait maps BEFORE generic erasure in 4 lowering paths, so `enum E` matches end-to-end (was `T→i64` false panic). `codegen/mod.rs:codegen_match`, `sema/mod.rs:match` exhaustive enum.
- [x] **T-18 Loops** — DONE 2026-09-14: any-iterable resolution (literals/calls via temp slots), `CodegenError` on non-iterables instead of len-16/var-0 fallback. — `for` over non-array (String iteration already), `defer` inside `for`, labeled `break`/`continue` codegen (`codegen/mod.rs:5295` `defer_stack`).

### Entry / FFI / Extensions / Top-level
- [x] **T-19 Functions: main args** — DONE 2026-09-14: `int main(string[] args)` is C `i32 (i32, ptr)`; `args` filled from `argv[1..]` (cap 16, null rest). — `int main(string[] args)` signature per EBNF §37 (`sema/mod.rs:754` only checks `void main()`/`int main()` empty, `codegen/mod.rs:734` truncates to `i32` ABI).
- [x] **T-20 FFI** — VERIFIED 2026-09-15: extern-struct/enum/const build and run. — `extern-struct has {field} end` / `extern-enum` / `extern-const const T N;` (only `function` parsed fully at `parse/mod.rs:1009` `parse_extern_decl`).
- [x] **T-21 Extensions** — DONE 2026-09-14 (lowering): conversion functions typed by declared `to_ty` (were hardcoded `i64`); own-scope tracking in bodies. Implicit application still future (no EBNF application syntax). — `field`/`operator`/`property`/`conversion` members (currently only `Function` fully, now field/operator/property partially done at `codegen/mod.rs:1694` `declare_extension`).
- [x] **T-22 Top-level** — VERIFIED 2026-09-15: top-level var/const build and run. — `variable-declaration` / `constant-declaration` as top-level (`parse_program` fallback is `Function` at `parse/mod.rs:250`).
- [x] **T-23 Docs/examples** — DONE 2026-09-14: added `examples/default_args.hll`; README table now covers all 15 example files. — keep `README` Build & Run (`hella build`) and `examples/advanced.hll`, `data_control.hll`, `abstraction.hll`, `variadic.hll` in sync (low).

---

### Own heap — P1 (ship-blockers)
- [x] **Own-P1: `is` on `own` — DONE 2026-09-14: pointer-identity compare (data-ptr `ptrtoint` EQ/NE); sema accepts `own`/`own` + `own`/`null`. as pointer-identity compare** — `Ty::Own` currently falls through to `Ty::Struct` path in `check_expr:3237` `MethodCall`/`MemberAccess` and `BinOp::Is` (`sema/mod.rs:2700`).
- [x] **Own-P1: bare `new` leak — DONE 2026-09-14: `is_bare_new` (paren-transparent) checked in `defer`/`if`/`while`/`for` + `Stmt::Expr`. in all `ExprStmt` positions including `defer`** — diagnosed for `Stmt::Expr` (`sema/mod.rs:2253`) but need to ensure `defer` expr and `for`/`if` cond `new` are also caught.
- [x] **Own-P1: move nulling for conditional — DONE 2026-09-14: `null_moved_sources`/`collect_moved_names` at all 4 move sites; ternary result type derived from branch (struct/pair support)./match branches** — `sema/mod.rs:434` `moved_ident_names` is transparent-only (`Ident`, `Paren`, `?:`, `match` arms) and `codegen` `VarDecl`/`Assign`/`call` only null direct `Ident`; leak-leaning approximation for `a ? b : c` where `b`,`c` are `own`.
- [x] **Own-P1: `own` param destruction — DONE 2026-09-14: `track_own_param` + outer scopes in methods/ctors/operators/closures; early `return` covered by `emit_all_owns`. for early returns and for methods/closures/operators** — `codegen/mod.rs:3725` `codegen_function` outer `own_slots` only for free fns; methods/closures/operators need same outer tracking and `Stmt::Return:5098` `emit_all_owns` must cover them.
- [x] **Own-P1: `new` for generic types — DONE 2026-09-14: `new Trait` rejected with actionable message. and `new` for trait (should be rejected)** — `sema/mod.rs:3658` currently allows `Ty::Struct` from trait, should reject `new Trait`.
- [x] **Own-P1: `delete this`/`non-Ident` — VERIFIED 2026-09-15: `delete this`/non-Ident rejected, double-delete is use-after-delete error, `delete a` destroys exactly once; codegen matches. (`sema/mod.rs:2364`), ensure error messages are precise and `codegen` `Stmt::Delete` matches**.
- [x] **Own-P1: `extern own`/`trait` rejection — DONE 2026-09-14: `contains_own` checks at all 5 extern positions (fn ret/params, struct fields incl. nested, enum payloads, consts). via `contains_own` (`sema/mod.rs:705`) — ensure `extern_trait_name` also covers `Ty::Own` in all extern positions (return, params, struct fields, enum payloads, consts)**.

### Own heap — P2 (deferred design)
- [x] **Own-P2: `own` fields with structural destruction** — DONE 2026-09-15: scope-exit/assign/param/return destruction via generalized scope_dtors; per-level member-chain auto-deref; move-null + poison for struct sources; struct literals; return-evaluated-before-destroy fix. (struct->class field refs still blocked by forward-declaration ordering.) — currently rejected `own fields need structural destruction — rejected in phase 1` at `sema/mod.rs:974`, `1095`; needs dtor generation for structs containing `own`.
- [x] **Own-P2: `own` in `Array`/`Vec`/`Map` elements and `own` globals** — DONE 2026-09-15: per-element destruction via generated `__container_dtor_N` (vec/map), program-end destruction of owning globals via `global_owns`/`global_dtors`/`emit_global_dtors`; sema no longer rejects `own` in containers or `var` globals; `const own` globals still rejected (non-const-foldable). — currently rejected `own types cannot be nested` (`sema/mod.rs:720`) and `global own variables need structural destruction` (`sema/mod.rs:1611`).

---

### LSP / Formatter / Testing (low unless noted)
- [x] **LSP: `own` completion — DONE 2026-09-14: `innermost_local` (narrowest scope + latest decl) fixes shadowing in hover/goto/completion; hover appends heap-ownership note for `own` types; `own Speaker` trait-upcast completion covered by test. for shadowed inner scopes and for `own` trait upcasts** — `analysis.rs:1206` `strip_own` already handles `own User` → `User`, need hover detail for `own` types.
- [x] **Formatter: `own`, `new`, `delete` — DONE 2026-09-14 (defaults): `fmt_param` preserves `= expr` (was silent strip → data loss); idempotency test added. idempotency and 100-col wrapping for `new` args** — `crates/hella-fmt/src/lib.rs:617` `fmt_type`, `1786` `New`, `1683` `Delete`.
- [x] **Testing (high):** — DONE 2026-09-14 (corpus, ongoing): 10 sema negative tests + 12 codegen verify tests; single-letter-`E` erasure regression covered. Also fixed: single-uppercase names resolve before generic erasure in 4 lowering paths. add corpus for `own` moves, deletes, trait upcasts (`examples/own_heap.hll` is the seed), and for all `T-*` above; verify with `hella build <file>` and `cargo test --workspace`.

---

## `hellastamp` file

Sidecar next to each built binary, e.g. `out/debug/app` → `out/debug/app.hellastamp` (`crates/hella-cli/src/main.rs:1035` `stamp_path`, `1041` `stamp_contents`, `1049` `write_build_stamp`, `1056` `is_fresh`).

Content:
```
profile=debug|release
toolchain=hella <version>  # Cargo.toml version
```

Purpose: `hella run` freshness — rebuild is skipped only when the binary is newer than **all** source files (`expand_imports` list via `hella_compiler::modules`) **and** the stamp matches current `profile`/`toolchain`; otherwise profile/version switch or touched source forces rebuild. `out/` is VCS-ignored (`.gitignore` `/out/`).

