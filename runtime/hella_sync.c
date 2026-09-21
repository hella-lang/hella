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
#include <sys/time.h>
#ifndef _WIN32
#include <time.h>
#include <sched.h>
#include <errno.h>
#endif

/* Shared task-state foundation (thread/mutex/cond impl, task struct, TLS
 * slot declaration): runtime/hella_task.h. This TU DEFINES the TLS slot
 * and task identity/cancellation so `cancelled()` links from sync
 * programs; the async trampoline publishes through it. */
#include "hella_task.h"

HELLA_TLS hella_task_t *hella_current_task = NULL;

void *hella_task_self(void) {
    return (void *)hella_current_task;
}

void hella_task_cancel(void *handle) {
    hella_task_t *t = (hella_task_t *)handle;
    if (!t) return;
    hella_mu_lock(&t->mu);
    t->cancelled = 1;
    hella_mu_unlock(&t->mu);
}

int hella_task_cancelled(void *handle) {
    hella_task_t *t = (hella_task_t *)handle;
    int c;
    if (!t) return 0;
    hella_mu_lock(&t->mu);
    c = t->cancelled;
    hella_mu_unlock(&t->mu);
    return c;
}

/* ── mutex ── */

typedef struct { hella_mutex_t mu; } hella_mutex_handle;

void *hella_mutex_create(void) {
    hella_mutex_handle *h = (hella_mutex_handle *)malloc(sizeof(*h));
    if (!h) abort();
    hella_mu_init(&h->mu);
    return (void *)h;
}

void hella_mutex_lock(void *handle) {
    hella_mutex_handle *h = (hella_mutex_handle *)handle;
    if (!h) abort();
    hella_mu_lock(&h->mu);
}

void hella_mutex_unlock(void *handle) {
    hella_mutex_handle *h = (hella_mutex_handle *)handle;
    if (!h) abort();
    hella_mu_unlock(&h->mu);
}

void hella_mutex_destroy(void *handle) {
    hella_mutex_handle *h = (hella_mutex_handle *)handle;
    if (!h) return;
    hella_mu_destroy(&h->mu);
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
    hella_mu_init(&c->mu);
    hella_cv_init(&c->not_full);
    hella_cv_init(&c->not_empty);
    return (void *)c;
}

void hella_chan_close(void *handle) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c) return;
    hella_mu_lock(&c->mu);
    c->closed = 1;
    hella_cv_broadcast(&c->not_full);
    hella_cv_broadcast(&c->not_empty);
    hella_mu_unlock(&c->mu);
}

void hella_chan_destroy(void *handle) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c) return;
    hella_mu_destroy(&c->mu);
    free(c->ivals);
    free(c->pvals);
    free(c);
}

/* Returns 0 on success, -1 when closed. */
int hella_chan_send_int(void *handle, int64_t v) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c) abort();
    hella_mu_lock(&c->mu);
    while (c->count == c->cap && !c->closed) hella_cv_wait(&c->not_full, &c->mu);
    if (c->closed) { hella_mu_unlock(&c->mu); return -1; }
    c->ivals[c->tail] = v;
    c->pvals[c->tail] = NULL;
    c->tail = (c->tail + 1) % c->cap;
    c->count++;
    hella_cv_signal(&c->not_empty);
    hella_mu_unlock(&c->mu);
    return 0;
}

/* Returns 0 on success (v filled), -1 when closed AND drained. */
int hella_chan_recv_int(void *handle, int64_t *out) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c || !out) abort();
    hella_mu_lock(&c->mu);
    while (c->count == 0 && !c->closed) hella_cv_wait(&c->not_empty, &c->mu);
    if (c->count == 0 && c->closed) { hella_mu_unlock(&c->mu); return -1; }
    *out = c->ivals[c->head];
    c->head = (c->head + 1) % c->cap;
    c->count--;
    hella_cv_signal(&c->not_full);
    hella_mu_unlock(&c->mu);
    return 0;
}

/* Returns 0 on success, -1 when closed. */
int hella_chan_send_ptr(void *handle, void *v) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c) abort();
    hella_mu_lock(&c->mu);
    while (c->count == c->cap && !c->closed) hella_cv_wait(&c->not_full, &c->mu);
    if (c->closed) { hella_mu_unlock(&c->mu); return -1; }
    c->ivals[c->tail] = 0;
    c->pvals[c->tail] = v;
    c->tail = (c->tail + 1) % c->cap;
    c->count++;
    hella_cv_signal(&c->not_empty);
    hella_mu_unlock(&c->mu);
    return 0;
}

/* Returns 0 on success (out filled), -1 when closed AND drained. */
int hella_chan_recv_ptr(void *handle, void **out) {
    hella_chan_t *c = (hella_chan_t *)handle;
    if (!c || !out) abort();
    hella_mu_lock(&c->mu);
    while (c->count == 0 && !c->closed) hella_cv_wait(&c->not_empty, &c->mu);
    if (c->count == 0 && c->closed) { hella_mu_unlock(&c->mu); return -1; }
    *out = c->pvals[c->head];
    c->head = (c->head + 1) % c->cap;
    c->count--;
    hella_cv_signal(&c->not_full);
    hella_mu_unlock(&c->mu);
    return 0;
}

int64_t hella_chan_len(void *handle) {
    hella_chan_t *c = (hella_chan_t *)handle;
    int64_t n;
    if (!c) return -1;
    hella_mu_lock(&c->mu);
    n = c->count;
    hella_mu_unlock(&c->mu);
    return n;
}

