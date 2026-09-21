# Hella Standard Library

Pure Hella sources. See `.opencode/skills/stdlib/SKILL.md` for import semantics (EBNF §32),
`.opencode/skills/stdlib/REAL_STDLIB.md` for the bare-minimum compiler contract, and roadmap.

The compiler knows no user-facing IO names. Every symbol below is an ordinary Hella
function defined in `stdlib/`, using Hella algorithms and `extern "c"` where needed. Calling one
without its `import` is a sema error (`undefined function`) by design.

## Modules

- `std::io` — `stdlib/std/io.hll`
  - `void print(string s)` — no newline (`printf("%s", s)`)
  - `void println(string s)` — with newline (`puts(s)`)
  - `void eprint(string s)` / `void eprintln(string s)` — stderr, without/with
    newline (`write(2, …)`; no `FILE*` global needed)
  - `string readLine()` — dynamically growing line reader, strips LF, preserves
    blank lines; returns the partial line on EOF/error, or `""` if no bytes read.
    No EOF/error distinction, CR stripping, or embedded-NUL support. Uses bounded
    one-byte `scanf("%1c")` reads, doubling buffers; intermediate buffers are freed.
- `std::math` — `stdlib/std/math.hll`
  - `double sqrt(double x)`, `sin`/`cos`/`tan`, `pow`, `floor`/`ceil`/`round`,
    `log`/`exp`, `fabs`, `fmin`/`fmax`, `atan2` via `libm`
  - `int abs(int x)`, `min`/`max`, `clamp`, `absf`/`minf`/`maxf` wrappers
- `std::str` — `stdlib/std/str.hll`
  - `int len(string s)`, `bool isEmpty`, `bool equals`/`int compare`,
    `bool contains`/`startsWith`/`endsWith`, `string clone`/`substring`
  - `indexOf(hay, needle)`, `indexOfFrom(hay, needle, start)`, `lastIndexOf`:
    byte offsets, `-1` on miss. Empty needle matches start/end. Negative search
    start clamps to 0; start beyond length returns -1, even for empty needle.
  - `trim(s)` / `isAsciiSpace(c)` — ASCII space, tab, LF, CR, VT, FF only.
  - `splitNext(s, sep, ref cursor)` — initialize cursor to 0; call while it is
    nonnegative. Returns one field, updates cursor, sets -1 after the last field.
    Preserves leading/adjacent/trailing empty fields. Empty separator returns s
    once. This streaming API avoids Hella's current 16-element vector limit.
  - `replaceAll(s, needle, replacement)` — left-to-right non-overlapping matches;
    empty needle is a no-op (unlike Go), replacement text is never searched.
  - `allocateString(size)` — low-level zero-filled, NUL-terminated allocation;
    asserts on invalid size or allocation failure. Used by composition helpers.
- `std::num` — `stdlib/std/num.hll`
  - `bool parseInt(string s, ref int value)` — strict whole-string signed decimal
    parsing for Hella's current 64-bit int. Optional leading `+`/`-`, leading zeros
    allowed; rejects whitespace, empty/sign-only input, prefixes, separators,
    trailing junk and overflow. Returns false **and resets value to 0** on failure.
    Checks before multiply/subtract, including -9223372036854775808; no libc parser.
- `std::env` — `stdlib/std/env.hll`
  - `string getEnv(string name)` (getenv + "" on miss, `string is null` now allowed), `bool hasEnv`, `void setEnv`/`unsetEnv` (setenv/unsetenv), `string cwd()` (getcwd + calloc)
- `std::fs` — `stdlib/std/fs.hll`
  - `bool exists(string path)` (access), `string readFile`/`void writeFile`/`appendFile`/`removeFile` (fopen/fseek/ftell/fread/fwrite/remove)
- `std::vector` — `stdlib/std/vector.hll` (pure Hella; named `vector` because
  `vec` is a compiler keyword and cannot be an import segment)
  - int vec: `iLen`/`iIsEmpty`/`iContains`/`iFirst`/`iLast`, `iPush`/`iPop`
    (`ref`), `iSum`/`iIndexOf` (loops)
  - string vec: `sLen`/`sIsEmpty`/`sFirst`/`sLast`, `sPush`/`sPop` (`ref`),
    `sContains`/`sJoin` (byte comparison/exact-size copying — the
    `contains` method hangs on ptr-element vecs)
  - double vec: `dLen`, `dPush` (`ref`) only (`contains` miscompiles f64,
    `+` has no double overload)
  - no `*Clear`: `clear()` fails LLVM verification even on locals
