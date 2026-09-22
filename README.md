<div align="center" style="">
    <img src="assets/main-1.png" style="height: auto; width: 30vw; object-fit: cover"/>
</div>

Hella is a small, statically-typed programming language with a compiler written in Rust that produces native binaries via LLVM.

```hll
import std::io

void main() do
    println("Hello, Hella!")
end
```

No curly braces, no `let`; blocks are `do … end` and declarations are type-first (`int x = 1`).

## Quick start

**Prerequisites:** Rust (edition 2024), LLVM 21 (`llvm-config --version` should print `21.x`), and `clang` for linking (Xcode Command Line Tools on macOS).

```sh
cargo build                         # build the compiler
cargo run -p hella -- setup         # install the standard library to ~/.hella/lib

# Start your own project, or try an example:
cargo run -p hella -- new hello     # scaffold hello/ with src/main.hll
cargo run -p hella -- run -f examples/hello_io.hll
```

## Commands

| Command | What it does |
|---------|--------------|
| `hella build` | Compile the current project (`hella.toml` + `src/main.hll`) |
| `hella build -f <file>` | Compile a single `.hll` file to a native binary (same name, extension stripped) |
| `hella run` | Build (only if sources changed) and run the current project |
| `hella run -f <file>` | Build (only if sources changed) and run a single file |
| `hella check` | Type-check the current project |
| `hella check -f <file>` | Type-check a single file without generating code |
| `hella lint [-f <file>]` | Type-check, then report advisory reference warnings in the entry file (`--deny-warnings` for CI); see [linter coverage](references/linter.md) |
| `hella new <name>` | Scaffold a project (`--lib` for a library instead of a binary) |
| `hella fmt [paths]` | Format Hella sources in place (`--check` to verify only) |
| `hella setup` | Install the embedded standard library to `~/.hella/lib` (`--force` to overwrite) |
| `hella lsp` | Run the language server (LSP over stdio) |
| `hella add <src>[@rev]` | Add a library dep from a git URL, `owner/repo`, or local path (`--name` to rename; pins exact SHA in `hella.lock`) |
| `hella remove <name>` | Drop a library dep and collect orphaned cache slots |
| `hella fetch` | Download all locked dependencies (CI-friendly) |
| `hella update [names]` | Bump deps to the newest matching revisions (default: all) |
| `hella list` | Print the dependency tree |
| `hella clean [--cache]` | Prune orphaned slots (or empty the whole `pkg/` cache) |
| `hella install <src>[@rev]` | Release-build a tool into `~/.hella/bin` (`--bin` to rename; never touches `hella.toml`) |
| `hella uninstall <tool>` | Remove a tool from `~/.hella/bin` |

Dependencies live in `hella.toml` (`[dependencies]`, short import names:
`mylib = { git = "github.com/owner/repo", version = "1.2.3" }`), with exact
SHAs pinned in `hella.lock` and sources cached under `~/.hella/pkg`
(never edit by hand; slots carry markers verified against the lock).
Version requests float within `^` (Cargo-style; `=1.2.3` pins one tag).
`build`/`run`/`check` auto-fetch missing deps; pass `--offline` to forbid
network access or `--frozen` to also forbid lockfile changes. Set
`HELLA_HOME` to relocate `~/.hella` (hermetic CI).

`hella build` / `hella run` (without `-f`) only work inside a project directory (marked by `hella.toml`, then `src/main.hll` or `src/lib.hll`). Use `-f`/`--file` for a single file outside a project.

Short aliases work too: `b`, `r`, `c`, `ls`. Add `--verbose` for per-phase output or `--quiet` for errors only. Any command accepts `--help` (e.g. `hella build --help`).

## Hella at a glance

```hll
struct User has
    string name
    int age = 30          // fields can have defaults
end

int fib(int n) do         // type-first declarations, no `let`
    if n < 2 do
        return n
    end
    return fib(n - 1) + fib(n - 2)
end

void main() do
    User u = has name = "Ada" end   // `User` is inferred from the declaration
    match u.age do
        30 -> println("default age")
        _  -> println("custom age")
    end
end
```

A few things that make Hella Hella:

- **Blocks are `do … end`**, and struct/class/trait/enum bodies are `has … end`.
- **Statements end with a newline or `;`.**
- **`match … do` with `->` arms**, `_` wildcards, `|`/`or` alternatives, and tuple patterns.
- **Classes** with `this`, `initialize` constructors, `open`/`override`/`sealed`, traits + `implements`, and `get`/`set` properties.
- **Generics with `where` bounds**, `distinct`/`typedef` types, `extend` blocks, closures, string interpolation (`"hi {name}"`), and `extern "c"` for calling C.
- **`defer`** runs when its scope exits, on every path.
- **`@cfg(...)`** compiles items conditionally: `@cfg(os = "macos")`,
  `@cfg(target = "aarch64-apple-darwin")`, `@cfg(debug)` / `@cfg(debug = false)`,
  joined with `or` (or `,`); stacking attributes means AND. The condition is
  resolved during import expansion, so absent items never reach sema or codegen.

