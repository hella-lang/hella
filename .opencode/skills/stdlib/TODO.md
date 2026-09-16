# Hella Stdlib — TODO (easy → hard)

Single source of truth for stdlib work. Ordered by dependency on compiler
primitives: do in order, one `S-*` at a time. All modules are pure Hella
under `stdlib/std/*.hll` on top of `extern "c"` FFI — no compiler intrinsics.

Current baseline (shipped): `std::io` (8 fns via printf/puts/write/scanf) + `std::types` doc-only.

- [x] **S-1 `std::math` — DONE: math via `libm`** (`stdlib/std/math.hll`: sqrt/sin/cos/tan/pow/floor/ceil/round/log/exp/fabs/fmin/fmax/atan2 + int abs/min/max/clamp; `examples/stdlib_math.hll` 16 asserts + "math ok"/"doubles ok")
- [x] **S-2 `std::str` — DONE: string utilities** (`stdlib/std/str.hll`: len/isEmpty/equals/compare/contains/startsWith/endsWith/clone/substring via strlen/strcmp/strncmp/strdup/calloc + while loops; `examples/stdlib_string.hll` 13 asserts + "string ok"; fixes `string[i]` codegen `i8` GEP + `i32` char handling)
- [x] **S-3 `std::env` — DONE: process environment** (`stdlib/std/env.hll`: getEnv/hasEnv/setEnv/unsetEnv/cwd via getenv/setenv/unsetenv/getcwd/calloc + `string is null` fix; `examples/stdlib_env.hll` HOME/missing/set/unset/cwd 6 asserts)
- [x] **S-4 `std::fs` — DONE: file I/O** (`stdlib/std/fs.hll`: exists/readFile/writeFile/appendFile/removeFile via fopen/fclose/fseek/ftell/fread/fwrite/remove/access/calloc/strlen; `examples/stdlib_fs.hll` write/read/append/remove + missing 5 asserts)
- [x] **S-5 `std::vector` + `std::map` — DONE: collection facades** (concrete, not generic: no overloading; `vec` is a keyword so the module is `std::vector`). `stdlib/std/vector.hll`: int vec `iLen/iIsEmpty/iContains/iFirst/iLast/iPush/iPop/iSum/iIndexOf`, string vec `sLen/sIsEmpty/sContains/sFirst/sLast/sPush/sPop/sJoin`, double vec `dLen/dPush`; `stdlib/std/map.hll` (read-only): `si*`/`ii*`/`ss*` `Len/IsEmpty/Has/GetOr`. Mutating helpers take `ref`. `examples/stdlib_collections.hll` push/contains/len with int+string vecs, all maps. Compiler gaps found (facades work around or omit): generic `T vec` params now resolve (sema `canonicalize_generic_param`), but generic *codegen* monomorphization fails for string-vec/map layouts + `ref` vec/map method calls; `clear()` fails verification even on locals; string-vec `contains` hangs (ptr-compare); double `contains` miscompiles + no double `+`; `m[k]`/keyed methods fail through by-value params (loops + `for`-in used instead).
- [x] **S-6 `std::fmt` — DONE: formatting helpers** (`stdlib/std/fmt.hll`: `formatS/formatSS/formatD/formatDD/formatF/formatDS` over `sprintf` 4KB `calloc` slabs — no Hella spread syntax exists so one fn per arity/shape; `join` via `sJoin`; pure-interpolation `repeat/padStart/padEnd`; `examples/stdlib_fmt.hll` interpolation parity).

**How to use:** mark `in_progress` one at a time, write `stdlib/std/<mod>.hll`
as pure Hella (type-first `int x`, `do…end`, `has…end`), add `examples/stdlib_<mod>.hll`
regression, run `cargo build -p hella && ./target/debug/hella build -f examples/stdlib_<mod>.hll && ./examples/stdlib_<mod>` plus `cargo test --workspace`, then mark `completed`.
