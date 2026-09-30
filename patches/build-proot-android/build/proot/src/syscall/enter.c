/* -*- c-set-style: "K&R"; c-basic-offset: 8 -*-
 *
 * This file is part of PRoot.
 *
 * Copyright (C) 2015 STMicroelectronics
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License as
 * published by the Free Software Foundation; either version 2 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful, but
 * WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA
 * 02110-1301 USA.
 */

#include <errno.h>       /* errno(3), E* */
#include <talloc.h>      /* talloc_*, */
#include <sys/un.h>      /* struct sockaddr_un, */
#include <linux/net.h>   /* SYS_*, */
#include <fcntl.h>       /* AT_FDCWD, */
#include <limits.h>      /* PATH_MAX, */
#include <string.h>      /* strcpy */
#include <sys/prctl.h>   /* PR_SET_DUMPABLE */
#include <sys/ptrace.h>  /* PTRACE_SYSCALL, */
#include <sys/stat.h>    /* S_IFLNK, */
#include <sys/syscall.h> /* SYS_faccessat2, */
#include <signal.h>      /* sigaction(2), SIGSYS, */
#include <unistd.h>      /* readlink(2), */
#include <termios.h>     /* TCSETS, TCSANOW */

#include "cli/note.h"
#include "syscall/syscall.h"
#include "syscall/sysnum.h"
#include "syscall/socket.h"
#include "ptrace/ptrace.h"
#include "ptrace/wait.h"
#include "syscall/heap.h"
#include "extension/extension.h"
#include "execve/execve.h"
#include "tracee/tracee.h"
#include "tracee/reg.h"
#include "tracee/mem.h"
#include "tracee/abi.h"
#include "path/path.h"
#include "path/canon.h"
#include "path/binding.h"
#include "tracee/statx.h"
#include "arch.h"
#include "attribute.h"

/**
 * Translate @path and put the result in the @tracee's memory address
 * space pointed to by the @reg argument of the current syscall. See
 * the documentation of translate_path() about the meaning of
 * @type. This function returns -errno if an error occured, otherwise
 * 0.
 */
static int translate_path2(Tracee *tracee, int dir_fd, char path[PATH_MAX], Reg reg, Type type)
{
	char new_path[PATH_MAX];
	int status;

	/* Special case where the argument was NULL. */
	if (path[0] == '\0')
		return 0;

	/* Translate the original path. */
	status = translate_path(tracee, new_path, dir_fd, path, type != SYMLINK);
	if (status < 0)
		return status;

	return set_sysarg_path(tracee, new_path, reg);
}

/**
 * A helper, see the comment of the function above.
 */
static int translate_sysarg(Tracee *tracee, Reg reg, Type type)
{
	char old_path[PATH_MAX];
	int status;

	/* Extract the original path. */
	status = get_sysarg_path(tracee, old_path, reg);
	if (status < 0)
		return status;

	return translate_path2(tracee, AT_FDCWD, old_path, reg, type);
}

/**
 * Answer readlink(2) of the symlink at @host_path at the entry stage:
 * read it here and translate its target back, as the exit stage does
 * with what the kernel returns (see translate_syscall_exit()), into
 * the tracee's @buffer of @size bytes.  This function returns 0 if it
 * answered, -errno to answer with that error, or 1 if it can't.
 */
static int answer_readlink(Tracee *tracee, const char host_path[PATH_MAX], word_t buffer, word_t size)
{
	char referee[PATH_MAX];
	ssize_t length;
	int status;

	/* The kernel checks that first.  */
	if ((int) size <= 0)
		return -EINVAL;

	length = readlink(host_path, referee, sizeof(referee));
	if (length < 0 || (size_t) length >= sizeof(referee))
		return 1;
	referee[length] = '\0';

	status = detranslate_path(tracee, referee, host_path);
	if (status < 0)
		return 1;
	length = strlen(referee);

	/* readlink(2) truncates silently.  */
	if ((word_t) length > size)
		length = size;
	status = write_data(tracee, buffer, referee, length);
	if (status < 0)
		return status;

	set_sysnum(tracee, PR_void);
	poke_reg(tracee, SYSARG_RESULT, length);
	return 0;
}

/**
 * Translate the path of readlink(2) or readlinkat(2), @path relative
 * to @dir_fd, into the @reg argument of the current syscall.  Only a
 * symlink at the end of the path gives it something to return, which
 * translate_syscall_exit() translates back, and canonicalize() looks
 * at that last component anyway (or knows it is a directory): ask for
 * the exit stage for a symlink only, and answer anything else that is
 * there with EINVAL right away, like the kernel.  glibc's realpath(3)
 * readlinks every component of a path to find its symlinks.  This
 * function returns -errno if an error occured, otherwise 0.
 */