- `std::map` — `stdlib/std/map.hll` (pure Hella, read-only)
  - `si*` (string:int), `ii*` (int:int), `ss*` (string:string):
    `Len`/`IsEmpty` (methods) + `Has`/`GetOr` (`for`-in loops — keyed
    methods and `m[k]` fail verification through by-value params)
  - mutate with direct local calls (`m.remove(k)`); `ref`-param method
    calls do not codegen
- `std::fmt` — `stdlib/std/fmt.hll`
  - `join(sep, parts)` (via `sJoin`), `repeat(s, n)`, `padStart(s, width)`,
    `padEnd(s, width)` — checked-size allocations and pure Hella copying loops,
    no format strings or interpolation buffers. Widths are bytes; padding uses
    spaces and never truncates. Nonpositive repeat counts yield `""`.
    Output-size overflow and allocation failure assert rather than wrap/truncate.
- `std::types` — `stdlib/std/types.hll` (doc-only manifest of the implicit
  environment: `bool string i8…u128 int uint float double`; importing is a no-op)
- `std::rand` — `stdlib/std/rand.hll` (Async-10 sibling: uses `@cfg` for the
  platform split)
  - Deterministic xoshiro256** generator, seeded via standard splitmix64
    (`seed(42)` produces the same stream as any splitmix64 implementation —
    the example is a golden test). `seed`/`seedRandom` (OS entropy),
    `next` (raw u64), `intRange`/`intBelow` (rejection sampling, no modulo
    bias), `bitsBelow`, `chance`, `flip`.
  - Cross-platform entropy via `@cfg`: POSIX reads 8 bytes from
    `/dev/urandom` (`fopen`/`fgetc`, plain libc, no extra link flags);
    Windows uses UCRT `rand_s` declared with `ref` out-params. One `@cfg`
    per platform block; the other is absent from the program entirely.
  - NOT cryptographically secure (documented in the module header); the
    generator state is module-global and not thread-safe.
  - Requires the unsigned `>>` fix: `u64 >> n` lowers to a logical shift
    (`u64Const` builds 64-bit constants from `int`-fitting halves, since
    Hella literals are `int`-bounded).
- `std::task` — `stdlib/std/task.hll` (named `task` because `async` is a
  compiler keyword and cannot be an import segment, same as `vector` vs `vec`;
  Async-10)
  - `void yieldNow()` — cooperative checkpoint (`sched_yield`); offers the
    OS scheduler a chance to run other ready tasks. Only referenced by
    programs that reach async code (the runtime is otherwise not linked).
  - `void sleepMs(int ms)` — park the calling task for `ms` milliseconds
    (clamped at 0): POSIX `nanosleep` retried across `EINTR`, Windows `Sleep`.
    Wall-clock parking, not a cooperative wait.
  - `bool cancelled()` — true when the running task was asked to stop
    (cooperative cancellation; tasks poll at checkpoints, nothing preempts).
    Always false outside a task. There is no API yet to request cancellation
    from Hella code (`hella_task_cancel` exists in the runtime but is not
    exposed until scope cancellation semantics settle).
  - Honest scope: these are cooperative primitives. `sleepMs` parks one
    task; nothing here turns blocking libc I/O into non-blocking I/O.

Import examples:

```
import std::io
import std::io::{print, println}
```

Selective imports keep extern blocks, but currently **drop helper dependencies**.
Use whole-module imports for the higher-level APIs above (`import std::str`, not
`import std::str::{trim}`). Simple IO selective imports remain usable.

## Migration from typed formatting / IO helpers

Removed `formatS`, `formatSS`, `formatD`, `formatDD`, `formatF`, `formatDS`.
Their unbounded `sprintf` calls could overflow 4KiB slabs; the old claim that libc
truncated them was incorrect. Use language interpolation, not C percent verbs:

