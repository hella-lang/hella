---
name: hella-lsp
description: Language server for the Hella programming language — LSP over stdio reusing hella-compiler lex/parse/sema plus hella-fmt. Use when the user asks about IDE features, hover, completion, diagnostics, formatting, rename, references, or LSP robustness.
---

# Hella LSP

`crates/hella-lsp` speaks LSP over stdio (`lsp-server` 0.10 + `lsp-types` 0.97),
reusing `hella-compiler` for lex/parse/sema and `hella-fmt` for formatting so
IDE behavior matches `hella build` / `hella fmt`.

## Architecture

```
editor --LSP/stdio--> hella-lsp (crates/hella-lsp/src/)
  server.rs     lifecycle, dispatch, capabilities, open/change/close, publishDiagnostics
  analysis.rs   AST walk -> Symbol table -> hover/definition/completion/symbols (+ refs/rename/signatures)
  diagnostics.rs lex->parse->expand_imports->sema CheckOptions, span -> Diagnostic
  document.rs   open docs, incremental change application, offset<->Position (UTF-16), uri_to_path
  progress.rs   window/workDoneProgress/create + indexing scan
  formatting.rs hella-fmt wrapper (full-file + range, idempotent)
```

## Core principles

1. **Reuse the compiler.** Never re-lex by hand. Diagnostics and symbols go
   through `hella_compiler::lexer::lex` -> `parse::parse` ->
   `modules::expand_imports` -> `sema::check_with_options`, same as the CLI.
2. **Spans everywhere.** Compiler `token::Span` (byte offsets) converts via
   `document::span_to_range` / `offset_to_position` (UTF-16 columns).
3. **Error-tolerant.** Mid-typing buffers rarely parse. Completion and
   signatures use dummy-ident repair + `end`-balancing, never hang: malformed
   requests get `InvalidParams`, unknown methods get `MethodNotFound`.
4. **Project-aware.** `uri_to_path` drives import resolution
   (`modules::expand_imports`) and the `main`-file gate (only `main.hll`
   must define `main`). Imported names feed completion/member resolution.
5. **Verify with tests.** `cargo test -p hella-lsp` plus a driver script
   against a scratch `.hll` project before marking any `L-*` done.

## Capabilities (see TODO.md for status)

Hover, definition, completion (incl. members/imports/snippets),
document symbols, diagnostics, progress — shipped. Formatting, signature
help, references, rename, workspace symbols, code actions, inlay hints —
in TODO.md order, easy to hard.

## When adding a feature

1. Pick the next `L-*` in `.opencode/skills/lsp/TODO.md` (single source of truth).
2. Extend `analysis.rs` (pure logic, unit-tested) first, then wire
   capability + dispatch in `server.rs`.
3. Advertise in `server_capabilities()`, handle `extract::<Params>()` errors.
4. Add tests in-module (`analysis::tests`, `document::tests`,
   `diagnostics::tests`) and run `cargo test -p hella-lsp`.
