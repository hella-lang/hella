# Hella LSP — TODO (easy → hard)

Single source of truth for LSP work. Ordered by effort/dependency: do in order,
one `L-*` at a time. Verify each with `cargo test -p hella-lsp`.

Current baseline (shipped): hover, goto-definition, completion (locals/symbols/
keywords, members after `.`, import paths, snippets), document symbols,
diagnostics (lex/parse/sema + imports + `main`-file gate), incremental sync,
UTF-16 positions, startup progress.

- [x] **L-1 Formatting — DONE: `textDocument/formatting` + `rangeFormatting` via `hella-fmt`** (`formatting.rs`, 5 tests, 43 total pass)
- [ ] **L-2 Signature help — `textDocument/signatureHelp`** — derive from `Symbol.params` + call-paren depth at offset (dummy-ident tolerant), active parameter from comma count, own/ref/out markers in labels. Advertise `signatureHelpProvider` with `(`/`,`/`)` triggers. Tests: free fn, method, extern-C, mid-typing `foo(a, |`.
- [ ] **L-3 References — `textDocument/references`** — identifier-at-offset → all `Location`s in open doc (symbol spans + word-lex fallback for unparseable buffers). Include-declaration flag. Tests: local, param, field, fallback path.
- [ ] **L-4 Rename — `textDocument/rename` + `prepareRename`** — reuse L-3 locations, single-file `WorkspaceEdit`, reject on keywords/empty with `InvalidParams`. Tests: rename local, reject keyword.
- [ ] **L-5 Workspace symbols — `workspace/symbol`** — query filter over cached analyses (top-level + members), fuzzy case-insensitive match. Tests: find fn/struct across two open docs.
- [ ] **L-6 Document highlights — `textDocument/documentHighlight`** — same-word occurrences in doc via L-3 core. Tests: read/write kinds on locals.
- [ ] **L-7 Type definition + implementation — `textDocument/typeDefinition`, `implementation`** — symbol `ty` → type decl location; trait method → implementor locations. Tests: var → struct, trait → impls.
- [ ] **L-8 Code actions — `textDocument/codeAction` quickfixes** — kind `quickfix`: remove unused import (diagnostic-driven), `delete` for leaked bare `new` (Own-P1 rule), add missing `end` for unclosed block. Tests: one per fix, titles exact.
- [ ] **L-9 Inlay hints — `textDocument/inlayHint`** — param-name hints at call sites (`param:`), inferred `let`-type hints for untyped decls, range-bounded. Tests: named-arg hint, inferred type hint.
- [ ] **L-10 Folding + selection ranges — `textDocument/foldingRange`, `selectionRange`** — fold `do…end`/`has…end` blocks + imports header; selection expands ident → stmt → block → decl. Tests: nested blocks fold, selection chain length.
- [ ] **L-11 Workspace index — multi-file, import-aware** — background scan of workspace `.hll` (cap ~500 files), imported-file symbol cache feeding completion/hover/definition, `didSave` re-index, file-watch invalidations. Tests: cross-file goto-def, import completion without open doc.
- [ ] **L-12 Robustness — debounce, cancel, config** — 150ms debounce on `didChange` analysis + diagnostics, per-request cancellation (`$/cancelRequest`), `initializationOptions` (`maxFiles`, `diagnosticsOnType`), no-panic guarantee (all `parse`/`sema` paths return, never `unwrap` on client input). Tests: rapid-change burst, cancel mid-index, bad config ignored.

**How to use:** mark `in_progress` one at a time, implement `analysis.rs` logic
first with unit tests, then `server.rs` wiring + capability, run
`cargo test -p hella-lsp`, then mark `completed`.
