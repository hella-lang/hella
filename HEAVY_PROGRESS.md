# Heavy-Duty Completeness — Progress Note

Goal: make Hella complete enough for OS, self-hosted compiler, servers.
Decisions (2026-09-21): priority = self-host compiler first; errors stay `bool+ref/out` (no Result/?); concurrency = pool+channels first, event-loop later.

## Order
- [ ] 1. A4 ABI fixes (unsigned zext, i32 returns, %lld interp)
- [ ] 2. A2 growable vec/map + ref-call fixes
- [ ] 3. A3 string builder + interpolation bounds
- [ ] 4. A1 true generic monomorphization
- [ ] 5. B1 thread pool + channels/mutex + thread-safe rand
- [ ] 6. B2 std::net/time (blocking first)
- [ ] 7. C systems (raw ptrs, volatile/asm, no_std, --target)
- [ ] 8. B3 non-blocking async I/O executor
- [ ] 9. Keep all tests green + docs/examples in sync

## Current position
Branch: `heavy-duty/complete`
Step: B1 done, starting B2 (std::net/time)
Policy: commit every single change; don't get stuck; get working first, optimize later; update all parts together (compiler + stdlib + examples + docs + tests).

## Log
- 2026-09-21: branch created, roadmap noted. Starting A4.
- 2026-09-21: A4 done (unsigned zext helper + unify, u32/i32 extern returns, %lld). Committed.
- 2026-09-21: A2 partial (working-first): VEC/MAP cap 16->256 committed. Found: `v.len()`/`v[i]`/`v.pop()`-as-stmt do NOT lower through `ref` params (only `push`/`return v.pop()` verify); caller-side `while v.len()>0 do v.pop() end` on locals works. Full heap-growable vec + ref-method inference fix deferred to follow-up; not blocking A3.
- 2026-09-21: A3 done (bounded 4KiB interp via strncat/snprintf truncation, `%lld`, `concat` builder in std::str). Committed.
- 2026-09-21: A1 done (generic struct monomorphization: `Box<int>` vs `Box<string>` specialize; sema subst for literals/field access; codegen ensure on demand + pre-pass). Generic function mangling still future. Committed.
- 2026-09-21: B1 done (runtime/hella_sync.c mutex+int/ptr channels; CLI conditional link + stamp; `std::sync`/`std::chan`; examples/sync_chan.hll producer/consumer). Also fixed: extern `ref`/`out` params lower to ptr (was value → verify fail; `rand_s` was silently broken), extern `any`/ptr/fn returns lower to ptr (was void → null handles). Workspace suite green (236 tests).