static int translate_readlink(Tracee *tracee, int dir_fd, char path[PATH_MAX], Reg reg)
{
	char host_path[PATH_MAX];
	int type;
	int status;

	/* An empty path: readlinkat(2) reads the descriptor's link.  */
	if (path[0] == '\0') {
		tracee->restart_how = PTRACE_SYSCALL;
		return 0;
	}

	/* A directory known already, without walking the path again.  */
	if (is_known_guest_directory(get_root(tracee), path))
		return -EINVAL;

	set_final_type_only(true);
	status = translate_path(tracee, host_path, dir_fd, path, false);
	set_final_type_only(false);
	if (status < 0)
		return status;

	/* The walk didn't end there: an extension replaced the path, like
	 * link2symlink for a fake hard link (pnpm's are, hard-linked from
	 * its store).  Look at the file itself, as the kernel would.  */
	type = final_component_type(host_path);
	if (type < 0) {
		struct stat statl;

		if (lstat(host_path, &statl) == 0)
			type = statl.st_mode & S_IFMT;
	}

	if (type > 0 && type != S_IFLNK) {
		if (type == S_IFDIR)
			remember_guest_directory(get_root(tracee), path, host_path);
		return -EINVAL;
	}

	/* A symlink in the guest's file system (pnpm's node_modules, say):
	 * the ones in /proc and in bindings keep the exit stage.  */
	if (type == S_IFLNK && belongs_to_guestfs(tracee, host_path)) {
		status = answer_readlink(tracee, host_path, peek_reg(tracee, CURRENT, reg + 1),
					peek_reg(tracee, CURRENT, reg + 2));
		if (status <= 0)
			return status;
	}

	/* A symlink, or not known.  */
	tracee->restart_how = PTRACE_SYSCALL;
	return set_sysarg_path(tracee, host_path, reg);
}

/* readlinkat(2) with this descriptor, which no process can have, asks
 * PRoot what realpath(3) returns for the path, see answer_realpath().
 * Local Desktop's realpath(3) asks this (src/guest/realpath.c in its
 * repository).  */
#define REALPATH_DIRFD (-1279545936)

/* How many symlinks glibc's realpath(3) follows.  */
#define REALPATH_MAX_LINKS 40

/**
 * Answer readlinkat(REALPATH_DIRFD, @user_path, buffer, size) with what
 * realpath(3) returns for @user_path, relative to the current directory:
 * the canonical guest path of a file that exists.  That is one stop
 * instead of glibc's readlink(2) of every component.  The answer ends
 * with a '\0', counted in the length, which no symlink's target has: it
 * tells this answer from what readlink(2) would return anywhere else.
 * Symlinks are followed, but not the fake hard links of the link2symlink
 * extension: their name is where the file is, as for any hard link.
 * Paths out of the guest rootfs (in /proc or other bindings) are left to
 * realpath(3) itself: EBADF, the kernel's answer to that descriptor,
 * tells it to walk the path the long way.  This function returns -errno
 * if an error occured, otherwise 0.
 */
static int answer_realpath(Tracee *tracee, const char user_path[PATH_MAX])
{
	word_t buffer = peek_reg(tracee, CURRENT, SYSARG_3);
	word_t size = peek_reg(tracee, CURRENT, SYSARG_4);
	char guest_path[PATH_MAX];
	char host_path[PATH_MAX];
	char link_path[PATH_MAX];
	char directory[PATH_MAX];
	char target[PATH_MAX];
	char path[PATH_MAX];
	unsigned int links;
	size_t length;
	int status;

	if (user_path[0] == '\0')
		return -ENOENT;
	strcpy(path, user_path);

	for (links = 0; ; links++) {
		struct stat statl;
		ssize_t target_length;
		bool want_directory = false;
		char *slash;
		int type;

		/* All of it resolved but its last component, unless a
		 * trailing "/" or "/." asks for a directory there.  */
		set_final_type_only(true);
		status = translate_path_with_guest(tracee, host_path, AT_FDCWD, path, false, guest_path);
		set_final_type_only(false);
		/* canonicalize() gives up on fewer nested symlinks than
		 * glibc does.  */
		if (status == -ELOOP)
			return -EBADF;
		if (status < 0)
			return status;
		if (guest_path[0] == '\0' || !belongs_to_guestfs(tracee, host_path))
			return -EBADF;

		length = strlen(guest_path);
		if (length > 1 && guest_path[length - 1] == '/') {
			guest_path[--length] = '\0';
			want_directory = true;
		}
		else if (length >= 2 && strcmp(guest_path + length - 2, "/.") == 0) {
			length = length > 2 ? length - 2 : 1;
			guest_path[length] = '\0';
			want_directory = true;
		}

		if (want_directory) {
			if (stat(host_path, &statl) < 0)
				return -errno;
			if (!S_ISDIR(statl.st_mode))
				return -ENOTDIR;
			break;
		}

		/* The link2symlink extension replaced a fake hard link with
		 * its data file, which has to be there.  */
		strcpy(link_path, guest_path);
		status = substitute_binding(tracee, GUEST, link_path);
		if (status < 0)
			return status;
		if (strcmp(link_path, host_path) != 0) {
			if (lstat(host_path, &statl) < 0)
				return -errno;
			break;
		}

		/* What canonicalize() found at the end, or the kernel's
		 * error if nothing.  */
		type = final_component_type(host_path);
		if (type <= 0) {
			if (lstat(host_path, &statl) < 0)
				return -errno;
			type = statl.st_mode & S_IFMT;
		}
		if (type != S_IFLNK)
			break;

		/* A symlink: go on from its target, which is relative to
		 * where the symlink is.  */
		if (links >= REALPATH_MAX_LINKS)
			return -ELOOP;

		target_length = readlink(host_path, target, sizeof(target));
		if (target_length < 0)
			return -errno;
		if ((size_t) target_length >= sizeof(target))
			return -ENAMETOOLONG;
		target[target_length] = '\0';

		status = detranslate_path(tracee, target, host_path);
		if (status < 0)
			return status;

		if (target[0] == '/') {
			strcpy(path, target);
			continue;
		}

		strcpy(directory, guest_path);
		slash = strrchr(directory, '/');
		slash[slash == directory ? 1 : 0] = '\0';
		status = join_paths(2, path, directory, target);
		if (status < 0)
			return status;
	}

	length = strlen(guest_path) + 1;
	if (length > size)
		return -ENAMETOOLONG;

	status = write_data(tracee, buffer, guest_path, length);
	if (status < 0)
		return status;

	set_sysnum(tracee, PR_void);
	poke_reg(tracee, SYSARG_RESULT, length);
	return 0;
}

