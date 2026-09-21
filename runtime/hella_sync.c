/* hella_sync.c -- mutex + bounded channel runtime for Hella (B1).
 *
 * Working-first primitives for servers (optimize later):
 * - mutex: opaque handle (malloc'd pthread_mutex / CRITICAL_SECTION),
 *   create/lock/unlock/destroy. Recursive? No — non-recursive, like
 *   pthreads default. Lock failures abort (fatal-assert policy, same as
 *   the async runtime).
 * - chan: bounded FIFO of int64 slots plus a parallel ptr-slot array, so
 *   one queue carries both `int` channels and `any`/string (ptr) channels.
 *   Blocking send/recv on mutex+condvars; close wakes all waiters.
 *   send on closed/full? Full blocks; closed channel: send aborts (loud,
 *   like double-await), recv returns 0/NULL with `ok=0`.
 *
 * Threading: pthreads on POSIX, Win32 (CRITICAL_SECTION + CONDITION_VARIABLE)
 * on Windows — mirrors runtime/hella_async.c.
 */

#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
typedef HANDLE hella_thread_t;
typedef CRITICAL_SECTION hella_mutex_t;
typedef CONDITION_VARIABLE hella_cond_t;
static void hella_m_init(hella_mutex_t *m) { InitializeCriticalSection(m); }
static void hella_m_lock(hella_mutex_t *m) { EnterCriticalSection(m); }
static void hella_m_unlock(hella_mutex_t *m) { LeaveCriticalSection(m); }
static void hella_m_destroy(hella_mutex_t *m) { DeleteCriticalSection(m); }
static void hella_c_init(hella_cond_t *c) { InitializeConditionVariable(c); }
static void hella_c_wait(hella_cond_t *c, hella_mutex_t *m) { SleepConditionVariableCS(c, m, INFINITE); }
static void hella_c_signal(hella_cond_t *c) { WakeConditionVariable(c); }
static void hella_c_broadcast(hella_cond_t *c) { WakeAllConditionVariable(c); }
#else
#include <pthread.h>
typedef pthread_mutex_t hella_mutex_t;
typedef pthread_cond_t hella_cond_t;
static void hella_m_init(hella_mutex_t *m) { pthread_mutex_init(m, NULL); }
static void hella_m_lock(hella_mutex_t *m) { pthread_mutex_lock(m); }
static void hella_m_unlock(hella_mutex_t *m) { pthread_mutex_unlock(m); }
static void hella_m_destroy(hella_mutex_t *m) { pthread_mutex_destroy(m); }
static void hella_c_init(hella_cond_t *c) { pthread_cond_init(c, NULL); }
static void hella_c_wait(hella_cond_t *c, hella_mutex_t *m) { pthread_cond_wait(c, m); }
static void hella_c_signal(hella_cond_t *c) { pthread_cond_signal(c); }
static void hella_c_broadcast(hella_cond_t *c) { pthread_cond_broadcast(c); }
#endif

/* ── mutex ── */

typedef struct { hella_mutex_t mu; } hella_mutex_handle;

void *hella_mutex_create(void) {
    hella_mutex_handle *h = (hella_mutex_handle *)malloc(sizeof(*h));
    if (!h) abort();
    hella_m_init(&h->mu);
    return (void *)h;
}

void hella_mutex_lock(void *handle) {
    hella_mutex_handle *h = (hella_mutex_handle *)handle;
    if (!h) abort();
    hella_m_lock(&h->mu);
}

void hella_mutex_unlock(void *handle) {
    hella_mutex_handle *h = (hella_mutex_handle *)handle;
    if (!h) abort();
    hella_m_unlock(&h->mu);
}

void hella_mutex_destroy(void *handle) {
    hella_mutex_handle *h = (hella_mutex_handle *)handle;
    if (!h) return;
    hella_m_destroy(&h->mu);
    free(h);
}

/* ── channel ── */

