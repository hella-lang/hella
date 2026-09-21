/* hella_async.c -- structured-concurrency runtime for Hella (Async-7).
 *
 * Model: every `async` function body runs on its own thread (pthreads on
 * Linux/macOS; Win32 threads on Windows). A `task<T>` value at LLVM level
 * is a `hella_task_t*` handle:
 *
 *   spawn f(args)   -> hella_task_spawn(entry, arg) -> handle
 *   await handle    -> hella_task_join(handle) + load inline result
 *   scope exit      -> structural (sema forces every task to be awaited
 *                      inside the scope), so no runtime barrier is needed
 *   yield           -> sched_yield() (plain libc call, not this file)
 *   async main      -> the CLI wraps it as a root task and joins it
 *
 * Result transport: small results (<= 16 bytes) are stored INLINE in the
 * task struct; `await` loads them directly. Larger results spill to a
 * malloc'd buffer; `await` loads through it and frees it.
 *
 * Exactly-once: join consumes (RUNNING -> DONE -> JOINED). A second join
 * aborts (sema already rejects double-await statically; this is defense
 * in depth). Allocation/thread failures abort (fatal-assert policy).
 *
 * Cancellation: cooperative. `hella_task_cancel` sets a flag the worker
 * polls via `hella_task_cancelled` at checkpoints. Blocking FFI calls do
 * NOT poll -- they run to completion.
 */

/* Shared task-state foundation (thread/mutex/cond impl, task struct, TLS
 * slot declaration): runtime/hella_task.h. The TLS slot + cancellation
 * flag live in the SYNC runtime so `cancelled()` links from sync programs;
 * the CLI always links sync alongside async. Sleep/yield also live in the
 * sync runtime for the same reason. */
#include "hella_task.h"

static void *hella_task_trampoline(void *p) {
    hella_task_t *t = (hella_task_t *)p;
    hella_current_task = t;
    t->entry((void *)t, t->arg);
    hella_current_task = NULL;
    hella_mu_lock(&t->mu);
    t->state = 1;
    hella_cv_signal(&t->done_cv);
    hella_mu_unlock(&t->mu);
    return NULL;
}

void *hella_task_spawn(void *(*entry)(void *, void *), void *arg, size_t result_size) {
    hella_task_t *t = (hella_task_t *)malloc(sizeof(hella_task_t));
    if (!t) abort();
    memset(t, 0, sizeof(*t));
    hella_mu_init(&t->mu);
    hella_cv_init(&t->done_cv);
    t->state = 0;
    t->result_size = result_size;
    t->entry = entry;
    t->arg = arg;
    if (result_size > HELLA_TASK_INLINE) {
        t->result_ptr = malloc(result_size ? result_size : 1);
        if (!t->result_ptr) abort();
    }
    if (hella_thr_create(&t->thread, hella_task_trampoline, t) != 0) abort();
    return (void *)t;
}

int hella_task_join(void *handle) {
    hella_task_t *t = (hella_task_t *)handle;
    if (!t) abort();
    hella_mu_lock(&t->mu);
    while (t->state == 0) hella_cv_wait(&t->done_cv, &t->mu);
    if (t->state == 2) { hella_mu_unlock(&t->mu); abort(); }
    t->state = 2;
    hella_mu_unlock(&t->mu);
    if (hella_thr_join(t->thread) != 0) abort();
    return 0;
}

void hella_task_result(void *handle, void *dst, size_t n) {
    hella_task_t *t = (hella_task_t *)handle;
    if (!t || !dst) abort();
    if (n > 0) {
        if (t->result_ptr) { memcpy(dst, t->result_ptr, n); free(t->result_ptr); }
        else memcpy(dst, t->result, n < HELLA_TASK_INLINE ? n : HELLA_TASK_INLINE);
    } else if (t->result_ptr) free(t->result_ptr);
    free(t);
}

void hella_task_store_inline(void *handle, const void *src, size_t n) {
    hella_task_t *t = (hella_task_t *)handle;
    if (!t || !src || n > HELLA_TASK_INLINE) abort();
    memcpy(t->result, src, n);
}

void hella_task_store_spill(void *handle, const void *src, size_t n) {
    hella_task_t *t = (hella_task_t *)handle;
    if (!t || !src || !t->result_ptr) abort();
    memcpy(t->result_ptr, src, n);
}

/* The task running on this thread (NULL in the main thread / outside a
 * task). Pair with `hella_task_cancelled` for cooperative cancellation. */
/* Task identity + cooperative cancellation (`hella_task_self`,
 * `hella_task_cancel`, `hella_task_cancelled`) live in the SYNC runtime
 * (hella_sync.c, via hella_task.h) so `cancelled()` links from sync
 * programs. The trampoline above publishes through the shared TLS slot.
 */
