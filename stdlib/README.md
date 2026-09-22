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
  - `int abs(int x)`, `min`/`max`, `clamp`, `absf`/`minf`/`maxf` wrappers,
    `double clampf(v, lo, hi)` (via `fmin`/`fmax`)
  - Double equality uses `is` / `is not` (ordered `OEQ` / unordered `UNE`:
    NaN is never equal, even to itself). Ordering (`<`, `<=`, `>`, `>=`)
    and arithmetic on doubles remain sema-rejected by design.
- `std::str` — `stdlib/std/str.hll`
  - `int len(string s)`, `bool isEmpty`, `bool equals`/`int compare`,
    `bool contains`/`startsWith`/`endsWith`, `string clone`/`substring`
  - `indexOf(hay, needle)`, `indexOfFrom(hay, needle, start)`, `lastIndexOf`:
    byte offsets, `-1` on miss. Empty needle matches start/end. Negative search
    start clamps to 0; start beyond length returns -1, even for empty needle.
  - `count(s, needle)` — non-overlapping occurrences (byte-based); empty
    needle yields `len(s) + 1` (Go `strings.Count` parity).
  - `trim(s)` / `trimStart` / `trimEnd` / `isAsciiSpace(c)` — ASCII space,
    tab, LF, CR, VT, FF only.
  - `trimPrefix(s, prefix)` / `trimSuffix(s, suffix)` — strip one affix,
    or return `s` unchanged (no allocation) when absent. Empty affixes
    always "match" (prefix strips nothing, suffix strips nothing).
  - `toUpper(s)` / `toLower(s)` — ASCII letters only; other bytes pass through.
  - `equalsIgnoreCase(a, b)` — ASCII case-insensitive equality; non-ASCII
    bytes compare exactly (no Unicode folding). Allocates nothing.
  - `isDigit(c)` / `isAlpha(c)` / `isAlphaNum(c)` — ASCII classification.
  - `splitNext(s, sep, ref cursor)` — initialize cursor to 0; call while it is
    nonnegative. Returns one field, updates cursor, sets -1 after the last field.
    Preserves leading/adjacent/trailing empty fields. Empty separator returns s
    once. This streaming API avoids collecting into fixed-capacity vectors
    (currently 256 elements; split stays streaming regardless).
  - `splitNextSpace(s, ref cursor)` — same streaming convention, but
    ASCII-whitespace runs collapse: no empty fields, leading/trailing runs
    produce nothing, and the last field arrives with cursor already -1.
    The whitespace-splitting counterpart to `splitNext`.
  - `replaceAll(s, needle, replacement)` — left-to-right non-overlapping matches;
    empty needle is a no-op (unlike Go), replacement text is never searched.
  - `replaceFirst(s, needle, replacement)` — first match only, same
    conventions (empty needle / absent needle return `s` unchanged).
  - `allocateString(size)` — low-level zero-filled, NUL-terminated allocation;
    asserts on invalid size or allocation failure. Used by composition helpers.
  - `concat(a, b)` — exact-size fresh allocation of `a` + `b` (no
    interpolation buffer); the safe builder for unbounded text.