/**
 * Translate the path of statx(2), @path relative to @dir_fd, and
 * answer it right away (see answer_statx_at_entry()).  When that isn't
 * possible, the translated path goes to the tracee and the exit stage
 * corrects the result, see handle_statx_syscall().  This function
 * returns -errno if an error occured, otherwise 0.
 */
static int translate_statx(Tracee *tracee, int dir_fd, char path[PATH_MAX])
{
	int flags = (int) peek_reg(tracee, CURRENT, SYSARG_3);
	char host_path[PATH_MAX];
	int status;

	if (path[0] != '\0') {
		status = translate_path(tracee, host_path, dir_fd, path,
					(flags & AT_SYMLINK_NOFOLLOW) == 0);
		if (status < 0)
			return status;

		status = answer_statx_at_entry(tracee, host_path);
	}
	/* An empty path: the descriptor's file (AT_EMPTY_PATH, what
	 * fstat(2) does through statx(2)), or an error.  A NULL one is
	 * left to the kernel.  */
	else if (peek_reg(tracee, CURRENT, SYSARG_2) == 0)
		status = 1;
	else if ((flags & AT_EMPTY_PATH) == 0)
		return -ENOENT;
	else
		status = answer_statx_of_descriptor_at_entry(tracee, dir_fd);

	if (status == 0) {
		set_sysnum(tracee, PR_void);
		poke_reg(tracee, SYSARG_RESULT, 0);
	}
	if (status <= 0)
		return status;

	tracee->restart_how = PTRACE_SYSCALL;
	return path[0] != '\0' ? set_sysarg_path(tracee, host_path, SYSARG_2) : 0;
}

/**
 * Void the current syscall with @status if it is 0 (answered), and
 * return it if it is an error; 1 (not answered) asks for the exit
 * stage instead, where the extensions correct the kernel's result.
 * This function returns the status for translate_syscall_enter().
 */
static int answered_or_exit_stage(Tracee *tracee, int status)
{
	if (status == 0) {
		set_sysnum(tracee, PR_void);
		poke_reg(tracee, SYSARG_RESULT, 0);
	}
	if (status <= 0)
		return status;

	tracee->restart_how = PTRACE_SYSCALL;
	return 0;
}

/**
 * Translate the path of fstatat(2) (newfstatat), @path relative to
 * @dir_fd, and answer it right away (see answer_stat_at_entry()), as
 * translate_statx() does statx(2).  This function returns -errno if an
 * error occured, otherwise 0.
 */
static int translate_stat(Tracee *tracee, int dir_fd, char path[PATH_MAX])
{
	int flags = (int) peek_reg(tracee, CURRENT, SYSARG_4);
	word_t buffer = peek_reg(tracee, CURRENT, SYSARG_3);
	char host_path[PATH_MAX];
	int status;

	if (path[0] != '\0') {
		status = translate_path(tracee, host_path, dir_fd, path,
					(flags & AT_SYMLINK_NOFOLLOW) == 0);
		if (status < 0)
			return status;

		status = answer_stat_at_entry(tracee, host_path, flags, buffer);
		if (status > 0) {
			status = set_sysarg_path(tracee, host_path, SYSARG_2);
			if (status < 0)
				return status;
			status = 1;
		}
	}
	/* An empty path: the descriptor's file (AT_EMPTY_PATH, what
	 * glibc's fstat(3) does), or an error.  A NULL one is left to
	 * the kernel.  */
	else if (peek_reg(tracee, CURRENT, SYSARG_2) == 0)
		status = 1;
	else if ((flags & AT_EMPTY_PATH) == 0)
		return -ENOENT;
	else
		status = answer_stat_of_descriptor_at_entry(tracee, dir_fd, buffer);

	return answered_or_exit_stage(tracee, status);
}

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif

