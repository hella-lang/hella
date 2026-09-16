# Hella Package Manager — TODO (easy → hard)

Single source of truth for package-manager work. Git-URLs-only, no registry.
Short import names (decision 4a): manifest key = name used in `import`.
Do in order, one `P-*` at a time. Commit each `P-*` separately.

- [x] **P-1 Manifest + lockfile model — DONE 2026-09-16** (`crates/hella-compiler/src/manifest.rs`: TOML `[package]` + `[dependencies]` `{ git, version, package? }`, short-name validation, `hella.lock` round-trip, legacy bare-form compat; CLI `read_manifest` delegates; 7 unit tests + scratch `check` smoke).
- [x] **P-2 Storage layout + dep-scoped resolver — DONE 2026-09-16** (`modules.rs`: `hella_home/pkg/bin/cache` helpers, `pkg/<git>/<version>/` slots + `package` sub-path roots, `dep_bases_for_project` allow-list from manifest+lock with lock-wins + request fallback, order `entry > project > deps > stdlib`, `lib/` stays stdlib-only; 5 resolver tests, workspace green).
- [x] **P-3 `add` / `remove` — DONE 2026-09-16** (`crates/hella-cli/src/pkg.rs`: spec parse incl. `owner/repo` + `file://`/local paths, `ls-remote` tag resolve (exact/semver-range/`latest`-stable/branch/SHA), shallow tag clone + full fallback, immutable slots with `.hella-slot` markers, first-wins closure with loud name/version conflicts, surgical `hella.toml` edits preserving comments, lock prune + orphan GC with empty-parent pruning; 7 CLI tests on local git fixtures, e2e `add`→`import`→`build`→`run`→`remove` verified).
- [x] **P-4 Build integration — DONE 2026-09-16** (`pkg::ensure_deps` fixpoint: fast local-only path, auto-resolve + lock write when online, `--offline`/`--frozen` errors; hooked into `build`/`run`/`check` in project mode; pins record exact SHAs; freshness already covered via `expand_imports` file set; 4 `ensure_*` tests + e2e flag matrix verified).
- [x] **P-5 `install` / `uninstall` — DONE 2026-09-16** (`pkg::prepare_tool` stages temp clone + locates `src/main.hll`/`main.hll` + names from `--bin`/manifest/derived; `run_install` resolves tool deps, release-builds via the shared pipeline into `~/.hella/bin/`, cleans staging, warns when `bin/` not on `$PATH`; `run_uninstall` removes binary + `.hellastamp` sidecar; 4 tests + e2e install→run→uninstall verified).
- [x] **P-6 `fetch` / `update` / `list` / `clean` + LSP — DONE 2026-09-16** (CLI: `fetch` ensure+count, `update [names]` (empty = full re-resolve) with prune+GC+per-dep report, `list` cycle-safe tree, `clean` project-orphans vs `--cache` nuke of `pkg/`+staging; semver reqs float within `^` (Cargo-style, `=x.y.z` for exact); `HELLA_HOME` override for hermetic CI/tests; LSP needed zero changes (shares `search_bases`/`expand_imports`) + 1 diagnostics regression test proving locked-dep imports resolve; e2e matrix verified).
- [x] **P-7 Docs + polish — DONE 2026-09-16** (README command table + manifest/lock/`^`-float/`--offline`/`--frozen`/`HELLA_HOME` notes, `hella new` template hints at `add`, skill corrected to as-built: no chmod, surgical edits, semver float; final full-suite + e2e green).

**How to use:** mark `in_progress` one at a time, implement per SKILL.md,
verify with `cargo test` (+ scratch-project `add`/`build`/`remove` from P-3
on), then mark `completed` and commit (`feat(pkg): ...`).
