# Heavy-Duty Completeness — Progress Note

Goal: make Hella complete enough for OS, self-hosted compiler, servers.
Decisions (2026-09-21): priority = self-host compiler first; errors stay `bool+ref/out` (no Result/?); concurrency = pool+channels first, event-loop later.

## Order
- [x] 1. A4 ABI fixes (unsigned zext, i32 returns, %lld interp)
- [x] 2. A2 growable vec/map + ref-call fixes (working-first: cap 256)
- [x] 3. A3 string builder + interpolation bounds
- [x] 4. A1 true generic monomorphization (structs; fn mangling future)
- [x] 5. B1 thread pool + channels/mutex + thread-safe rand (pool = pattern over spawn+chan; rand guarded by mutex, no dedicated API)
- [x] 6. B2 std::net/time (blocking first)
- [x] 7. C systems (raw ptrs `&`/`*`, `--target`; volatile/asm/no_std/atomics deferred)
- [x] 8. B3 non-blocking async I/O executor (working-first: poll readiness; io_uring/kqueue future)
- [x] 9. Keep all tests green + docs/examples in sync

## Current position
Branch: `heavy-duty/complete`
Step: ALL DONE — final verification + docs sync.
Policy: commit every single change; don't get stuck; get working first, optimize later; update all parts together (compiler + stdlib + examples + docs + tests).

## Deferred (need design, not forgotten)
- Heap-growable `{ptr,len,cap}` vec/map; `ref`-param `len`/`[]`/`pop`-as-stmt inference.
- Generic function mangling (per-arg-set specialization).
- `volatile`, inline `asm`, atomics, freestanding/`no_std`, allocator control.
- DNS hostnames, TLS, multi-fd/one-syscall poll, io_uring/kqueue executor.
- `and`/`or` short-circuit semantics (today both sides evaluate — OOB guard with nested `if`).
- Reported as found: extern `ref`/`any` ABI (FIXED), `and`-OOB crash (doc pattern), narrow-int varargs (FIXED), `main(args)` indexing is 0-based excl. prog name.

## Log
- 2026-09-21: branch created, roadmap noted. Starting A4.
- 2026-09-21: A4 done (unsigned zext helper + unify, u32/i32 extern returns, %lld). Committed.
- 2026-09-21: A2 partial (working-first): VEC/MAP cap 16->256 committed. Found: `v.len()`/`v[i]`/`v.pop()`-as-stmt do NOT lower through `ref` params (only `push`/`return v.pop()` verify); caller-side `while v.len()>0 do v.pop() end` on locals works. Full heap-growable vec + ref-method inference fix deferred to follow-up; not blocking A3.
- 2026-09-21: A3 done (bounded 4KiB interp via strncat/snprintf truncation, `%lld`, `concat` builder in std::str). Committed.
- 2026-09-21: A1 done (generic struct monomorphization: `Box<int>` vs `Box<string>` specialize; sema subst for literals/field access; codegen ensure on demand + pre-pass). Generic function mangling still future. Committed.
- 2026-09-21: B1 done (runtime/hella_sync.c mutex+int/ptr channels; CLI conditional link + stamp; `std::sync`/`std::chan`; examples/sync_chan.hll producer/consumer). Also fixed: extern `ref`/`out` params lower to ptr (was value → verify fail; `rand_s` was silently broken), extern `any`/ptr/fn returns lower to ptr (was void → null handles). Workspace suite green (236 tests).
- 2026-09-21: B2 done (wall-clock, blocking TCP echo working; sleep/yield+task-identity split into sync runtime via hella_task.h; `hella_*` wide returns; async test harness links sync too). Workspace green.
- 2026-09-21: C done (`&x`/`*p` + `*p=v` end-to-end; `--target` host-verified, clean error otherwise; fixed narrow-int varargs + u8→int sext + pointee tracking; EBNF + README + stdlib docs synced). Workspace green.
- 2026-09-21: B3 done (non-blocking + tcpPoll readiness; net_poll.hll single-threaded 2-client echo exit 0). Workspace green. ALL TRACKS COMPLETE.