```hll
import std::io
import std::num

void main() do
    string name = "Ada"
    int n = 42
    println("{name}: {n}")  // replaces formatDS / printInt
    print("!")             // replaces putChar for literal text
    int value = 0
    string input = readLine()
    if parseInt(input, ref value) do
        println("{value}")
    end else do
        eprintln("invalid integer")
    end
end
```

`printInt`, `putChar`, and `readInt` are removed. Use `print`/`println` and
`parseInt(readLine(), ref value)` (or explicit temporaries as above). Interpolation
has no supported precision specifier equivalent to `%.1f`; none is invented here.

**Compiler safety limitation:** interpolation still uses an unchecked 512-byte
stack buffer plus a 64-byte `%f` temporary (large doubles can overflow that too).
Its integer formatting uses `%ld`, which is not 64-bit on Windows. Do not interpolate
unbounded text or large doubles until codegen is fixed. The new composition APIs
avoid that path and are tested with 10–20KB text. This is not a claim that arbitrary
Hella interpolation is now safe.

## Representation, ownership and other limits

- Strings are non-null NUL-terminated byte pointers, not Unicode scalar sequences
  or binary buffers. Search/substrings can split UTF-8 code points; trim is ASCII.
- Raw strings have **no automatic reclamation**. `clone`, non-reversed `substring`,
  `trim`, valid `splitNext` fields, `join`/`sJoin`, `readLine`, and allocations from
  `allocateString` produce heap buffers. `repeat` with positive count and nonempty
  input allocates; empty cases return literals. Padding that needs no work and
  replacement with no matches/empty needle return the input pointer. Other empty
  cases can return literals. Do not blindly `free` every result or mutate literals.
  Free only buffers known to be newly allocated (via extern `free`) after all aliases
  are dead. Examples favor clarity and do not reclaim every temporary; long-lived
  applications need explicit ownership discipline. No safe generic string owner
  or builder is claimed in this change.
- New size guards assume the compiler's current 64-bit int ABI. Allocation failures
  are fatal assertions, not recoverable results. Naive substring search is O(h*n);
  replacement scans twice; composition copies into one final buffer rather than
  repeatedly leaking growing interpolation intermediates.
- Existing vector/map operations retain compiler limitations documented above,
  including fixed-capacity storage; split does not collect into a vector.
- `math.abs` does not handle minimum int safely. FS/env wrappers still conflate
  failures with empty results, ignore some status codes, and `cwd` has a 1024-byte
  buffer. These are unchanged, not audited safe replacements.
- Extern ABI lowering is incomplete: `i32` return declarations can become `void`.
  `readLine` uses the supported Hella `int scanf` declaration, which codegen maps to
  C i32 and sign-extends. Runtime Windows shims and the other existing FFI wrappers
  still need cross-platform ABI review. Verified locally on macOS, not Windows.

## Verification

```sh
cargo test -p hella-compiler --test stdlib
cargo test --workspace
cargo build -p hella
./target/debug/hella build --force -f examples/stdlib_fmt.hll
./examples/stdlib_fmt
./target/debug/hella build --force -f examples/stdlib_rand.hll
./examples/stdlib_rand   # golden sequence; must pass on every platform
printf 'Ada\n42\n' | ./target/debug/hella run --force -f examples/stdlib_io.hll
```

`crates/hella-compiler/tests/stdlib.rs` uses the public lexer/parser/import/sema/
object APIs and a real C linker. It builds/runs all eight stdlib examples in debug
and release, checks exact output, exercises long/blank/EOF input, and checks fatal
size-overflow guards. It uses checkout imports and isolated scratch files, no
installed stdlib or prebuilt CLI; binaries have a ten-second timeout. Requires LLVM
and `clang` (or `HELLA_LINKER`).

## Public API references consulted

- [Go strings](https://pkg.go.dev/strings): byte offsets, empty fields and
  non-overlapping replacement. Hella deliberately uses ASCII trim and an empty
  separator/needle no-op policy rather than Go's UTF-8 empty-pattern behavior.
- [Odin core:strings](https://pkg.odin-lang.org/core/strings/): split iterators,
  ASCII whitespace, explicit allocation/ownership concerns.
- [Rust i64 parsing](https://doc.rust-lang.org/std/primitive.i64.html#method.from_str_radix):
  strict signed parsing and explicit invalid/overflow failure. Hella uses bool +
  ref output rather than claiming generic Result support.
