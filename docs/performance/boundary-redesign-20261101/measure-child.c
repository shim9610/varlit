#define _GNU_SOURCE
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>
#include <stdio.h>
#include <errno.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    if (argc < 3) return 125;
    pid_t pid = fork();
    if (pid < 0) return 125;
    if (!pid) { execv(argv[2], argv + 2); _exit(127); }
    int status;
    struct rusage usage;
    while (wait4(pid, &status, 0, &usage) < 0) if (errno != EINTR) return 125;
    FILE *out = fopen(argv[1], "w");
    if (!out) return 125;
    fprintf(out, "{\"cpu\":%.9f,\"rss\":%ld,\"write_blocks\":%ld}\n",
        usage.ru_utime.tv_sec + usage.ru_stime.tv_sec +
        (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) / 1000000.0,
        usage.ru_maxrss, usage.ru_oublock);
    if (fclose(out)) return 125;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}
