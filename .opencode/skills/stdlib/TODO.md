# Hella Stdlib — TODO (easy → hard)

Single source of truth for stdlib work. Ordered by dependency on compiler
primitives: do in order, one `S-*` at a time. All modules are pure Hella
under `stdlib/std/*.hll` on top of `extern "c"` FFI — no compiler intrinsics.

Current baseline (shipped): `std::io` (8 fns via printf/puts/write/scanf) + `std::types` doc-only.

- [x] **S-1 `std::math` — DONE: math via `libm`** (`stdlib/std/math.hll`: sqrt/sin/cos/tan/pow/floor/ceil/round/log/exp/fabs/fmin/fmax/atan2 + int abs/min/max/clamp; `examples/stdlib_math.hll` 16 asserts + "math ok"/"doubles ok")
- [x] **S-2 `std::str` — DONE: string utilities** (`stdlib/std/str.hll`: len/isEmpty/equals/compare/contains/startsWith/endsWith/clone/substring via strlen/strcmp/strncmp/strdup/calloc + while loops; `examples/stdlib_string.hll` 13 asserts + "string ok"; fixes `string[i]` codegen `i8` GEP + `i32` char handling)
- [ ] **S-3 `std::env` — process environment** — `string getEnv(string name)` (getenv + empty on miss), `int argCount()` / `string argAt(int i)` via `int main(string[] args)` plumbing if available else extern `getenv("HELLA_ARGS")` fallback, `void exit(int code)` (extern `exit`). Tests: `examples/stdlib_env.hll` checks `getEnv` miss → "" and `argCount` ≥1.
- [ ] **S-4 `std::fs` — file I/O** — `bool exists(string path)`, `string readFile(string path)` (fopen/fread via `extern`), `void writeFile(string path, string data)` (fopen/fwrite), `void appendFile`, `void removeFile`, `bool isDir`. Uses `extern "c" from "libc"` `FILE*` opaque + `fopen`/`fclose`/`fread`/`fwrite`/`remove`/`stat`. Tests: `examples/stdlib_fs.hll` writes/reads/removes a temp file under `/tmp`.
- [ ] **S-5 `std::vec` + `std::map` helpers — collection facades** — `std::vec`: `int len<T>(T vec)`, `void push<T>(T vec, T elem)` etc. as generic free functions over the `T vec` layout (fixed-cap [16 x T] today); `std::map`: `bool has<K,V>(K:V map, K key)`. Thin wrappers over compiler's `len`/`push`/`contains` mechanics where available, otherwise Hella loops. Tests: `examples/stdlib_collections.hll` push/contains/len with int and string vecs.
- [ ] **S-6 `std::fmt` — formatting helpers** — `string format(string fmt, ...)` thin wrapper over `sprintf` (reuses interpolation buffer), `string join(string sep, string[] parts)`. Tests: `examples/stdlib_fmt.hll` interpolation parity.

**How to use:** mark `in_progress` one at a time, write `stdlib/std/<mod>.hll`
as pure Hella (type-first `int x`, `do…end`, `has…end`), add `examples/stdlib_<mod>.hll`
regression, run `cargo build -p hella && ./target/debug/hella build -f examples/stdlib_<mod>.hll && ./examples/stdlib_<mod>` plus `cargo test --workspace`, then mark `completed`.