/* ── TCP sockets (B2, blocking) ──
 *
 * Minimal blocking TCP for servers: connect/listen/accept/send/recv/close
 * over IPv4 literals (`127.0.0.1`, `0.0.0.0`). Hostname DNS is future work
 * (use IP literals). All fds are int64_t so Windows SOCKET (UINT_PTR)
 * survives truncation; Hella `int` is i64. Returns -1 on error, recv
 * returns 0 on orderly close. Blocking only — a call parks its task's
 * thread (documented in std::net); non-blocking/evented I/O is B3.
 */

#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
#pragma comment(lib, "ws2_32.lib")
typedef SOCKET hella_fd_t;
static int hella_net_init(void) {
    static int done = 0;
    if (!done) {
        WSADATA wsa;
        if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return -1;
        done = 1;
    }
    return 0;
}
#else
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
typedef int hella_fd_t;
static int hella_net_init(void) { return 0; }
#endif

int64_t hella_tcp_connect(const char *ip, int64_t port) {
    struct sockaddr_in addr;
    hella_fd_t fd;
    if (!ip || port <= 0 || port > 65535) return -1;
    if (hella_net_init() != 0) return -1;
    fd =
#ifdef _WIN32
        socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
#else
        socket(AF_INET, SOCK_STREAM, 0);
#endif
#ifdef _WIN32
    if (fd == INVALID_SOCKET) return -1;
#else
    if (fd < 0) return -1;
#endif
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, ip, &addr.sin_addr) != 1) {
#ifdef _WIN32
        closesocket(fd);
#else
        close(fd);
#endif
        return -1;
    }
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
#ifdef _WIN32
        closesocket(fd);
#else
        close(fd);
#endif
        return -1;
    }
    return (int64_t)fd;
}

int64_t hella_tcp_listen(int64_t port) {
    struct sockaddr_in addr;
    hella_fd_t fd;
    int one = 1;
    if (port <= 0 || port > 65535) return -1;
    if (hella_net_init() != 0) return -1;
    fd =
#ifdef _WIN32
        socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
#else
        socket(AF_INET, SOCK_STREAM, 0);
#endif
#ifdef _WIN32
    if (fd == INVALID_SOCKET) return -1;
#else
    if (fd < 0) return -1;
#endif
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, (const char *)&one, sizeof(one));
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    addr.sin_port = htons((uint16_t)port);
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0 ||
        listen(fd, 16) != 0) {
#ifdef _WIN32
        closesocket(fd);
#else
        close(fd);
#endif
        return -1;
    }
    return (int64_t)fd;
}

int64_t hella_tcp_accept(int64_t listen_fd) {
    hella_fd_t c =
#ifdef _WIN32
        accept((SOCKET)listen_fd, NULL, NULL);
#else
        accept((int)listen_fd, NULL, NULL);
#endif
#ifdef _WIN32
    if (c == INVALID_SOCKET) return -1;
#else
    if (c < 0) return -1;
#endif
    return (int64_t)c;
}

int64_t hella_tcp_send(int64_t fd, const char *buf, int64_t n) {
    int64_t sent = 0;
    if (fd < 0 || !buf || n <= 0) return -1;
    while (sent < n) {
#ifdef _WIN32
        int r = send((SOCKET)fd, buf + sent, (int)(n - sent), 0);
        if (r == SOCKET_ERROR) return sent > 0 ? sent : -1;
#else
        ssize_t r = send((int)fd, buf + sent, (size_t)(n - sent), 0);
        if (r < 0) {
            if (errno == EINTR) continue;
            return sent > 0 ? sent : -1;
        }
#endif
        if (r == 0) break;
        sent += r;
    }
    return sent;
}

int64_t hella_tcp_recv(int64_t fd, char *buf, int64_t n) {
    if (fd < 0 || !buf || n <= 0) return -1;
#ifdef _WIN32
    {
        int r = recv((SOCKET)fd, buf, (int)n, 0);
        if (r == SOCKET_ERROR) return -1;
        return (int64_t)r;
    }
#else
    {
        ssize_t r;
        do {
            r = recv((int)fd, buf, (size_t)n, 0);
        } while (r < 0 && errno == EINTR);
        if (r < 0) return -1;
        return (int64_t)r;
    }
#endif
}

void hella_tcp_close(int64_t fd) {
    if (fd < 0) return;
#ifdef _WIN32
    closesocket((SOCKET)fd);
#else
    close((int)fd);
#endif
}

/* ── sleep / yield (also serve `std::task`) ──
 *
 * `hella_sleep_ms`/`hella_task_yield` live HERE (not in hella_async.c) so
 * `std::task::sleepMs`/`yieldNow` link from synchronous programs without
 * the task system. Only task identity/cancellation stay in the async
 * runtime. POSIX sleep retries across EINTR; Windows uses Sleep.
 */
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

/* ── wall clock (B2) ── */

int64_t hella_wall_micros(void) {
#ifdef _WIN32
    /* FILETIME: 100-ns ticks since 1601-01-01 (UTC). */
    FILETIME ft;
    ULARGE_INTEGER u;
    GetSystemTimeAsFileTime(&ft);
    u.LowPart = ft.dwLowDateTime;
    u.HighPart = ft.dwHighDateTime;
    return (int64_t)(u.QuadPart / 10 - 11644473600000000LL);
#else
    struct timeval tv;
    if (gettimeofday(&tv, NULL) != 0) return 0;
    return (int64_t)tv.tv_sec * 1000000 + (int64_t)tv.tv_usec;
#endif
}
