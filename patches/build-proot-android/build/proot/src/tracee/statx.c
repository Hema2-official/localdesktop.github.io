#include <errno.h>          /* E*, */
#include <fcntl.h>          /* AT_FDCWD, */
#include <signal.h>         /* sigaction(2), SIGSYS, */
#include <stdio.h>          /* snprintf(3), */
#include <string.h>         /* strcpy(3), */
#include <unistd.h>         /* syscall(2), */
#include <sys/syscall.h>    /* SYS_statx, */
#include <sys/sysmacros.h>  /* major, minor, */

#include "tracee/statx.h"
#include "tracee/mem.h"
#include "tracee/abi.h"
#include "attribute.h"

static volatile sig_atomic_t statx_trapped;

static void note_trapped_syscall(int signal UNUSED)
{
	statx_trapped = 1;
}

/**
 * Whether PRoot may call statx(2) itself: the seccomp policy for apps
 * traps it on Android versions before 11 (handle_statx_syscall() then
 * emulates it for the tracees).
 */
static bool statx_allowed(void)
{
	static int allowed = -1;

	if (allowed < 0) {
		struct sigaction trap;
		struct sigaction previous;
		struct statx buffer;
		long status;

		memset(&trap, 0, sizeof(trap));
		trap.sa_handler = note_trapped_syscall;
		sigemptyset(&trap.sa_mask);
		statx_trapped = 0;
		sigaction(SIGSYS, &trap, &previous);
		status = syscall(SYS_statx, AT_FDCWD, "/", 0, STATX_BASIC_STATS, &buffer);
		sigaction(SIGSYS, &previous, NULL);
		allowed = status == 0 && !statx_trapped;
	}
	return allowed;
}

static int answer_statx(Tracee *tracee, const char *path, const char *host_path, int flags);

/**
 * Answer the @tracee's statx(2) of @host_path, the translation of its
 * path, at the entry stage: call statx(2) here, let the extensions
 * correct the result (ownership records, fake hard links) and write it
 * to the tracee's buffer.  That saves the exit stage, and copying the
 * translated path to the tracee.  This function returns 0 if it
 * answered, -errno to answer with that error, or 1 if it can't answer.
 */
int answer_statx_at_entry(Tracee *tracee, const char host_path[PATH_MAX])
{
	return answer_statx(tracee, host_path, host_path, (int) peek_reg(tracee, CURRENT, SYSARG_3));
}

/**
 * answer_statx_at_entry() for statx(2) of the descriptor @dir_fd (an
 * empty path with AT_EMPTY_PATH), or of the working directory for
 * AT_FDCWD: through its link in /proc, like fstat(2).
 */
int answer_statx_of_descriptor_at_entry(Tracee *tracee, int dir_fd)
{
	char host_path[PATH_MAX];
	char link[64];
	ssize_t length;
	int flags;

	if (dir_fd == AT_FDCWD)
		snprintf(link, sizeof(link), "/proc/%d/cwd", tracee->pid);
	else
		snprintf(link, sizeof(link), "/proc/%d/fd/%d", tracee->pid, dir_fd);

	/* The file it refers to, for the extensions.  Not a descriptor of
	 * the tracee's: the kernel answers.  */
	length = readlink(link, host_path, sizeof(host_path) - 1);
	if (length < 0)
		return 1;
	host_path[length] = '\0';

	flags = (int) peek_reg(tracee, CURRENT, SYSARG_3) & ~(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW);
	return answer_statx(tracee, link, host_path, flags);
}

/**
 * statx(2) @path with @flags for the tracee, and let the extensions
 * correct the result for the file at @host_path.  See
 * answer_statx_at_entry() for the returned value.
 */
static int answer_statx(Tracee *tracee, const char *path, const char *host_path, int flags)
{
	struct statx_syscall_state state = {};
	unsigned int mask = (unsigned int) peek_reg(tracee, CURRENT, SYSARG_4);
	word_t buffer = peek_reg(tracee, CURRENT, SYSARG_5);
	int status;

	if (!statx_allowed())
		return 1;

	strcpy(state.host_path, host_path);
	if (syscall(SYS_statx, AT_FDCWD, path, flags, mask, &state.statx_buf) < 0)
		return -errno;

	state.updated_stats = true;
	status = notify_extensions(tracee, STATX_SYSCALL, (intptr_t) &state, 0);
	if (status < 0)
		return status;

	return write_data(tracee, buffer, &state.statx_buf, sizeof(state.statx_buf));
}