typedef struct {
    hella_mutex_t mu;
    hella_cond_t not_full;
    hella_cond_t not_empty;
    int64_t *ivals;
    void **pvals;
    int64_t cap;
    int64_t head;
    int64_t tail;
    int64_t count;
    int closed;
} hella_chan_t;

void *hella_chan_create(int64_t capacity) {
    hella_chan_t *c;
    if (capacity <= 0) capacity = 16;
    if (capacity > 1000000) capacity = 1000000;
    c = (hella_chan_t *)malloc(sizeof(*c));
    if (!c) abort();
    memset(c, 0, sizeof(*c));
    c->ivals = (int64_t *)malloc(sizeof(int64_t) * (size_t)capacity);
    c->pvals = (void **)malloc(sizeof(void *) * (size_t)capacity);
    if (!c->ivals || !c->pvals) abort();
    c->cap = capacity;
    hella_m_init(&c->mu);
    hella_c_init(&c->not_full);
    hella_c_init(&c->not_empty);
    return (void *)c;
}

void hella_chan_close(void *handle) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c) return;
    hella_m_lock(&c->mu);
    c->closed = 1;
    hella_c_broadcast(&c->not_full);
    hella_c_broadcast(&c->not_empty);
    hella_m_unlock(&c->mu);
}

void hella_chan_destroy(void *handle) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c) return;
    hella_m_destroy(&c->mu);
    free(c->ivals);
    free(c->pvals);
    free(c);
}

/* Returns 0 on success, -1 when closed. */
int hella_chan_send_int(void *handle, int64_t v) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c) abort();
    hella_m_lock(&c->mu);
    while (c->count == c->cap && !c->closed) hella_c_wait(&c->not_full, &c->mu);
    if (c->closed) { hella_m_unlock(&c->mu); return -1; }
    c->ivals[c->tail] = v;
    c->pvals[c->tail] = NULL;
    c->tail = (c->tail + 1) % c->cap;
    c->count++;
    hella_c_signal(&c->not_empty);
    hella_m_unlock(&c->mu);
    return 0;
}

/* Returns 0 on success (v filled), -1 when closed AND drained. */
int hella_chan_recv_int(void *handle, int64_t *out) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c || !out) abort();
    hella_m_lock(&c->mu);
    while (c->count == 0 && !c->closed) hella_c_wait(&c->not_empty, &c->mu);
    if (c->count == 0 && c->closed) { hella_m_unlock(&c->mu); return -1; }
    *out = c->ivals[c->head];
    c->head = (c->head + 1) % c->cap;
    c->count--;
    hella_c_signal(&c->not_full);
    hella_m_unlock(&c->mu);
    return 0;
}

/* Returns 0 on success, -1 when closed. */
int hella_chan_send_ptr(void *handle, void *v) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c) abort();
    hella_m_lock(&c->mu);
    while (c->count == c->cap && !c->closed) hella_c_wait(&c->not_full, &c->mu);
    if (c->closed) { hella_m_unlock(&c->mu); return -1; }
    c->ivals[c->tail] = 0;
    c->pvals[c->tail] = v;
    c->tail = (c->tail + 1) % c->cap;
    c->count++;
    hella_c_signal(&c->not_empty);
    hella_m_unlock(&c->mu);
    return 0;
}

/* Returns 0 on success (out filled), -1 when closed AND drained. */
int hella_chan_recv_ptr(void *handle, void **out) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c || !out) abort();
    hella_m_lock(&c->mu);
    while (c->count == 0 && !c->closed) hella_c_wait(&c->not_empty, &c->mu);
    if (c->count == 0 && c->closed) { hella_m_unlock(&c->mu); return -1; }
    *out = c->pvals[c->head];
    c->head = (c->head + 1) % c->cap;
    c->count--;
    hella_c_signal(&c->not_full);
    hella_m_unlock(&c->mu);
    return 0;
}

int64_t hella_chan_len(void *handle) {
    hella_chan_t *c = (hella_chan_t *)handle;
    int64_t n;
    if (!c) return -1;
    hella_m_lock(&c->mu);
    n = c->count;
    hella_m_unlock(&c->mu);
    return n;
}
