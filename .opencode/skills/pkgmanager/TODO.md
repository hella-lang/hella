# Hella Package Manager — TODO (easy → hard)

Single source of truth for package-manager work. Git-URLs-only, no registry.
Short import names (decision 4a): manifest key = name used in `import`.
Do in order, one `P-*` at a time. Commit each `P-*` separately.

- [x] **P-1 Manifest + lockfile model — DONE 2026-09-16** (`crates/hella-compiler/src/manifest.rs`: TOML `[package]` + `[dependencies]` `{ git, version, package? }`, short-name validation, `hella.lock` round-trip, legacy bare-form compat; CLI `read_manifest` delegates; 7 unit tests + scratch `check` smoke).
- [ ] **P-2 Storage layout + dep-scoped resolver** — `hella_home()/pkg_dir()/bin_dir()` helpers, `pkg/<host>/<owner>/<repo>/<version>/` paths, `dep_search_bases(entry)` allow-list from manifest+lock closure, order `entry > project > deps > stdlib`, keep `~/.hella/lib` stdlib-only. Resolver unit tests with fixture dirs.
- [ ] **P-3 `add` / `remove`** — `hella add <url>[@rev]`: parse URL+rev, `git clone --depth 1`, resolve latest semver tag (else HEAD), copy to immutable `pkg/` slot, edit `hella.toml`, pin `hella.lock` (incl. transitive closure from dep manifests, newest-wins conflict error). `hella remove <name>`: drop + prune lock + orphan GC. Tests with local `git` fixtures (no network).
- [ ] **P-4 Build integration** — `build`/`run`/`check` auto-fetch missing locked deps, `--offline`/`--frozen` flags (no network, error if incomplete), hash re-verify on use, freshness includes dep file set. Integration test: scratch project `add` → `import` → `build`.
- [ ] **P-5 `install` / `uninstall`** — `hella install <url>[@rev]`: temp clone, release build, copy to `~/.hella/bin/` (+`.exe` on Windows), clean temp, never touch `./hella.toml`; PATH warning when `bin/` not on `$PATH`. `hella uninstall <tool>`: rm binary. Test with tiny fixture tool.
- [ ] **P-6 `fetch` / `update` / `list` / `clean` + LSP** — `fetch` (CI download), `update [name]`, `list` (dep tree), `clean --cache` (GC unreferenced). LSP: dep-aware bases in `analysis.rs` + `auto_import.rs` so completion/diagnostics match `build`. Tests: `cargo test -p hella-lsp`.
- [ ] **P-7 Docs + polish** — README command table, `references/` notes, `--help` text, `hella new` template with `[dependencies]` comment, error-message pass. Full `cargo test --workspace` + end-to-end scratch run.

**How to use:** mark `in_progress` one at a time, implement per SKILL.md,
verify with `cargo test` (+ scratch-project `add`/`build`/`remove` from P-3
on), then mark `completed` and commit (`feat(pkg): ...`).
