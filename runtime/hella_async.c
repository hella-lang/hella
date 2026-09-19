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

#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
typedef HANDLE hella_thread_t;
typedef CRITICAL_SECTION hella_mutex_t;
typedef CONDITION_VARIABLE hella_cond_t;
static int hella_thread_create(hella_thread_t *out, void *(*fn)(void *), void *arg) {
    HANDLE h = CreateThread(NULL, 0, (LPTHREAD_START_ROUTINE)(void *)fn, arg, 0, NULL);
    if (h == NULL) return -1;
    *out = h;
    return 0;
}
static int hella_thread_join(hella_thread_t t) {
    DWORD r = WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
    return (r == WAIT_OBJECT_0) ? 0 : -1;
}
static void hella_mutex_init(hella_mutex_t *m) { InitializeCriticalSection(m); }
static void hella_mutex_lock(hella_mutex_t *m) { EnterCriticalSection(m); }
static void hella_mutex_unlock(hella_mutex_t *m) { LeaveCriticalSection(m); }
static void hella_cond_init(hella_cond_t *c) { InitializeConditionVariable(c); }
static void hella_cond_wait(hella_cond_t *c, hella_mutex_t *m) { SleepConditionVariableCS(c, m, INFINITE); }
static void hella_cond_signal(hella_cond_t *c) { WakeConditionVariable(c); }
#else
#include <pthread.h>
#include <sched.h>
#include <time.h>
#include <errno.h>
typedef pthread_t hella_thread_t;
typedef pthread_mutex_t hella_mutex_t;
typedef pthread_cond_t hella_cond_t;
static int hella_thread_create(hella_thread_t *out, void *(*fn)(void *), void *arg) { return pthread_create(out, NULL, fn, arg); }
static int hella_thread_join(hella_thread_t t) { return pthread_join(t, NULL); }
static void hella_mutex_init(hella_mutex_t *m) { pthread_mutex_init(m, NULL); }
static void hella_mutex_lock(hella_mutex_t *m) { pthread_mutex_lock(m); }
static void hella_mutex_unlock(hella_mutex_t *m) { pthread_mutex_unlock(m); }
static void hella_cond_init(hella_cond_t *c) { pthread_cond_init(c, NULL); }
static void hella_cond_wait(hella_cond_t *c, hella_mutex_t *m) { pthread_cond_wait(c, m); }
static void hella_cond_signal(hella_cond_t *c) { pthread_cond_signal(c); }
#endif

#ifdef _WIN32
#define HELLA_TLS __declspec(thread)
#else
#define HELLA_TLS _Thread_local
#endif

#define HELLA_TASK_INLINE 16

typedef struct hella_task {
    hella_thread_t thread;
    hella_mutex_t mu;
    hella_cond_t done_cv;
    int state; /* 0 = running, 1 = done, 2 = joined/consumed */
    int cancelled;
    unsigned char result[HELLA_TASK_INLINE];
    void *result_ptr;
    size_t result_size;
    /* worker entry: called as entry(task_handle, arg) so the worker can
     * store its result without racing the spawner's handle handoff */
    void *(*entry)(void *, void *);
    void *arg;
} hella_task_t;

/* Task running on THIS thread (TLS), or NULL outside a task. Backs
 * `hella_task_self()` so a task body can poll its own cancellation. */
static HELLA_TLS hella_task_t *hella_current_task = NULL;

static void *hella_task_trampoline(void *p) {
    hella_task_t *t = (hella_task_t *)p;
    hella_current_task = t;
    t->entry((void *)t, t->arg);
    hella_current_task = NULL;
    hella_mutex_lock(&t->mu);
    t->state = 1;
    hella_cond_signal(&t->done_cv);
    hella_mutex_unlock(&t->mu);
    return NULL;
}

void *hella_task_spawn(void *(*entry)(void *, void *), void *arg, size_t result_size) {
    hella_task_t *t = (hella_task_t *)malloc(sizeof(hella_task_t));
    if (!t) abort();
    memset(t, 0, sizeof(*t));
    hella_mutex_init(&t->mu);
    hella_cond_init(&t->done_cv);
    t->state = 0;
    t->result_size = result_size;
    t->entry = entry;
    t->arg = arg;
    if (result_size > HELLA_TASK_INLINE) {
        t->result_ptr = malloc(result_size ? result_size : 1);
        if (!t->result_ptr) abort();
    }
    if (hella_thread_create(&t->thread, hella_task_trampoline, t) != 0) abort();
    return (void *)t;
}

int hella_task_join(void *handle) {
    hella_task_t *t = (hella_task_t *)handle;
    if (!t) abort();
    hella_mutex_lock(&t->mu);
    while (t->state == 0) hella_cond_wait(&t->done_cv, &t->mu);
    if (t->state == 2) { hella_mutex_unlock(&t->mu); abort(); }
    t->state = 2;
    hella_mutex_unlock(&t->mu);
    if (hella_thread_join(t->thread) != 0) abort();
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
void *hella_task_self(void) {
    return (void *)hella_current_task;
}

void hella_task_cancel(void *handle) {
    hella_task_t *t = (hella_task_t *)handle;
    if (!t) return;
    hella_mutex_lock(&t->mu);
    t->cancelled = 1;
    hella_mutex_unlock(&t->mu);
}

int hella_task_cancelled(void *handle) {
    hella_task_t *t = (hella_task_t *)handle;
    int c;
    if (!t) return 0;
    hella_mutex_lock(&t->mu);
    c = t->cancelled;
    hella_mutex_unlock(&t->mu);
    return c;
}

/* Wall-clock sleep, used by `std::async::sleepMs`. Windows has no
 * POSIX `nanosleep`, so this is the portable entry point; POSIX builds
 * forward to `nanosleep` and retry on EINTR so the requested delay is
 * honoured rather than truncated by a signal. */
void hella_sleep_ms(int ms) {
    if (ms <= 0) return;
#ifdef _WIN32
    Sleep((DWORD)ms);
#else
    {
        struct timespec ts;
        ts.tv_sec = ms / 1000;
        ts.tv_nsec = (long)(ms % 1000) * 1000000L;
        while (nanosleep(&ts, &ts) == -1) {
            /* EINTR: `ts` holds the remaining time; loop to finish it. */
        }
    }
#endif
}

void hella_task_yield(void) {
#ifndef _WIN32
    sched_yield();
#else
    SwitchToThread();
#endif
}

#ifdef _WIN32
int sched_yield(void) { SwitchToThread(); return 0; }
#endif
