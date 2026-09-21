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
Step: starting step 1 (A4 ABI)
Policy: commit every single change; don't get stuck; get working first, optimize later; update all parts together (compiler + stdlib + examples + docs + tests).

## Log
- 2026-09-21: branch created, roadmap noted. Starting A4.
