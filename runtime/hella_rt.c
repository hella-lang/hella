/* hella_rt.c — tiny platform shim for the Hella standard library.
 *
 * The Hella stdlib modules declare a POSIX-flavored libc
 * surface via `extern "c"`. Most of it exists verbatim on Linux, macOS,
 * and Windows (ucrt), but a handful of symbols are missing from the
 * MSVC C runtime, which only provides underscore-prefixed variants:
 *   write    -> _write     (io.h, fd-based; used for stderr output)
 *   setenv   -> _putenv_s  (stdlib.h)
 *   unsetenv -> _putenv    ("NAME=" unsets; stdlib.h)
 *   access   -> _access    (io.h, F_OK probing)
 *   strdup   -> _strdup    (string.h)
 *
 * On POSIX systems this translation unit compiles to nothing (the libc
 * provides all five names), so the CLI only compiles and links it on
 * Windows. Call shapes mirror POSIX exactly (`int` args/returns), which
 * matches how Hella lowers `int` at the ABI boundary for small values.
 */
#ifdef _WIN32

#include <io.h>
#include <stdlib.h>
#include <string.h>

int write(int fd, const void *buf, int count) {
    return _write(fd, buf, (unsigned int)count);
}

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

int access(const char *path, int mode) {
    return _access(path, mode);
}

char *strdup(const char *s) {
    return _strdup(s);
}

#endif /* _WIN32 */