/**
 * Answer the @tracee's stat(2)-like syscall at the entry stage, like
 * answer_statx_at_entry() does statx(2): stat @path with @flags here,
 * let the extensions correct the result for the file at @host_path
 * (STAT_SYSCALL: ownership records, fake hard links) and write it to
 * @buffer in the tracee.  This function returns 0 if it answered,
 * -errno to answer with that error, or 1 if it can't answer.
 */
static int answer_stat(Tracee *tracee, const char *path, const char *host_path,
		bool of_descriptor, int flags, word_t buffer)
{
	struct stat_syscall_state state = {};
	int status;

	/* Their struct stat is another one.  */
	if (is_32on64_mode(tracee))
		return 1;

	strcpy(state.host_path, host_path);
	state.of_descriptor = of_descriptor;
	if (fstatat(AT_FDCWD, path, &state.stat_buf, flags) < 0)
		return -errno;

	status = notify_extensions(tracee, STAT_SYSCALL, (intptr_t) &state, 0);
	if (status < 0)
		return status;

	return write_data(tracee, buffer, &state.stat_buf, sizeof(state.stat_buf));
}

/**
 * answer_stat() for @host_path, the translation of the tracee's path.
 */
int answer_stat_at_entry(Tracee *tracee, const char host_path[PATH_MAX], int flags, word_t buffer)
{
	return answer_stat(tracee, host_path, host_path, false, flags, buffer);
}

/**
 * answer_stat() for the descriptor @fd (fstat(2), or an empty path with
 * AT_EMPTY_PATH), or the working directory for AT_FDCWD: through its
 * link in /proc.
 */
int answer_stat_of_descriptor_at_entry(Tracee *tracee, int fd, word_t buffer)
{
	char host_path[PATH_MAX];
	char link[64];
	ssize_t length;

	if (fd == AT_FDCWD)
		snprintf(link, sizeof(link), "/proc/%d/cwd", tracee->pid);
	else
		snprintf(link, sizeof(link), "/proc/%d/fd/%d", tracee->pid, fd);

	/* Not a descriptor of the tracee's: the kernel answers.  */
	length = readlink(link, host_path, sizeof(host_path) - 1);
	if (length < 0)
		return 1;
	host_path[length] = '\0';

	return answer_stat(tracee, link, host_path, true, 0, buffer);
}

