/* GNU/Linux subprocess-only allocator refusal; no host memory exhaustion. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <errno.h>
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <unistd.h>

extern void *__libc_malloc(size_t);
extern void *__libc_calloc(size_t, size_t);
static _Thread_local int inside;
static _Thread_local int remaining;

static int refuse(size_t size) {
    if (!inside && size >= 1024) {
        inside = 1;
        const char *path = getenv("VARLIT_ZSTD_REFUSAL_FILE");
        if (path && !remaining) {
            int fd = open(path, O_RDONLY);
            if (fd >= 0) {
                char count = '0';
                if (read(fd, &count, 1) == 1 && count >= '1' && count <= '9')
                    remaining = count - '0';
                close(fd);
            }
        }
        if (remaining && --remaining == 0) {
            unlink(path);
            inside = 0;
            errno = ENOMEM;
            return 1;
        }
        inside = 0;
    }
    return 0;
}

void *malloc(size_t size) {
    return refuse(size) ? NULL : __libc_malloc(size);
}

void *calloc(size_t count, size_t size) {
    if (size && count > SIZE_MAX / size)
        return __libc_calloc(count, size);
    return refuse(count * size) ? NULL : __libc_calloc(count, size);
}