static int faccessat2_trapped;

static void note_trapped_faccessat2(int signal UNUSED)
{
	faccessat2_trapped = 1;
}

/**
 * Whether the kernel has faccessat2(2), which came with Linux 5.8.  The
 * probe is guarded like statx_allowed()'s: a seccomp policy may answer
 * a system call it doesn't know with SIGSYS.
 */
static bool kernel_has_faccessat2(void)
{
	static int has = -1;

	if (has < 0) {
		struct sigaction trap;
		struct sigaction previous;
		long status;

		memset(&trap, 0, sizeof(trap));
		trap.sa_handler = note_trapped_faccessat2;
		sigemptyset(&trap.sa_mask);
		faccessat2_trapped = 0;
		sigaction(SIGSYS, &trap, &previous);
		status = syscall(SYS_faccessat2, AT_FDCWD, "/", F_OK, 0);
		sigaction(SIGSYS, &previous, NULL);
		has = status == 0 && !faccessat2_trapped;
	}
	return has;
}

/**
 * Translate faccessat2(2), whose AT_SYMLINK_NOFOLLOW leaves a symlink at
 * the end of the path alone.  Where the kernel doesn't have it, turn it
 * into faccessat(2): glibc tries faccessat2(2) first for every
 * faccessat(3), even without flags, and on ENOSYS falls back to
 * faccessat(2), or to fstatat(2) and get*id(2), so each call used to
 * cost two to four stops.  Under PRoot a process's real and effective
 * ids are the same, so AT_EACCESS changes nothing; a symlink checked
 * itself allows everything, it only has to be there.  This function
 * returns -errno if an error occured, otherwise 0.
 */
static int translate_faccessat2(Tracee *tracee)
{
	int dir_fd = peek_reg(tracee, CURRENT, SYSARG_1);
	int flags = peek_reg(tracee, CURRENT, SYSARG_4);
	bool follow = (flags & AT_SYMLINK_NOFOLLOW) == 0;
	char host_path[PATH_MAX];
	char path[PATH_MAX];
	int status;

	status = get_sysarg_path(tracee, path, SYSARG_2);
	if (status < 0)
		return status;

	if (kernel_has_faccessat2())
		return translate_path2(tracee, dir_fd, path, SYSARG_2, follow ? REGULAR : SYMLINK);

	/* What glibc says to other flags on such kernels.  */
	if ((flags & ~(AT_SYMLINK_NOFOLLOW | AT_EACCESS)) != 0)
		return -EINVAL;

	/* faccessat(2) has no AT_EMPTY_PATH either.  */
	if (path[0] == '\0')
		return peek_reg(tracee, CURRENT, SYSARG_2) == 0 ? -EFAULT : -ENOENT;

	status = translate_path(tracee, host_path, dir_fd, path, follow);
	if (status < 0)
		return status;

	/* The extensions look at the translated path, see
	 * handle_access_enter_end().  */
	status = set_sysarg_path(tracee, host_path, SYSARG_2);
	if (status < 0)
		return status;

	if (!follow && final_component_type(host_path) == S_IFLNK) {
		set_sysnum(tracee, PR_void);
		poke_reg(tracee, SYSARG_RESULT, 0);
		return 0;
	}

	set_sysnum(tracee, PR_faccessat);
	return 0;
}

/**
 * Translate the input arguments of the current @tracee's syscall in the
 * @tracee->pid process area. This function sets @tracee->status to
 * -errno if an error occured from the tracee's point-of-view (EFAULT
 * for instance), otherwise 0.
 */
