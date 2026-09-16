---
name: hella-pkg
description: Package manager for the Hella programming language — git-URL dependencies with short import names, project-scoped resolution, tool installs. Use when the user asks about hella add/remove/install/uninstall, hella.toml dependencies, hella.lock, module caching, or third-party libraries.
---

# Hella Package Manager

Git-URLs-only (no central registry yet). Libraries declare their own short
import name (decision 4a): code writes `import mylib`, the manifest maps
`mylib -> git URL + version`. The shared on-disk cache is world-readable;
*visibility* is enforced by the resolver allow-list, not by filesystem
permissions.

## Layout

```
~/.hella/
  lib/                          # stdlib only, owned by `hella setup` — never mix third-party here
  pkg/<host>/<owner>/<repo>/<version>/   # immutable fetched sources (read-only)
  bin/<tool>                    # `hella install` targets (must be on $PATH)
  cache/                        # clone tarballs / scratch
```

Project files:

```
hella.toml                      # [package] name/version + [dependencies]
hella.lock                      # exact commit SHAs, generated (like Cargo.lock)
```

## Manifest contract

```toml
[package]
name = "myapp"
version = "0.1.0"

[dependencies]
mylib = { git = "github.com/repo/lib", version = "1.2.3" }
tool  = { git = "github.com/repo/tool", version = "0.4.0", package = "tool::cli" }
```

* Table key = short import name used in `import mylib` / `import mylib::sub`.
* `git` = host + path, no scheme (`github.com/...`), optional `https://` accepted.
* `version` = semver tag request (`1.2.3`, `^1.2`, `latest`); resolved to an
  exact tag/commit and pinned in `hella.lock`.
* Optional `package` = sub-path inside the repo when the Hella module root
  is not the repo root.
* Transitive deps come from each dep's own `hella.toml`; conflicts resolve
  newest-compatible-wins, incompatibilities are a loud `add`/`fetch` error,
  never a silent build break.

## Commands

| Command | Scope | Effect |
|---------|-------|--------|
| `hella add <url>[@rev]` | project | clone to `pkg/`, add to `hella.toml` + pin in `hella.lock` |
| `hella remove <name>` | project | drop from `hella.toml`, prune `hella.lock`, GC if orphan |
| `hella fetch` | project | ensure all locked deps are in `pkg/` (CI-friendly, like `go mod download`) |
| `hella update [name]` | project | bump req(s) to newest matching tag, re-pin lock |
| `hella list` | project | print dep tree |
| `hella install <url>[@rev]` | global | temp clone, `hella build --release`, copy binary to `~/.hella/bin/`, clean temp; does NOT touch `./hella.toml` |
| `hella uninstall <tool>` | global | remove `~/.hella/bin/<tool>` |
| `hella clean --cache` | global | GC unreferenced `pkg/` entries |

`add` without `@rev` resolves latest semver tag, else HEAD, and always
writes the resolved version into the lock. `build`/`run`/`check` auto-fetch
missing locked deps; `--offline` / `--frozen` never touch the network.

## Resolver rules (`crates/hella-compiler/src/modules.rs`)

Search order per entry file:

1. entry dir (modules next to the entry win — never hijacked by a dep)
2. project root (`hella.toml` dir)
3. dep cache dirs for names allow-listed in `hella.toml` + `hella.lock`
   (transitive closure only)
4. stdlib roots (dev-checkout `stdlib/`, then `~/.hella/lib`)

A cached dep that is not in the importing project's closure is invisible,
even though it sits on disk. Cycles cut by visited-file set (existing).
Selective imports (`import mylib::{foo}`) still carry the dep's `extern`
blocks (existing rule); system-link requirements of third-party `extern "c"`
are the project's responsibility.

## Implementation notes

* Manifest/lock parsing lives in `hella-compiler` (`src/manifest.rs`) so CLI
  and LSP share it — never re-parse TOML by hand in the CLI.
* `toml` + `semver` crates for manifest/versions; `git` via the `git`
  CLI (shallow `--depth 1 --branch <tag>`), no libgit2 dependency for v1.
* `pkg/<...>/<version>/` dirs are immutable: chmod read-only after fetch,
  re-fetch on hash mismatch, never mutate in place.
* Shallow clones can't resolve arbitrary SHAs — full clone fallback only
  when `@<sha>` is requested.
* Private repos reuse the user's existing git auth (SSH agent / credential
  helper); no token flags in v1.
* Windows: same layout under `%USERPROFILE%\.hella`, binaries get `.exe`.

## Workflow

1. Pick the next `P-*` in `.opencode/skills/pkgmanager/TODO.md` (source of truth).
2. Extend `manifest.rs` (pure logic, unit-tested) first, then `modules.rs`
   bases, then CLI subcommands in `crates/hella-cli/src/main.rs`.
3. Mirror dep-aware bases in `hella-lsp` (`analysis.rs`, `auto_import.rs`)
   so IDE diagnostics match `build`.
4. Verify: `cargo test --workspace` + a scratch project exercising
   `add` → `import` → `build` → `remove` before marking any `P-*` done.
5. Commit each `P-*` separately (`feat(pkg): ...`).

## Anti-patterns

* No third-party sources under `~/.hella/lib` (stdlib-owned).
* No network access during `build` when `--offline`/`--frozen` is set.
* No `install` that edits the current project's `hella.toml`.
* No silent version drift: every fetch pins exact SHA in `hella.lock`.
* No `{}` blocks or `let` in Hella sources; stdlib rules still apply.
