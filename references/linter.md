# Linter

`hella lint` runs the normal dependency resolution and semantic checks, then
analyzes the entry source file for advisory warnings. `-f FILE` selects a file;
without it the project's entry is used. It does not compile or run the program.

```sh
hella lint
hella lint -f src/main.hll --deny-warnings
```

Warnings go to stderr with a file, one-based line/column and stable code. They
exit successfully by default; `--deny-warnings` makes warnings fail the command.
Lexing, parsing, import and semantic errors still fail regardless of that flag.
`--quiet` suppresses progress, not warnings. The usual `--offline` and `--frozen`
check options are available. Dependencies are type-checked but not linted: use
`-f` on another source to lint it explicitly.

The LSP reports the same warnings automatically for the open document, with
warning severity, a diagnostic code, and UTF-16 positions. Imported source spans
are never used for lint warning positions in the open document.

## H001: aliased explicit mutable arguments

Passing the same variable to more than one explicit `ref`/`out` argument may make
writes through one parameter unexpectedly affect the other:

```hll
void update(ref int a, out int b) do
    a = 1
    b = 2
end

void main() do
    int x = 0
    update(ref x, out x) // H001 on out x
end
```

The warning does not prohibit deliberate aliasing. Use separate variables if the
callee expects independent storage. Every repeat after the first is reported;
parenthesized variable references count as the same variable. Separate calls do
not share argument history.

The traversal covers nested expressions and control flow, closures, class
methods/constructors/destructors, property bodies, extensions and defaults.

## Limits

This is an initial advisory linter, **not a borrow checker or complete ownership
analysis**. H001 only compares explicit bare-variable arguments in one call. It
cannot discover aliasing through different variable names, pointers, fields or
indices, or implicit reference passing. Typed `out` declarations are treated
conservatively as binding boundaries; the declaration itself is not warned on.

There are no additional object-lifetime, escaping-reference, unused-owner or
resource-result lint rules yet. Existing semantic errors for invalid ownership
operations remain the compiler's responsibility. No H002 rule is shipped.

## Tests

```sh
cargo test -p hella-compiler --lib lint::tests
cargo test -p hella --test lint
cargo test -p hella-lsp diagnostics::tests::lint_
```

CLI integration tests execute the built CLI, checking clean/warning/deny/error
exit statuses and entry-file isolation. LSP regressions assert severity, code,
UTF-16 ranges and that imported warnings do not appear in the importing file.