int translate_syscall_enter(Tracee *tracee)
{
	int flags;
	int dirfd;
	int olddirfd;
	int newdirfd;

	int status;
	int status2;

	char path[PATH_MAX];
	char oldpath[PATH_MAX];
	char newpath[PATH_MAX];

	word_t syscall_number;
	bool special = false;

	status = notify_extensions(tracee, SYSCALL_ENTER_START, 0, 0);
	if (status < 0)
		goto end;
	if (status > 0)
		return 0;

	/* Translate input arguments. */
	syscall_number = get_sysnum(tracee, ORIGINAL);

	/* What might turn a directory into something else, or make it
	 * unsearchable, outdates the directories canonicalize() knows.  */
	switch (syscall_number) {
	case PR_rmdir:
	case PR_rename:
	case PR_renameat:
	case PR_renameat2:
	case PR_chmod:
	case PR_fchmod:
	case PR_fchmodat:
	case PR_mount:
	case PR_umount:
	case PR_umount2:
	case PR_pivot_root:
	case PR_chroot:
		invalidate_directory_cache();
		break;
	case PR_unlinkat:
		if ((peek_reg(tracee, CURRENT, SYSARG_3) & AT_REMOVEDIR) != 0)
			invalidate_directory_cache();
		break;
	default:
		break;
	}

	switch (syscall_number) {
	default:
		/* Nothing to do. */
		status = 0;
		break;

	case PR_execve:
		status = translate_execve_enter(tracee);
		break;

	case PR_execveat:
		if ((int) peek_reg(tracee, CURRENT, SYSARG_1) == AT_FDCWD) {
			set_sysnum(tracee, PR_execve);
			poke_reg(tracee, SYSARG_1, peek_reg(tracee, CURRENT, SYSARG_2));
			poke_reg(tracee, SYSARG_2, peek_reg(tracee, CURRENT, SYSARG_3));
			poke_reg(tracee, SYSARG_3, peek_reg(tracee, CURRENT, SYSARG_4));
		} else {
			note(tracee, ERROR, SYSTEM, "execveat() with non-AT_FDCWD fd is not currently supported");
			status = -ENOSYS;
			break;
		}
		status = translate_execve_enter(tracee);
		break;

	case PR_ptrace:
		status = translate_ptrace_enter(tracee);
		break;

	case PR_wait4:
	case PR_waitpid:
		status = translate_wait_enter(tracee);
		break;

	case PR_brk:
		translate_brk_enter(tracee);
		status = 0;
		break;

	case PR_getcwd:
		set_sysnum(tracee, PR_void);
		status = 0;
		break;

	case PR_fchdir:
	case PR_chdir: {
		struct stat statl;
		char *tmp;

		/* The ending "." ensures an error will be reported if
		 * path does not exist or if it is not a directory.  */
		if (syscall_number == PR_chdir) {
			status = get_sysarg_path(tracee, path, SYSARG_1);
			if (status < 0)
				break;

			status = join_paths(2, oldpath, path, ".");
			if (status < 0)
				break;

			dirfd = AT_FDCWD;
		}
		else {
			strcpy(oldpath, ".");
			dirfd = peek_reg(tracee, CURRENT, SYSARG_1);
		}

		status = translate_path(tracee, path, dirfd, oldpath, true);
		if (status < 0)
			break;

		status = lstat(path, &statl);
		if (status < 0)
			break;

		/* Check this directory is accessible.  */
		if ((statl.st_mode & S_IXUSR) == 0)
			return -EACCES;

		/* Sadly this method doesn't detranslate statefully,
		 * this means that there's an ambiguity when several
		 * bindings are from the same host path:
		 *
		 *    $ proot -m /tmp:/a -m /tmp:/b fchdir_getcwd /a
		 *    /b
		 *
		 *    $ proot -m /tmp:/b -m /tmp:/a fchdir_getcwd /a
		 *    /a
		 *
		 * A solution would be to follow each file descriptor
		 * just like it is done for cwd.
		 */

		status = detranslate_path(tracee, path, NULL);
		if (status < 0)
			break;

		/* Remove the trailing "/" or "/.".  */
		chop_finality(path);

		tmp = talloc_strdup(tracee->fs, path);
		if (tmp == NULL) {
			status = -ENOMEM;
			break;
		}
		TALLOC_FREE(tracee->fs->cwd);

		tracee->fs->cwd = tmp;
		talloc_set_name_const(tracee->fs->cwd, "$cwd");

		set_sysnum(tracee, PR_void);
		status = 0;
		break;
	}

	case PR_bind:
	case PR_connect: {
		word_t address;
		word_t size;

		address = peek_reg(tracee, CURRENT, SYSARG_2);
		size    = peek_reg(tracee, CURRENT, SYSARG_3);

		status = translate_socketcall_enter(tracee, &address, size);
		if (status <= 0)
			break;

		poke_reg(tracee, SYSARG_2, address);
		poke_reg(tracee, SYSARG_3, sizeof(struct sockaddr_un));

		status = 0;
		break;
	}

#define SYSARG_ADDR(n) (args_addr + ((n) - 1) * sizeof_word(tracee))

#define PEEK_WORD(addr, forced_errno)		\
	peek_word(tracee, addr);		\
	if (errno != 0) {			\
		status = forced_errno ?: -errno; \
		break;				\
	}

#define POKE_WORD(addr, value)			\
	poke_word(tracee, addr, value);		\
	if (errno != 0) {			\
		status = -errno;		\
		break;				\
	}

	case PR_accept:
	case PR_accept4:
		/* Nothing special to do if no sockaddr was specified.  */
		if (peek_reg(tracee, ORIGINAL, SYSARG_2) == 0) {
			status = 0;
			break;
		}
		special = true;
		/* Fall through.  */
	case PR_getsockname:
	case PR_getpeername:{
		int size;

		/* Remember: PEEK_WORD puts -errno in status and breaks if an
		 * error occured.  */
		size = (int) PEEK_WORD(peek_reg(tracee, ORIGINAL, SYSARG_3), special ? -EINVAL : 0);

		/* The "size" argument is both used as an input parameter
		 * (max. size) and as an output parameter (actual size).  The
		 * exit stage needs to know the max. size to not overwrite
		 * anything, that's why it is copied in the 6th argument
		 * (unused) before the kernel updates it.  */
		poke_reg(tracee, SYSARG_6, size);

		status = 0;
		break;
	}

	case PR_socketcall: {
		word_t args_addr;
		word_t sock_addr_saved;
		word_t sock_addr;
		word_t size_addr;
		word_t size;

		args_addr = peek_reg(tracee, CURRENT, SYSARG_2);

		switch (peek_reg(tracee, CURRENT, SYSARG_1)) {
		case SYS_BIND:
		case SYS_CONNECT:
			/* Handle these cases below.  */
			status = 1;
			break;

		case SYS_ACCEPT:
		case SYS_ACCEPT4:
			/* Nothing special to do if no sockaddr was specified.  */
			sock_addr = PEEK_WORD(SYSARG_ADDR(2), 0);
			if (sock_addr == 0) {
				status = 0;
				break;
			}
			special = true;
			/* Fall through.  */
		case SYS_GETSOCKNAME:
		case SYS_GETPEERNAME:
			/* Remember: PEEK_WORD puts -errno in status and breaks
			 * if an error occured.  */
			size_addr =  PEEK_WORD(SYSARG_ADDR(3), 0);
			size = (int) PEEK_WORD(size_addr, special ? -EINVAL : 0);

			/* See case PR_accept for explanation.  */
			poke_reg(tracee, SYSARG_6, size);
			status = 0;
			break;

		default:
			status = 0;
			break;
		}

		/* An error occured or there's nothing else to do.  */
		if (status <= 0)
			break;

		/* Remember: PEEK_WORD puts -errno in status and breaks if an
		 * error occured.  */
		sock_addr = PEEK_WORD(SYSARG_ADDR(2), 0);
		size      = PEEK_WORD(SYSARG_ADDR(3), 0);

		sock_addr_saved = sock_addr;
		status = translate_socketcall_enter(tracee, &sock_addr, size);
		if (status <= 0)
			break;

		/* These parameters are used/restored at the exit stage.  */
		poke_reg(tracee, SYSARG_5, sock_addr_saved);
		poke_reg(tracee, SYSARG_6, size);

		/* Remember: POKE_WORD puts -errno in status and breaks if an
		 * error occured.  */
		POKE_WORD(SYSARG_ADDR(2), sock_addr);
		POKE_WORD(SYSARG_ADDR(3), sizeof(struct sockaddr_un));

		status = 0;
		break;
	}

#undef SYSARG_ADDR
#undef PEEK_WORD
#undef POKE_WORD

	case PR_access:
	case PR_acct:
	case PR_chmod:
	case PR_chown:
	case PR_chown32:
	case PR_chroot:
	case PR_getxattr:
	case PR_listxattr:
	case PR_mknod:
	case PR_oldstat:
	case PR_creat:
	case PR_removexattr:
	case PR_setxattr:
	case PR_stat:
	case PR_stat64:
	case PR_statfs:
	case PR_statfs64:
	case PR_swapoff:
	case PR_swapon:
	case PR_truncate:
	case PR_truncate64:
	case PR_umount:
	case PR_umount2:
	case PR_uselib:
	case PR_utime:
	case PR_utimes:
		status = translate_sysarg(tracee, SYSARG_1, REGULAR);
		break;

	case PR_open:
		flags = peek_reg(tracee, CURRENT, SYSARG_2);

		if (   ((flags & O_NOFOLLOW) != 0)
		    || ((flags & O_EXCL) != 0 && (flags & O_CREAT) != 0))
			status = translate_sysarg(tracee, SYSARG_1, SYMLINK);
		else
			status = translate_sysarg(tracee, SYSARG_1, REGULAR);
		break;

	/* fstatat(2) is PR_fstatat64 on arm64, like fstatat64(2) on arm.  */
	case PR_fstatat64:
	case PR_newfstatat:
		dirfd = peek_reg(tracee, CURRENT, SYSARG_1);

		status = get_sysarg_path(tracee, path, SYSARG_2);
		if (status < 0)
			break;

		status = translate_stat(tracee, dirfd, path);
		break;

	case PR_fstat:
		status = answered_or_exit_stage(tracee,
			answer_stat_of_descriptor_at_entry(tracee, (int) peek_reg(tracee, CURRENT, SYSARG_1),
							peek_reg(tracee, CURRENT, SYSARG_2)));
		break;

	case PR_fchownat:
	case PR_utimensat:
	case PR_name_to_handle_at:
		dirfd = peek_reg(tracee, CURRENT, SYSARG_1);

		status = get_sysarg_path(tracee, path, SYSARG_2);
		if (status < 0)
			break;

		flags = (  syscall_number == PR_fchownat
			|| syscall_number == PR_name_to_handle_at)
			? peek_reg(tracee, CURRENT, SYSARG_5)
			: peek_reg(tracee, CURRENT, SYSARG_4);

		if ((flags & AT_SYMLINK_NOFOLLOW) != 0)
			status = translate_path2(tracee, dirfd, path, SYSARG_2, SYMLINK);
		else
			status = translate_path2(tracee, dirfd, path, SYSARG_2, REGULAR);
		break;

	case PR_faccessat2:
		status = translate_faccessat2(tracee);
		break;

	case PR_fchmodat:
	case PR_faccessat:
	case PR_futimesat:
	case PR_mknodat:
		dirfd = peek_reg(tracee, CURRENT, SYSARG_1);

		status = get_sysarg_path(tracee, path, SYSARG_2);
		if (status < 0)
			break;

		status = translate_path2(tracee, dirfd, path, SYSARG_2, REGULAR);
		break;

	case PR_inotify_add_watch:
		flags = peek_reg(tracee, CURRENT, SYSARG_3);

		if ((flags & IN_DONT_FOLLOW) != 0)
			status = translate_sysarg(tracee, SYSARG_2, SYMLINK);
		else
			status = translate_sysarg(tracee, SYSARG_2, REGULAR);
		break;

	case PR_readlink:
		status = get_sysarg_path(tracee, path, SYSARG_1);
		if (status < 0)
			break;

		status = translate_readlink(tracee, AT_FDCWD, path, SYSARG_1);
		break;

	case PR_lchown:
	case PR_lchown32:
	case PR_lgetxattr:
	case PR_llistxattr:
	case PR_lremovexattr:
	case PR_lsetxattr:
	case PR_lstat:
	case PR_lstat64:
	case PR_oldlstat:
	case PR_unlink:
	case PR_rmdir:
	case PR_mkdir:
		status = translate_sysarg(tracee, SYSARG_1, SYMLINK);
		break;

	case PR_pivot_root:
		status = translate_sysarg(tracee, SYSARG_1, REGULAR);
		if (status < 0)
			break;

		status = translate_sysarg(tracee, SYSARG_2, REGULAR);
		break;

	case PR_linkat:
		olddirfd = peek_reg(tracee, CURRENT, SYSARG_1);
		newdirfd = peek_reg(tracee, CURRENT, SYSARG_3);
		flags    = peek_reg(tracee, CURRENT, SYSARG_5);

		status = get_sysarg_path(tracee, oldpath, SYSARG_2);
		if (status < 0)
			break;

		status = get_sysarg_path(tracee, newpath, SYSARG_4);
		if (status < 0)
			break;

		if ((flags & AT_SYMLINK_FOLLOW) != 0)
			status = translate_path2(tracee, olddirfd, oldpath, SYSARG_2, REGULAR);
		else
			status = translate_path2(tracee, olddirfd, oldpath, SYSARG_2, SYMLINK);
		if (status < 0)
			break;

		status = translate_path2(tracee, newdirfd, newpath, SYSARG_4, SYMLINK);
		break;

	case PR_mount:
		status = get_sysarg_path(tracee, path, SYSARG_1);
		if (status < 0)
			break;

		/* The following check covers only 90% of the cases. */
		if (path[0] == '/' || path[0] == '.') {
			status = translate_path2(tracee, AT_FDCWD, path, SYSARG_1, REGULAR);
			if (status < 0)
				break;
		}

		status = translate_sysarg(tracee, SYSARG_2, REGULAR);
		break;

	case PR_openat:
		dirfd = peek_reg(tracee, CURRENT, SYSARG_1);
		flags = peek_reg(tracee, CURRENT, SYSARG_3);

		status = get_sysarg_path(tracee, path, SYSARG_2);
		if (status < 0)
			break;

		if (   ((flags & O_NOFOLLOW) != 0)
			|| ((flags & O_EXCL) != 0 && (flags & O_CREAT) != 0))
			status = translate_path2(tracee, dirfd, path, SYSARG_2, SYMLINK);
		else
			status = translate_path2(tracee, dirfd, path, SYSARG_2, REGULAR);
		break;

	case PR_readlinkat:
		dirfd = peek_reg(tracee, CURRENT, SYSARG_1);

		status = get_sysarg_path(tracee, path, SYSARG_2);
		if (status < 0)
			break;

		if (dirfd == REALPATH_DIRFD)
			status = answer_realpath(tracee, path);
		else
			status = translate_readlink(tracee, dirfd, path, SYSARG_2);
		break;

	case PR_unlinkat:
	case PR_mkdirat:
		dirfd = peek_reg(tracee, CURRENT, SYSARG_1);

		status = get_sysarg_path(tracee, path, SYSARG_2);
		if (status < 0)
			break;

		status = translate_path2(tracee, dirfd, path, SYSARG_2, SYMLINK);
		break;

	case PR_link:
	case PR_rename:
		status = translate_sysarg(tracee, SYSARG_1, SYMLINK);
		if (status < 0)
			break;

		status = translate_sysarg(tracee, SYSARG_2, SYMLINK);
		break;

	case PR_renameat:
	case PR_renameat2:
		olddirfd = peek_reg(tracee, CURRENT, SYSARG_1);
		newdirfd = peek_reg(tracee, CURRENT, SYSARG_3);

		status = get_sysarg_path(tracee, oldpath, SYSARG_2);
		if (status < 0)
			break;

		status = get_sysarg_path(tracee, newpath, SYSARG_4);
		if (status < 0)
			break;

		status = translate_path2(tracee, olddirfd, oldpath, SYSARG_2, SYMLINK);
		if (status < 0)
			break;

		status = translate_path2(tracee, newdirfd, newpath, SYSARG_4, SYMLINK);
		break;

	case PR_symlink:
		status = translate_sysarg(tracee, SYSARG_2, SYMLINK);
		break;

	case PR_symlinkat:
		newdirfd = peek_reg(tracee, CURRENT, SYSARG_2);

		status = get_sysarg_path(tracee, newpath, SYSARG_3);
		if (status < 0)
			break;

		status = translate_path2(tracee, newdirfd, newpath, SYSARG_3, SYMLINK);
		break;

	case PR_statx:
		newdirfd = peek_reg(tracee, CURRENT, SYSARG_1);

		status = get_sysarg_path(tracee, newpath, SYSARG_2);
		if (status < 0)
			break;

		status = translate_statx(tracee, newdirfd, newpath);
		break;

	case PR_prctl:
		/* Prevent tracees from setting dumpable flag.
		 * (Otherwise it could break tracee memory access)  */
		if (peek_reg(tracee, CURRENT, SYSARG_1) == PR_SET_DUMPABLE) {
			set_sysnum(tracee, PR_void);
			status = 0;
		}
		break;

#ifdef __ANDROID__
	case PR_ioctl:
		/* Using literal value because Termux build system patches TCSAFLUSH */
		if (peek_reg(tracee, CURRENT, SYSARG_2) == TCSETS + 2 /* + TCSAFLUSH */) {
			poke_reg(tracee, SYSARG_2, TCSETS + TCSANOW);
		}

		if (peek_reg(tracee, CURRENT, SYSARG_2) == TCGETS2) {
			poke_reg(tracee, SYSARG_2, TCGETS);
		}

		if (peek_reg(tracee, CURRENT, SYSARG_2) == TCSETS2) {
			poke_reg(tracee, SYSARG_2, TCSETS);
		}

		if (peek_reg(tracee, CURRENT, SYSARG_2) == TCSETSW2) {
			poke_reg(tracee, SYSARG_2, TCSETSW);
		}

		if (peek_reg(tracee, CURRENT, SYSARG_2) == TCSETSF2) {
			poke_reg(tracee, SYSARG_2, TCSETSF);
		}

		break;
#endif
	
	case PR_memfd_create:
		{
			char memfd_name[20] = {};
			if (read_string(tracee, memfd_name, peek_reg(tracee, CURRENT, SYSARG_1), sizeof(memfd_name) - 1) < 0) {
				/* Failed to read memfd name, do nothing and let normal memfd proceed.  */
				break;
			}
			/* If this memfd is one of those used by Qt/QML for executable code,
			 * deny memfd_create() call and let Qt fall back to anonymous mmap.  */
			if (0 == strncmp(memfd_name, "JITCode:", 8)) {
				status = -EACCES;
			}
			/* php8.3 attempts using memfd as lock through fcntl(F_SETLKW),
			 * which is not allowed on Android,
			 * deny memfd_create() call and let php fall back to open(O_TMPFILE).
			 * https://github.com/php/php-src/blob/26c432d850c153aaf79a1b24e4753bc0533e02b0/ext/opcache/zend_shared_alloc.c#L91
			 */
			if (0 == strcmp(memfd_name, "opcache_lock")) {
				status = -EACCES;
			}
			/* apk-tools v3 use memfd_create + execveat, which is not supported under PRoot
			 * https://github.com/termux/proot-distro/issues/595#issuecomment-3705344471
			 * https://git.alpinelinux.org/apk-tools/tree/src/package.c?h=v3.0.3#n737
			 */
			if (0 == strncmp(memfd_name, "lib/apk/exec/", 13)) {
				status = -EACCES;
			}
			break;
		}
	}


end:
	status2 = notify_extensions(tracee, SYSCALL_ENTER_END, status, 0);
	if (status2 < 0)
		status = status2;

	return status;
}

