# hella-lsp — Hella Language Server

Implementation tracking for the `hella-lsp` LSP binary.

## Goal

A Language Server for the Hella language, speaking the Language Server
Protocol over stdio, reusing the `compiler` crate for lex/parse/sema.

## Stack / rationale

| Crate | Version | Why |
|-------|---------|-----|
| `lsp-server` | 0.7.8 | Rust-analyzer's generic LSP scaffold: stdio `Connection`, message framing, crossbeam channels. All deps cached locally (offline-safe). |
| `lsp-types` | 0.97 | Typed LSP structures (requests, notifications, diagnostics, etc.). |
| `serde` / `serde_json` | 1 | Wire format (the LSP is JSON-RPC 2.0). |
| `crossbeam-channel` | 0.5 | Transitive from `lsp-server`. |
| `compiler` | path | Lex/parse/sema/AST reuse. |

## Progress log

- [x] Inspect workspace, `compiler` public API (`lex`, `parse`, `sema::check`), and AST.
- [x] Confirm offline dependency availability for `lsp-server` + `lsp-types`.
- [x] Scaffold `hella-lsp` crate (`Cargo.toml` with deps, module layout).
- [ ] Implement LSP server loop (introspect main, lifecycle).
- [ ] Document manager: open/change/close, byte &ndash; line/col utils.
- [ ] Diagnostics: lex/parse/sema errors -> `textDocument/publishDiagnostics`.
- [ ] Symbol table from AST (functions, structs, classes, enums, consts, locals, params).
- [ ] Features: hover, goto-definition, completion, document symbols.
  - [x] Completion: tiered locals/symbols/keywords; member completion after `.` (locals' struct/class types, `this`, enum variants, typedef resolution, unresolvable → global fallback); error-tolerant dummy-ident retry for mid-typing buffers; call snippets + `end`-closing block templates when the client advertises `snippetSupport`.
  - [x] Completion robustness: `end`-balancing for unclosed mid-typing blocks; import-path completion (`import std::|` lists modules/subdirs from the project + stdlib roots); names from directly imported files complete as globals (selective `::{a, b}` respected) and feed member-type resolution.
  - [x] Completion members: class properties (get/set, plain-name items) and constructor call snippets on the class name; extern-C params in snippets; buffer-wide dangling-dot repair so one half-typed `x.` doesn't sink completion elsewhere.
  - [x] Constructors/destructors: collected as outline symbols with body locals (params, vars) visible to completion/hover/definition; excluded from expression completion (`~C` never offered); operator/conversion bodies walked too. Malformed requests now get `InvalidParams` instead of hanging the client in silence.
  - [x] Extensions: members collected with class-identical treatment and clone-merged into the target type's children (outline shows them under the target); `this.` resolves inside `extend` blocks; `super.` walks the full `extends` chain (child-shadows-parent, cycle-guarded); imported extensions link into local/imported targets mirroring sema inlining.
  - [x] Trait methods: collected as `Method` children (outline, globals, hover/goto, call snippets); params kept as before.
  - [x] Trait objects: trait-typed locals complete the interface methods; diagnostics clean on trait-heavy files via the shared sema pipeline.
- [x] Server-initiated work-done progress (`src/progress.rs`): `window/workDoneProgress/create` after the initialize handshake, then `$/progress` begin/report/end around the startup `.hll` workspace scan ("Indexing Hella workspace"); silent on clients that reject `create`.
- [ ] Tests + manual verification with a driver script.