## Examples

The `examples/` directory is the fastest way to learn. Build one and run the binary:

```sh
cargo run -p hella -- build -f examples/basics.hll && ./examples/basics; echo $?
```

| File | Shows you |
|------|-----------|
| `basics.hll` | Numbers, booleans, arithmetic, `if`/`else`, `while`, functions, recursion |
| `data_control.hll` | Structs, field defaults, arrays, `match`, strings, `loop`/`for`, `defer` |
| `abstraction.hll` | Classes, constructors, traits, enums, properties |
| `hello_io.hll` | `import std::io` and printing (`print`, `println`, interpolation) |
| `cli_args.hll` | Command-line args (`string[] args`, `args.len()`, iteration) |
| `stdlib_io.hll` | Standard-library I/O over `extern "c"` declarations |
| `advanced.hll` | Generics, closures, interpolation, operators, `extern`, `distinct` |
| `variadic.hll` | Variadic functions (`...`) |
| `default_args.hll` | Default parameter values and `named:` arguments |
| `own_heap.hll` | Heap ownership (`own`, `new`, `delete`, moves, trait upcasts) |
| `trait_objects.hll` | Trait objects and dynamic dispatch |
| `async_basic.hll` | Structured concurrency: `async` functions, `task<T>`, `spawn`, `await`, `scope do ... end`, `yield` |
| `sync_chan.hll` | Mutex + bounded channels: producer/consumer across tasks (`std::sync`, `std::chan`) |
| `net_echo.hll` | Blocking TCP echo server + client (`std::net`, `std::time`) |
| `net_poll.hll` | Single-threaded poll-loop echo server: non-blocking fds + `tcpPoll` readiness (B3 event loop) |
| `systems_ptr.hll` | Raw pointers: `&x` address-of, `*p` load/store (`--target` selects the LLVM triple) |
| `cfg_platform.hll` | Conditional compilation: `@cfg(os = "...")`, `@cfg(debug)` |
| `types_ints.hll` / `types_arr.hll` / `types_vec.hll` / `types_map.hll` / `types_methods.hll` | Integer widths, fixed arrays (incl. slicing), vectors, maps, methods |
| `stdlib_*.hll` (`string`, `num`, `math`, `env`, `fs`, `path`, `collections`, `fmt`, `io`, `rand`, `log`, `encoding`, `hash`, `url`, `terminal`, `time`) | Standard-library tours: strings, parsing/conversion, math, environment, files, paths, vectors/maps, text composition, terminal IO, random generation, logging, hex/base64, hashing, URLs, terminal detection, clocks (see `stdlib/README.md` for the module index) |

The exit code of each example is its answer; `basics` exits with `230`, `abstraction` with `233`, and so on.

## Project layout

```
hella/
├── crates/
│   ├── hella-cli/       # the `hella` binary (commands, progress output)
│   ├── hella-compiler/  # the compiler library (lexer → parser → sema → codegen)
│   └── hella-lsp/       # the language server
├── examples/            # .hll example programs
├── stdlib/              # standard library, written in Hella itself
├── runtime/             # C runtime sources (Windows shim + async task runtime)
└── references/          # language spec and design notes
```

The pipeline is: lex → parse → resolve imports → type-check → LLVM IR → object file → `clang` links a native binary. Run `cargo test` to execute the test suite.

## Learn more

The grammar in `references/ebnf-0.1.txt` is authoritative. Also see `references/phases.md` (how the language was built, phase by phase) and `references/llvm-mapping.md` (how Hella types lower to LLVM). `SKILL.md` has detailed guidance for working on the compiler itself.

## Status

The core language (phases 0-5) compiles to native code: structs, classes, traits, enums, pattern matching, generics, closures, string interpolation, and C interop all work end to end. Structured concurrency is also in: `async` functions run as thread-backed tasks with `task<T>` handles, `await`/`spawn`/`scope do ... end` enforce single-await and scope-bound joins at compile time, and the async runtime (`runtime/hella_async.c`) is compiled and linked only when a program actually reaches async code — a synchronous binary gains no scheduler symbols. `import std::task` adds `yieldNow()`, `sleepMs()`, and `cancelled()`. Not yet async: I/O (a blocking libc call inside an `async` body still blocks that task's thread), worker-thread scheduling, channels/select. The gap list in `SKILL.md` tracks what remains.

## License

Apache-2.0 ... see [`LICENSE`](/LICENSE).
