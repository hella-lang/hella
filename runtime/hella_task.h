/* hella_task.h -- shared task-state foundation for the Hella runtimes.
 *
 * Included by BOTH runtime/hella_sync.c and runtime/hella_async.c (the CLI
 * writes this header next to the generated .c files before compiling them,
 * since the runtimes are embedded via include_str!).
 *
 * Split rationale: `std::task::cancelled()` must link from synchronous
 * programs (where it is always false — correct, as no task runs there), so
 * task identity (TLS slot) + cooperative cancellation (flag under the task
 * mutex) live in the SYNC runtime. Thread lifecycle (spawn/join/result,
 * trampoline) stays in the ASYNC runtime; its trampoline publishes the
 * running task through the TLS slot defined here. The CLI always links the
 * sync runtime alongside the async one, so the slot exists whenever tasks
 * can run.
 */

#ifndef HELLA_TASK_H
#define HELLA_TASK_H

#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
typedef HANDLE hella_thread_t;
typedef CRITICAL_SECTION hella_mutex_t;
typedef CONDITION_VARIABLE hella_cond_t;
static int hella_thr_create(hella_thread_t *out, void *(*fn)(void *), void *arg) {
    HANDLE h = CreateThread(NULL, 0, (LPTHREAD_START_ROUTINE)(void *)fn, arg, 0, NULL);
    if (h == NULL) return -1;
    *out = h;
    return 0;
}
static int hella_thr_join(hella_thread_t t) {
    DWORD r = WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
    return (r == WAIT_OBJECT_0) ? 0 : -1;
}
static void hella_mu_init(hella_mutex_t *m) { InitializeCriticalSection(m); }
static void hella_mu_lock(hella_mutex_t *m) { EnterCriticalSection(m); }
static void hella_mu_unlock(hella_mutex_t *m) { LeaveCriticalSection(m); }
static void hella_mu_destroy(hella_mutex_t *m) { DeleteCriticalSection(m); }
static void hella_cv_init(hella_cond_t *c) { InitializeConditionVariable(c); }
static void hella_cv_wait(hella_cond_t *c, hella_mutex_t *m) { SleepConditionVariableCS(c, m, INFINITE); }
static void hella_cv_signal(hella_cond_t *c) { WakeConditionVariable(c); }
static void hella_cv_broadcast(hella_cond_t *c) { WakeAllConditionVariable(c); }
#define HELLA_TLS __declspec(thread)
#else
#include <pthread.h>
typedef pthread_t hella_thread_t;
typedef pthread_mutex_t hella_mutex_t;
typedef pthread_cond_t hella_cond_t;
static int hella_thr_create(hella_thread_t *out, void *(*fn)(void *), void *arg) { return pthread_create(out, NULL, fn, arg); }
static int hella_thr_join(hella_thread_t t) { return pthread_join(t, NULL); }
static void hella_mu_init(hella_mutex_t *m) { pthread_mutex_init(m, NULL); }
static void hella_mu_lock(hella_mutex_t *m) { pthread_mutex_lock(m); }
static void hella_mu_unlock(hella_mutex_t *m) { pthread_mutex_unlock(m); }
static void hella_mu_destroy(hella_mutex_t *m) { pthread_mutex_destroy(m); }
static void hella_cv_init(hella_cond_t *c) { pthread_cond_init(c, NULL); }
static void hella_cv_wait(hella_cond_t *c, hella_mutex_t *m) { pthread_cond_wait(c, m); }
static void hella_cv_signal(hella_cond_t *c) { pthread_cond_signal(c); }
static void hella_cv_broadcast(hella_cond_t *c) { pthread_cond_broadcast(c); }
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

/* Task running on THIS thread (TLS), or NULL outside a task. DEFINED in
 * hella_sync.c (always linked with the async runtime); the async
 * trampoline publishes through this declaration. */
extern HELLA_TLS hella_task_t *hella_current_task;

#endif /* HELLA_TASK_H */
