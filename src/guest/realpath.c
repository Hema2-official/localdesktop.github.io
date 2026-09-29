/* realpath(3) for the Linux programs, in one question to proot.
 *
 * glibc's realpath(3) readlink(2)s every component of a path to find its symlinks, and under
 * proot every one of those calls stops the process while proot translates the path: Vite's dev
 * server resolves each module this way, about eight stops per file. proot resolves paths anyway,
 * so this library, preloaded through /etc/ld.so.preload (see setup_fast_realpath() in
 * src/android/proot/setup.rs), asks it for the whole answer with one readlinkat(2) on a
 * descriptor no process can have (see answer_realpath() in proot's src/syscall/enter.c).
 *
 * Only proot's answer ends with a '\0', which no symlink's target has. Anywhere else the question
 * reads like readlink(2) of the path (the kernel and other proots ignore the descriptor for an
 * absolute path, a sandbox may broker it), and glibc's realpath(3) walks the path itself, as it
 * does where proot says EBADF: paths in /proc or other bindings.
 *
 * Built by scripts/build-guest-libs.sh into assets/guest/.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define EXPORT __attribute__((visibility("default")))

/* The same in proot's src/syscall/enter.c.  */
#define REALPATH_DIRFD (-1279545936)

extern void __chk_fail(void) __attribute__((noreturn));

/* 1 if proot answers in this process, -1 if not, 0 before the first question.  */
static int proot_answers;

static char *(*glibc_realpath)(const char *path, char *resolved);

/* Ask proot for realpath(3) of @path: 1 if it answered into @buffer, 0 if glibc's realpath(3) has
 * to do it, -1 for an error (in errno) realpath(3) would return too, wherever it came from.
 * Rarer errors go to glibc, which counts symlinks and path lengths its own way.  */
static int ask_proot(const char *path, char buffer[PATH_MAX])
{
	long length = syscall(SYS_readlinkat, REALPATH_DIRFD, path, buffer, PATH_MAX);

	if (length > 0)
		return buffer[length - 1] == '\0';
	if (length == 0)
		return 0;

	switch (errno) {
	case ENOENT:
	case ENOTDIR:
	case EACCES:
		return -1;
	default:
		return 0;
	}
}

/* Whether proot answers here, found out once per process: otherwise every realpath(3) would
 * make one more system call, and under another proot one more stop.  */
static int proot_answers_here(void)
{
	int answers = __atomic_load_n(&proot_answers, __ATOMIC_RELAXED);

	if (answers == 0) {
		char buffer[PATH_MAX];

		answers = ask_proot("/", buffer) == 1 ? 1 : -1;
		__atomic_store_n(&proot_answers, answers, __ATOMIC_RELAXED);
	}
	return answers > 0;
}

static char *fast_realpath(const char *path, char *resolved)
{
	char buffer[PATH_MAX];
	int saved_errno = errno;

	if (path != NULL && proot_answers_here()) {
		switch (ask_proot(path, buffer)) {
		case 1:
			errno = saved_errno;
			return resolved != NULL ? strcpy(resolved, buffer) : strdup(buffer);
		case -1:
			return NULL;
		default:
			break;
		}
	}
	errno = saved_errno;

	if (glibc_realpath == NULL) {
		glibc_realpath = (char *(*)(const char *, char *)) dlsym(RTLD_NEXT, "realpath");
		if (glibc_realpath == NULL) {
			errno = ENOSYS;
			return NULL;
		}
	}
	return glibc_realpath(path, resolved);
}

EXPORT char *realpath(const char *restrict path, char *restrict resolved)
{
	return fast_realpath(path, resolved);
}

EXPORT char *canonicalize_file_name(const char *path)
{
	return fast_realpath(path, NULL);
}

/* realpath(3) with a buffer of known size, what _FORTIFY_SOURCE turns calls into.  */
EXPORT char *__realpath_chk(const char *path, char *resolved, size_t resolved_length)
{
	if (resolved_length < PATH_MAX)
		__chk_fail();
	return fast_realpath(path, resolved);
}