int handle_statx_syscall(Tracee *tracee, bool from_sigsys) {
	RegVersion regVersion = from_sigsys ? CURRENT : ORIGINAL;
	struct statx_syscall_state state = {};
	char guest_path[PATH_MAX] = {};
	struct stat stat_buf = {};
	bool do_fstat = false;

	/* Read arguments and translate path */
	word_t flags = peek_reg(tracee, regVersion, SYSARG_3);
	bool do_lstat = ((flags & AT_SYMLINK_NOFOLLOW) != 0);
	word_t mask = peek_reg(tracee, regVersion, SYSARG_4);
	int status = read_string(tracee, guest_path, peek_reg(tracee, regVersion, SYSARG_2), PATH_MAX);
	if (status < 0) {
		return status;
	}

	word_t dirfd = peek_reg(tracee, regVersion, SYSARG_1);
	if (status == 0) {
		return -EFAULT;
	}
	if (status == 1) {
		if ((flags & AT_EMPTY_PATH) == 0) {
			return -ENOENT;
		}
		status = readlink_proc_pid_fd(tracee->pid, dirfd, state.host_path);
		do_fstat = true;
	} else if (!from_sigsys) {
		/* The entry stage already translated the path and put it in
		 * the tracee's memory: read it back instead of translating
		 * it again, which cost as much as the whole entry stage.  */
		status = read_string(tracee, state.host_path, peek_reg(tracee, MODIFIED, SYSARG_2), PATH_MAX);
		if (status >= PATH_MAX)
			return -ENAMETOOLONG;
		if (status > 0)
			status = 0;
		else if (status == 0)
			status = -EFAULT;
	} else {
		if (status >= PATH_MAX) {
			return -ENAMETOOLONG;
		}
		status = translate_path(tracee, state.host_path, dirfd, guest_path, !do_lstat);
	}
	if (status < 0) {
		return status;
	}

	if (from_sigsys || peek_reg(tracee, CURRENT, SYSARG_RESULT) != 0) {
		/* Call [l]stat() on translated path */
		if (do_fstat) {
			char link[32] = {}; /* 32 > sizeof("/proc//cwd") + sizeof(#ULONG_MAX) */
			snprintf(link, sizeof(link), "/proc/%d/fd/%d", tracee->pid, (int) dirfd);
			status = stat(link, &stat_buf);
		} else if (do_lstat) {
			status = lstat(state.host_path, &stat_buf);
		} else {
			status = stat(state.host_path, &stat_buf);
		}
		if (status < 0) {
			status = -errno;
			if (status >= 0) status = -EPERM;
			return status;
		}

		/* Translate results from stat to statx */
		state.statx_buf.stx_mask = (
			mask & (
				STATX_TYPE |
				STATX_MODE |
				STATX_NLINK |
				STATX_UID |
				STATX_GID |
				STATX_ATIME |
				STATX_MTIME |
				STATX_CTIME |
				STATX_INO |
				STATX_SIZE |
				STATX_BLOCKS |
				STATX_BTIME
			)
		);
		state.statx_buf.stx_blksize = stat_buf.st_blksize;
		if (mask & (STATX_TYPE | STATX_MODE)) {
			state.statx_buf.stx_mode = stat_buf.st_mode;
		}
		if (mask & STATX_NLINK) {
			state.statx_buf.stx_nlink = stat_buf.st_nlink;
		}
		if (mask & STATX_UID) {
			state.statx_buf.stx_uid = stat_buf.st_uid;
		}
		if (mask & STATX_GID) {
			state.statx_buf.stx_gid = stat_buf.st_gid;
		}
		if (mask & STATX_ATIME) {
			state.statx_buf.stx_atime.tv_sec = stat_buf.st_atim.tv_sec;
			state.statx_buf.stx_atime.tv_nsec = stat_buf.st_atim.tv_nsec;
		}
		if (mask & STATX_MTIME) {
			state.statx_buf.stx_mtime.tv_sec = stat_buf.st_mtim.tv_sec;
			state.statx_buf.stx_mtime.tv_nsec = stat_buf.st_mtim.tv_nsec;
		}
		if (mask & STATX_CTIME) {
			state.statx_buf.stx_ctime.tv_sec = stat_buf.st_ctim.tv_sec;
			state.statx_buf.stx_ctime.tv_nsec = stat_buf.st_ctim.tv_nsec;
		}
		if (mask & STATX_INO) {
			state.statx_buf.stx_ino = stat_buf.st_ino;
		}
		if (mask & STATX_SIZE) {
			state.statx_buf.stx_size = stat_buf.st_size;
		}
		if (mask & STATX_BLOCKS) {
			state.statx_buf.stx_blocks = stat_buf.st_blocks;
		}
		if (mask & STATX_BTIME) {
			// stat() doesn't expose this, take ctime
			state.statx_buf.stx_btime.tv_sec = stat_buf.st_ctim.tv_sec;
			state.statx_buf.stx_btime.tv_nsec = stat_buf.st_ctim.tv_nsec;
		}
		state.statx_buf.stx_rdev_major = major(stat_buf.st_rdev);
		state.statx_buf.stx_rdev_minor = minor(stat_buf.st_rdev);
		state.updated_stats = true;
	} else {
		status = read_data(tracee, &state.statx_buf, peek_reg(tracee, ORIGINAL, SYSARG_5), sizeof(struct statx));
		if (status < 0) {
			return status;
		}
	}

	/* Notify extensions */
	status = notify_extensions(tracee, STATX_SYSCALL, (intptr_t) &state, 0);
	if (status < 0) {
		return status;
	}

	/* Return results to tracee */
	if (state.updated_stats) {
		status = write_data(tracee, peek_reg(tracee, CURRENT, SYSARG_5), &state.statx_buf, sizeof(state.statx_buf));
		if (status < 0) {
			return status;
		}
	}
	return 0;
}
