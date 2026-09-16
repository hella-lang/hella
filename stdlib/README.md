# Hella Standard Library

Pure Hella sources. See `.opencode/skills/stdlib/SKILL.md` for import semantics (EBNF §32),
`.opencode/skills/stdlib/REAL_STDLIB.md` for the bare-minimum compiler contract, and roadmap.

The compiler knows no user-facing IO names. Every symbol below is an ordinary Hella
function defined in `stdlib/` on top of `extern "c"` libc declarations. Calling one
without its `import` is a sema error (`undefined function`) by design.

## Modules

- `std::io` — `stdlib/std/io.hll`
  - `void print(string s)` — no newline (`printf("%s", s)`)
  - `void println(string s)` — with newline (`puts(s)`)
  - `void printInt(int n)` — decimal with newline (`printf("%ld\n", n)`)
  - `void putChar(char c)` — single char, no newline (`putchar(c)`)
  - `void eprint(string s)` / `void eprintln(string s)` — stderr, without/with
    newline (`write(2, …)`; no `FILE*` global needed)
  - `string readLine()` — one stdin line sans newline, 255-byte cap, `""` on EOF
    (`calloc` + `scanf("%255[^\n]%*c")`)
  - `int readInt()` — one stdin integer, `0` on EOF (`scanf("%ld%*c", out n)`)
  - extern linkage (not imported selectively — always carried along):
    `i32 puts(string s)`, `i32 printf(string fmt, ...)`, `i32 putchar(char c)`,
    `int write(int fd, string buf, int count)`, `string calloc(int n, int size)`,
    `int scanf(string fmt, ...)`
- `std::math` — `stdlib/std/math.hll`
  - `double sqrt(double x)`, `sin`/`cos`/`tan`, `pow`, `floor`/`ceil`/`round`,
    `log`/`exp`, `fabs`, `fmin`/`fmax`, `atan2` via `libm`
  - `int abs(int x)`, `min`/`max`, `clamp`, `absf`/`minf`/`maxf` wrappers
- `std::str` — `stdlib/std/str.hll`
  - `int len(string s)`, `bool isEmpty`, `bool equals`/`int compare`,
    `bool contains`/`startsWith`/`endsWith`, `string clone`/`substring`
  - via `strlen`/`strcmp`/`strncmp`/`strdup`/`calloc`; `contains`/`endsWith`/
    `substring` are pure Hella loops (no `Range` iteration — while loops)
- `std::env` — `stdlib/std/env.hll`
  - `string getEnv(string name)` (getenv + "" on miss, `string is null` now allowed), `bool hasEnv`, `void setEnv`/`unsetEnv` (setenv/unsetenv), `string cwd()` (getcwd + calloc)
- `std::fs` — `stdlib/std/fs.hll`
  - `bool exists(string path)` (access), `string readFile`/`void writeFile`/`appendFile`/`removeFile` (fopen/fseek/ftell/fread/fwrite/remove)
- `std::vector` — `stdlib/std/vector.hll` (pure Hella; named `vector` because
  `vec` is a compiler keyword and cannot be an import segment)
  - int vec: `iLen`/`iIsEmpty`/`iContains`/`iFirst`/`iLast`, `iPush`/`iPop`
    (`ref`), `iSum`/`iIndexOf` (loops)
  - string vec: `sLen`/`sIsEmpty`/`sFirst`/`sLast`, `sPush`/`sPop` (`ref`),
    `sContains`/`sJoin` (loops over `equals`/interpolation — the
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
  - `formatS`/`formatSS`/`formatD`/`formatDD`/`formatF`/`formatDS` over
    `sprintf` into 4KB `calloc` slabs (one fn per arity/shape — Hella has
    no spread/forwarding syntax for C varargs)
  - `join(sep, parts)` (via `sJoin`), `repeat`, `padStart`/`padEnd`
    (interpolation loops)
- `std::types` — `stdlib/std/types.hll` (doc-only manifest of the implicit
  environment: `bool string i8…u128 int uint float double`; importing is a no-op)

Import examples:

```
import std::io
import std::io::{print, println}
```

Selective imports keep the module's `extern` blocks automatically (linkage
requirements, not selectable symbols).

## Build

`./target/debug/hella build examples/stdlib_io.hll` inlines `stdlib/std/io.hll` and links against libc. The input half needs piped stdin:

```
printf 'Ada\n42\n' | ./stdlib_io
```

## Not yet

`std::env` argument plumbing (`argCount`/`argAt`), `std::option`/`std::result`
(generic enum payloads codegen as `i64` only), trait-extension sugar
(`extend string do … end`).
