# Hella LSP — TODO (easy → hard)

Single source of truth for LSP work. Ordered by effort/dependency: do in order,
one `L-*` at a time. Verify each with `cargo test -p hella-lsp`.

Current baseline (shipped): hover, goto-definition, completion (locals/symbols/
keywords, members after `.`, import paths, snippets), document symbols,
diagnostics (lex/parse/sema + imports + `main`-file gate), incremental sync,
UTF-16 positions, startup progress.

- [x] **L-1 Formatting — DONE: `textDocument/formatting` + `rangeFormatting` via `hella-fmt`** (`formatting.rs`, 5 tests, 43 total pass)
- [x] **L-2 Signature help — DONE: `textDocument/signatureHelp`** (`signatures.rs` text-scan + `lookup_callable`, 8 tests, 51 total pass)
- [x] **L-3 References — DONE: `textDocument/references`** (`Analysis::reference_spans`, lexer-driven, scope-narrowed, 5 tests, 56 total pass)
- [x] **L-4 Rename — DONE: `textDocument/rename` + `prepareRename`** (single-file `WorkspaceEdit` via L-3 spans, `valid_ident` rejects keywords/digits, `InvalidParams` on bad target)
- [x] **L-5 Workspace symbols — DONE: `workspace/symbol`** (fuzzy case-insensitive over open docs, deterministic URI order, 200 cap, `Flat` response)
- [x] **L-6 Document highlights — DONE: `textDocument/documentHighlight`** (L-3 core, decl = `Write`, uses = `Read`)
- [x] **L-3b Analysis helpers — DONE** (`anchor_for`, `highlights`, `word_span_at`, `valid_ident`, `search`, `Analysis: Clone`, `DocumentManager::uris`)
- [x] **L-7 Type definition + implementation — DONE** (`type_definition_span` with `own`/generic/array unwrap + self-resolve; `implementation_spans` via `Symbol.implements`; 6 tests, 65 total pass)
- [x] **L-8 Code actions — DONE: `textDocument/codeAction` quickfixes** (`actions.rs`: remove unresolvable import, bare-`new` → `own` bind, insert missing `end`; 4 tests, 69 total pass)
- [x] **L-9 Auto import — DONE: `textDocument/codeAction` + completion auto-import** (`auto_import.rs` workspace/stdlib scan, `code_actions_with_imports` + completion `additionalTextEdits`; 5 tests, 74 total pass)
- [ ] **L-10 Inlay hints — `textDocument/inlayHint`** — param-name hints at call sites (`param:`), inferred `let`-type hints for untyped decls, range-bounded. Tests: named-arg hint, inferred type hint.
- [ ] **L-11 Folding + selection ranges — `textDocument/foldingRange`, `selectionRange`** — fold `do…end`/`has…end` blocks + imports header; selection expands ident → stmt → block → decl. Tests: nested blocks fold, selection chain length.
- [ ] **L-12 Workspace index — multi-file, import-aware** — background scan of workspace `.hll` (cap ~500 files), imported-file symbol cache feeding completion/hover/definition, `didSave` re-index, file-watch invalidations. Tests: cross-file goto-def, import completion without open doc.
- [ ] **L-13 Robustness — debounce, cancel, config** — 150ms debounce on `didChange` analysis + diagnostics, per-request cancellation (`$/cancelRequest`), `initializationOptions` (`maxFiles`, `diagnosticsOnType`), no-panic guarantee (all `parse`/`sema` paths return, never `unwrap` on client input). Tests: rapid-change burst, cancel mid-index, bad config ignored.

**How to use:** mark `in_progress` one at a time, implement `analysis.rs` logic
first with unit tests, then `server.rs` wiring + capability, run
`cargo test -p hella-lsp`, then mark `completed`.
