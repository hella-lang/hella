/* hella_rt.c — tiny platform shim for the Hella standard library.
 *
 * The Hella stdlib modules declare a POSIX-flavored libc
 * surface via `extern "c"`. Most of it exists verbatim on Linux, macOS,
 * and Windows (ucrt), but `setenv`/`unsetenv` are missing from the
 * MSVC C runtime, which only provides underscore-prefixed variants:
 *   setenv   -> _putenv_s  (stdlib.h)
 *   unsetenv -> _putenv    ("NAME=" unsets; stdlib.h)
 *
 * `write`, `access`, and `strdup` are NOT shimmed here even though old
 * revisions of this file defined them: the UCRT headers declare the
 * undecorated names whenever `_CRT_INTERNAL_NONSTDC_NAMES` is enabled
 * (the default for clang targeting Windows), so local definitions clash
 * with `corecrt_io.h`/`string.h` (`conflicting types for 'write'`) and
 * break every `hella build` on Windows. Codegen already declares those
 * three as ordinary externs and the linker resolves them from the UCRT.
 *
 * On POSIX systems this translation unit compiles to nothing (the libc
 * provides both names), so the CLI only compiles and links it on
 * Windows. Call shapes mirror POSIX exactly (`int` args/returns), which
 * matches how Hella lowers `int` at the ABI boundary for small values.
 */
#ifdef _WIN32

#include <stdlib.h>
#include <string.h>

int setenv(const char *name, const char *value, int overwrite) {
    (void)overwrite;
    return _putenv_s(name, value);
}

int unsetenv(const char *name) {
    /* "_putenv(\"NAME=\")" removes NAME from the environment. */
    size_t nlen = strlen(name);
    char *Assignment = (char *)malloc(nlen + 2);
    int rc;
    if (Assignment == 0) {
        return -1;
    }
    memcpy(Assignment, name, nlen);
    Assignment[nlen] = '=';
    Assignment[nlen + 1] = '\0';
    rc = _putenv(Assignment);
    free(Assignment);
    return rc;
}

#endif /* _WIN32 */
