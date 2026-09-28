/* Time the syscalls proot intercepts, in ns per call, to compare proot builds and options with
 * each other and with running natively. Build it static so it runs outside the rootfs too:
 *
 *     gcc -O2 -static -o syscall-bench scripts/guest/syscall-bench.c
 *
 * Usage: syscall-bench FILE DIR [SCALE [NAME]]
 *   FILE  an existing regular file (its path is stat'ed and opened), DIR a directory with a few
 *         hundred entries (listed); SCALE multiplies the iteration counts (default 1); NAME runs
 *         only the benchmarks whose name starts with it.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static double now(void)
{
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return time.tv_sec * 1e9 + time.tv_nsec;
}

#define BENCH(name, count, body)                                                     \
    do {                                                                             \
        if (only != NULL && strncmp(name, only, strlen(only)) != 0)                  \
            break;                                                                   \
        long n = (long) (count) * scale;                                             \
        double start = now();                                                        \
        for (long i = 0; i < n; i++) {                                               \
            body;                                                                    \
        }                                                                            \
        printf("%-16s %9.0f ns\n", name, (now() - start) / n);                       \
        fflush(stdout);                                                              \
    } while (0)

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s FILE DIR [SCALE]\n", argv[0]);
        return 2;
    }
    const char *file = argv[1];
    const char *dir = argv[2];
    long scale = argc > 3 ? atol(argv[3]) : 1;
    const char *only = argc > 4 ? argv[4] : NULL;
    struct stat st;
    int fd = open(file, O_RDONLY);
    int pipes[2];
    char byte;
    if (fd < 0 || pipe(pipes) < 0) {
        perror(file);
        return 1;
    }

    BENCH("getpid", 200000, syscall(SYS_getpid));
    BENCH("read(pipe,0)", 200000, (void) read(pipes[0], &byte, 0));
    BENCH("getuid", 20000, syscall(SYS_getuid));
    BENCH("fstat", 20000, fstat(fd, &st));
    BENCH("stat", 20000, stat(file, &st));
    BENCH("lstat", 20000, lstat(file, &st));
    BENCH("stat(missing)", 20000, stat("/nonexistent/path/x", &st));
    BENCH("statx", 20000, { struct statx stx; statx(AT_FDCWD, file, 0, STATX_BASIC_STATS, &stx); });
    BENCH("access", 20000, access(file, R_OK));
    BENCH("open+close", 20000, close(open(file, O_RDONLY)));
    BENCH("readlink", 20000, { char buf[256]; (void) readlink("/proc/self/exe", buf, sizeof(buf)); });
    BENCH("ioctl(TCGETS)", 20000, { struct termios t; (void) ioctl(pipes[0], TCGETS, &t); });
    BENCH("ioctl(FIONREAD)", 20000, { int n; (void) ioctl(pipes[0], FIONREAD, &n); });
    BENCH("brk", 20000, syscall(SYS_brk, 0));
    BENCH("opendir+list", 200, {
        DIR *d = opendir(dir);
        while (d && readdir(d))
            ;
        if (d)
            closedir(d);
    });
    BENCH("fork+wait", 200, {
        pid_t pid = fork();
        if (pid == 0)
            _exit(0);
        waitpid(pid, NULL, 0);
    });
    return 0;
}