- `std::num` — `stdlib/std/num.hll`
  - `bool parseInt(string s, ref int value)` — strict whole-string signed decimal
    parsing for Hella's current 64-bit int. Optional leading `+`/`-`, leading zeros
    allowed; rejects whitespace, empty/sign-only input, prefixes, separators,
    trailing junk and overflow. Returns false **and resets value to 0** on failure.
    Checks before multiply/subtract, including -9223372036854775808; no libc parser.
  - `bool parseDouble(string s, ref double value)` — strict whole-string
    decimal float via `sscanf` `%lf` + `%n` (optional sign/fraction/exponent;
    `inf`/`nan` convert like Go's `ParseFloat`). Rejects empty input,
    whitespace, hex floats (`0x…`, which Hella has no syntax for) and
    trailing junk. Returns false **and resets value to 0.0** unless the
    entire string converted. Overflow yields ±inf with true (Rust parity).
  - `bool parseBool(string s, ref bool value)` — exactly `"true"`/`"false"`
    (case-sensitive, no `1`/`0` shorthands). Returns false **and resets
    value to false** on any other input.
  - `string toString(int v)` — exact decimal text for every 64-bit int
    (bounded `%lld` `snprintf` into a 21-byte buffer; `%lld` is 64-bit on
    every platform, unlike interpolation's `%ld`).
  - `string doubleToString(double v)` — `%g` display text (6 significant
    digits) in a 32-byte buffer, always big enough. Display-grade only:
    shortest round-trip is not guaranteed.
- `std::env` — `stdlib/std/env.hll`
  - `string getEnv(string name)` (getenv + "" on miss, `string is null` now allowed), `bool hasEnv`, `void setEnv`/`unsetEnv` (setenv/unsetenv), `string cwd()` (getcwd + calloc)
  - `string homeDir()` — `HOME`, else `USERPROFILE` (Windows), else `""`.
    Pure lookup; no `~` expansion anywhere.
  - `string tempDir()` — `TMPDIR`, else `TEMP`, else `TMP`, else `""` (empty
    values skipped). No `/tmp` fallback — it does not exist on Windows, so
    callers handle `""` (e.g. fall back to `cwd()`). Join with
    `std::path::joinPath`; no trailing slash is guaranteed.
  - `string progName()` — `argv[0]` as the parent exec'd it (compiler-provided
    `__hella_progname`; no libc symbol involved despite the `from "libc"` header)
  - `void exitProcess(int code)` — immediate `exit(code)`; deferred cleanup
    does not run, so reserve it for fatal paths
- `std::fs` — `stdlib/std/fs.hll`
  - `bool exists(string path)` (access), `string readFile`/`void writeFile`/`appendFile`/`removeFile` (fopen/fseek/ftell/fread/fwrite/remove)
  - `int fileSize(string path)` — bytes via `fseek`/`ftell`, or `-1` when
    the file cannot be opened
  - `bool renameFile(oldPath, newPath)` — ISO C `rename`, portable across
    Windows/Linux/macOS. Overwrites an existing destination on POSIX;
    on Windows renaming onto an existing file fails.
  - `bool copyFile(src, dst)` — best-effort copy via `readFile`/`writeFile`,
    verified against the source size (empty files copy fine). Not atomic,
    permissions not preserved; concurrent writers can race verification.
- `std::path` — `stdlib/std/path.hll` (pure Hella over `std::str` + `std::vector`)
  - `joinPath(a, b)` (exactly one `/`, empty sides pass through),
    `basename(p)` (trailing slashes ignored; root/empty yield `""`),
    `dirname(p)` (no slash yields `"."`; root yields `"/"`),
    `extension(p)` (`"a.tar.gz"` → `"gz"`; dotfiles/dotless/trailing-dot yield `""`)
  - `isAbsolute(p)` — true for a leading `/` (POSIX semantics;
    `C:/x` is not absolute, `\` is an ordinary byte)
  - `clean(p)` — lexical normalization (Go `path.Clean` parity):
    collapses `//`, drops `.`, resolves `..` without escaping the root,
    strips trailing slashes; `""` → `"."`, root stays `"/"`. At most 255
    stacked components (string vec capacity); deeper paths abort.
  - `/`-separated only (no Windows `\` handling); composes with `std::fs` paths
- `std::vector` — `stdlib/std/vector.hll` (pure Hella; named `vector` because
  `vec` is a compiler keyword and cannot be an import segment)
  - int vec: `iLen`/`iIsEmpty`/`iContains`/`iFirst`/`iLast`, `iPush`/`iPop`
    (`ref`), `iSum`/`iIndexOf` (loops)
  - string vec: `sLen`/`sIsEmpty`/`sFirst`/`sLast`, `sPush`/`sPop` (`ref`),
    `sContains`/`sJoin` (byte comparison/exact-size copying — the
    `contains` method hangs on ptr-element vecs)
  - double vec: `dLen`, `dPush` (`ref`) only (`contains` miscompiles f64,
    `+` has no double overload)
  - no `*Clear`/`*Sort`/`*Reverse`: `clear()` fails LLVM verification even
    on locals, and indexing (`v[i]`, `v[i] = x`) plus `v.len()` do not lower
    through `ref` params — only `push`/`pop` method calls do. Clear with a
    caller-side `while v.len() > 0 do iPop(ref v) end` loop or by rebinding
    to a fresh literal; sort/reverse need compiler support first
- `std::map` — `stdlib/std/map.hll` (pure Hella, read-only)
  - `si*` (string:int), `ii*` (int:int), `ss*` (string:string):
    `Len`/`IsEmpty` (methods) + `Has`/`GetOr` (`for`-in loops — keyed
    methods and `m[k]` fail verification through by-value params)
  - mutate with direct local calls: subscript insert/update/read
    (`m[k] = v`, `m[k]`), `contains`/`get_or`/`len`/`is_empty` methods,
    `m.remove(k)`; `ref`-param method calls do not codegen. `m.clear()`
    miscompiles (LLVM verification) — pop in a loop or rebind instead.
    See `examples/stdlib_collections.hll` for the working pattern.
- `std::log` — `stdlib/std/log.hll` (pure Hella over `std::io`/`str`/`num`/`time`)
  - `logDebug`/`logInfo`/`logWarn`/`logError` to stderr, each line prefixed
    with wall-clock epoch millis and the level tag (timestamps order output;
    they are not pretty dates). `emitLog(level, s)` is the unfiltered
    escape hatch.
  - `LOG_DEBUG`/`LOG_INFO`/`LOG_WARN`/`LOG_ERROR` consts + `setLogLevel`
    over a program-global threshold (default `INFO`, like `std::rand`
    state: set once at startup, not task-synchronized). Above `LOG_ERROR`
    silences everything.
  - Lines use exact-size `concat`, never the interpolation buffer, so
    unbounded messages stay safe.
- `std::encoding` — `stdlib/std/encoding.hll` (pure Hella over `std::str`)
  - `hexEncode(s)` (lowercase, exact `2n` output), `hexDecode(s, ref value)`
    (either case; false + reset on odd length or non-hex input),
    `hexValue(c)` digit helper (`-1` on miss)
  - `base64Encode(s)` (standard alphabet, `=` padding, exact output),
    `base64Decode(s, ref value)` (strict length/`=`-placement/alphabet;
    false + reset on violation), `base64Value(c)` helper (`-1`, incl. `=`)
  - Possible because `char` widens to `int` at bindings and `int` narrows
    into `char` index stores (low byte); bytes are masked `& 255` after
    widening so high bytes behave regardless of char extension
- `std::hash` — `stdlib/std/hash.hll` (pure Hella over `std::str` + `std::encoding`)
  - `fnv1a64(s)` / `fnv1a32(s)` / `crc32(s)` (zlib/IEEE) as lowercase hex
    strings (16/8/8 chars, leading zeros kept). Digests render nibble-direct
    from the integer: Hella strings cannot hold NUL bytes (`len` is
    `strlen`-based), so a digest byte of zero would truncate a byte-string
    round-trip.
  - Non-cryptographic (no collision/preimage resistance — never passwords,
    tokens, or signatures; no crypto lives in the standard library).
    Wrapping mod-2^N arithmetic like `std::rand` relies on; `u64Pair` halves
    must be split (a 40-bit FNV prime in `lo` truncates — see module header).
- `std::fmt` — `stdlib/std/fmt.hll` (pure Hella over `std::str`/`vector`/`num`)
  - `join(sep, parts)` (via `sJoin`), `iJoin(sep, parts)` (int vec via
    `toString` each; empty yields `""`), `repeat(s, n)`, `padStart(s, width)`,
    `padEnd(s, width)`, `padCenter(s, width)` (extra space goes right) —
    checked-size allocations and pure Hella copying loops,
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
    `sleepMs`/`yieldNow` link from sync programs too (sync runtime);
    `cancelled()` needs an async context (async runtime).
- `std::sync` — `stdlib/std/sync.hll` (B1)
  - `mutexCreate`/`mutexLock`/`mutexUnlock`/`mutexDestroy` over
    `runtime/hella_sync.c` (opaque `any` handles, pthread/Win32).
    Non-recursive; guard `std::rand` globals or any state shared across
    `spawn`ed tasks. Linked only when imported.
- `std::chan` — `stdlib/std/chan.hll` (B1)
  - Bounded blocking FIFOs: `chanCreateInt`/`chanCreatePtr`,
    `chanSendInt`/`chanRecvInt` (`out` value + bool ok),
    `chanSendPtr`/`chanRecvPtr` (pointer queued, not copied),
    `chanClose`/`chanDestroy`/`chanLen`. Full senders block; `recv`
    returns `ok=false` when closed and drained. See `examples/sync_chan.hll`.
- `std::time` — `stdlib/std/time.hll` (B2)
  - `wallMs`/`wallMicros` (Unix epoch, via `hella_wall_micros`),
    `deadlineMs`/`expiredMs` helpers. Wall clock, not monotonic.
  - `monotonicMs()`/`monotonicMicros()` for elapsed measurement
    (via `hella_monotonic_micros` in `runtime/hella_sync.c`: Linux
    `CLOCK_MONOTONIC`, macOS monotonic-raw counter — which rejects
    `CLOCK_MONOTONIC` —, Windows `QueryPerformanceCounter`). Immune to
    NTP/DST steps; use for benchmarks and durations, never time-of-day.
    Zero on backend failure.
  - `dateUtc()`/`dateLocal()` as `"YYYY-MM-DD HH:MM:SS"` (fixed 19 chars,
    `""` on failure) via libc `gmtime_r`/`localtime_r` (POSIX) or
    `gmtime_s`/`localtime_s` (Windows, reversed args + errno) over an
    extern `Tm` struct (32-bit fields matching C layout — Hella `int`
    would misalign). No timezone suffix; selective importers take `Tm`
    along (`import std::time::{dateUtc, Tm}` also works explicitly).
  - `cpuMs()` — processor milliseconds (ISO C `clock()`). Monotonic
    while running, but CPU time, not wall: sleeping and parked tasks do
    not advance it. For CPU benchmarks, never wall timeouts. POSIX
    divides by 1000000, Windows returns millis directly (`@cfg` split).
- `std::net` — `stdlib/std/net.hll` (B2)
  - Blocking IPv4 TCP: `tcpConnect`/`tcpListen`/`tcpAccept`/
    `tcpSend`/`tcpRecv`/`tcpClose` (`int` fds, -1 on error, `recv` 0 on
    peer close). IP literals only (no DNS); caller-owned string buffers.
    Blocking parks the task thread — serve each fd on its own task
    (see `examples/net_echo.hll`). Evented I/O is future work.
  - Non-blocking + readiness (B3 event loops): `tcpSetNonblock(fd)`,
    `tcpPoll(fd, events, timeoutMs)` (mask 1 readable/HUP, 2 writable,
    4 error; 0 timeout, -1 error; `pollRead()`/`pollWrite()` bits).
    Loop over a vec of non-blocking fds in one task instead of parking
    a thread per connection (see `examples/net_poll.hll`). True
    multi-fd/one-syscall poll and io_uring/kqueue backends are future.
- `std::url` — `stdlib/std/url.hll` (pure Hella over `std::str` + `std::encoding`)
  - `Url parseUrl(s)` into `scheme`/`userinfo`/`host`/`port`/`path`/`query`/
    `fragment` + `hasAuthority`/`valid`. Split, never normalized: no case
    folding, no percent-normalization, no IDNA; empty query/fragment
    normalize away on rebuild. Empty input invalid (stricter than Go);
    bare `host:port` parses as `scheme:path` like Go; ports digits-only
    without range check; bracketed IPv6 keeps brackets; authority ends at
    `/?` so `@` there belongs to path/query, never userinfo.
  - `buildUrl(u)` recomposes (the `hasAuthority` flag round-trips
    `http:///empty` faithfully); `percentEncode(s)` (uppercase hex,
    never `+` for space) / `percentDecode(s, ref value)` (false + reset
    on truncated/non-hex `%`); `hasUrlControl`/`isUrlUnreserved`/
    `isSchemeStart`/`isSchemeRest` building blocks.
- `std::terminal` — `stdlib/std/terminal/mod.hll`
  - `isStdoutTerminal()` / `isStderrTerminal()` (`isatty` on Unix).
    Windows: not yet implemented — conservative false so styled programs
    degrade to plain text. Gate `std::terminal::ansi` styling on these.
- `std::terminal::ansi` — `stdlib/std/terminal/ansi.hll`
  - Plain `const string` values, no functions, no FFI: SGR styles
    (`RESET BOLD DIM ITALIC UNDERLINE ...`), standard + bright foreground
    (`RED GREEN ... BRIGHT_RED ...`) and background (`BG_RED ...`) colors,
    `FG_DEFAULT`/`BG_DEFAULT`, cursor movement and screen control
    (`CURSOR_UP/DOWN/FORWARD/BACK/HOME`, `ERASE_LINE`, `ERASE_DISPLAY_ALL`,
    `HIDE_CURSOR`/`SHOW_CURSOR`, alternate-screen enter/exit).
  - Pair with `std::io`: `println("{RED}error{RESET}")`. Non-TTY output
    keeps the raw bytes; gate on your own TTY detection when that matters.

Import examples:

```
import std::io
import std::io::{print, println}
```

Selective imports keep the wanted symbols plus everything they reference
(the resolver closes over call/identifier references, so `import std::str::{trim}`
still brings `substring`/`allocateString`, and `import std::rand::{flip}` still
brings the generator globals), and the module's `extern` blocks always ride along.
Unrelated declarations stay out, so importing two modules that define the same
helper name selectively does not collide.

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
./target/debug/hella build --force -f examples/stdlib_path.hll
./examples/stdlib_path
printf 'Ada\n42\n' | ./target/debug/hella run --force -f examples/stdlib_io.hll
```

`crates/hella-compiler/tests/stdlib.rs` uses the public lexer/parser/import/sema/
object APIs and a real C linker. It builds/runs all ten stdlib examples in debug
and release, checks exact output, exercises long/blank/EOF input, checks fatal
size-overflow guards, and checks selective imports keep transitive helpers
(`trim` + `toString` + `join` + an `ansi` const + `flip` with its globals). It uses checkout imports and isolated scratch files, no
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
- [RFC 4648](https://datatracker.ietf.org/doc/html/rfc4648): test vectors
  (`f`/`fo`/`foo`/`foob`/`fooba`/`foobar`) and strict `=`-placement rules.
  Hella returns bool + ref output with reset-on-failure instead of Result.
