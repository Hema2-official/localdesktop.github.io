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

#include <assert.h>      /* assert(3), */
#include <stdint.h>      /* intptr_t, */
#include <errno.h>       /* E*, */
#include <sys/stat.h>    /* chmod(2), stat(2) */
#include <sys/types.h>   /* uid_t, gid_t, get*id(2), */
#include <unistd.h>      /* get*id(2),  */
#include <sys/ptrace.h>  /* linux.git:c0a3a20b  */
#include <linux/audit.h> /* AUDIT_ARCH_*,  */
#include <string.h>      /* memcpy(3), */
#include <stdlib.h>      /* strtol(3), */
#include <linux/auxvec.h>/* AT_,  */
#include <sys/socket.h>  /* cmsghdr, */
#include <linux/net.h>   /* SYS_SENDMSG, */

#include "extension/extension.h"
#include "syscall/syscall.h"
#include "syscall/sysnum.h"
#include "syscall/seccomp.h"
#include "syscall/chain.h"
#include "execve/execve.h"
#include "tracee/tracee.h"
#include "tracee/abi.h"
#include "tracee/mem.h"
#include "execve/auxv.h"
#include "path/binding.h"
#include "arch.h"

#include "extension/fake_id0/chown.h"
#include "extension/fake_id0/chroot.h"
#include "extension/fake_id0/getsockopt.h"
#include "extension/fake_id0/sendmsg.h"
#include "extension/fake_id0/socket.h"
#include "extension/fake_id0/stat.h"
#ifdef USERLAND
#include "extension/fake_id0/open.h"
#include "extension/fake_id0/unlink.h"
#include "extension/fake_id0/rename.h"
#include "extension/fake_id0/chmod.h"
#include "extension/fake_id0/utimensat.h"
#include "extension/fake_id0/access.h"
#include "extension/fake_id0/exec.h"
#include "extension/fake_id0/link.h"
#include "extension/fake_id0/symlink.h"
#include "extension/fake_id0/mk.h"
#include "extension/fake_id0/stat.h"
#include "extension/fake_id0/helper_functions.h"
#endif

/**
 * Copy config->@field to the tracee's memory location pointed to by @sysarg:
 * 16 bits for the legacy getres[ug]id(2) of 32-bit ABIs, 32 bits otherwise.
 */
#define POKE_MEM_ID(sysarg, field) do {					\
	Sysnum id_sysnum = get_sysnum(tracee, ORIGINAL);		\
	word_t id_address = peek_reg(tracee, ORIGINAL, sysarg);	\
	if ((id_sysnum == PR_getresuid || id_sysnum == PR_getresgid)	\
	    && (sizeof(word_t) == 4 || is_32on64_mode(tracee)))	\
		poke_uint16(tracee, id_address, config->field);	\
	else								\
		poke_uint32(tracee, id_address, config->field);	\
	if (errno != 0)							\
		return -errno;						\
} while (0)

/**
 * Emulate setuid(2) and setgid(2).
 */
#define SETXID(id, version) do {					\
	id ## _t id = peek_reg(tracee, version, SYSARG_1);		\
	bool allowed;							\
									\
	/* "EPERM: The user is not privileged (does not have the	\
	 * CAP_SETUID capability) and uid does not match the real UID	\
	 * or saved set-user-ID of the calling process." -- man		\
	 * setuid */							\
	allowed = (config->euid == 0 /* TODO: || HAS_CAP(SETUID) */	\
		|| id == config->r ## id				\
		|| id == config->e ## id				\
		|| id == config->s ## id);				\
	if (!allowed)							\
		return -EPERM;						\
									\
	/* "If the effective UID of the caller is root, the real UID	\
	 * and saved set-user-ID are also set." -- man setuid */	\
	if (config->euid == 0) {					\
		config->r ## id = id;					\
		config->s ## id = id;					\
	}								\
									\
	/* "whenever the effective user ID is changed, fsuid will also	\
	 * be changed to the new value of the effective user ID."  --	\
	 * man setfsuid */						\
	config->e ## id  = id;						\
	config->fs ## id = id;						\
									\
	poke_reg(tracee, SYSARG_RESULT, 0);				\
	return 0;							\
} while (0)

/**
 * Check whether @id is set or not.
 */
#define UNSET_ID(id) (id == (uid_t) -1)

/**
 * Check whether @id is change or not.
 */
#define UNCHANGED_ID(id) (UNSET_ID(id) || id == config->id)

/**
 * Emulate setreuid(2) and setregid(2).
 */
#define SETREXID(id, version) do {					\
	id ## _t r ## id = peek_reg(tracee, version, SYSARG_1); 	\
	id ## _t e ## id = peek_reg(tracee, version, SYSARG_2); 	\
	bool allowed;							\
									\
	/* "Unprivileged processes may only set the effective user ID	\
	 * to the real user ID, the effective user ID, or the saved	\
	 * set-user-ID.							\
	 *								\
	 * Unprivileged users may only set the real user ID to the	\
	 * real user ID or the effective user ID."			\
	 *
	 * "EPERM: The calling process is not privileged (does not	\
	 * have the CAP_SETUID) and a change other than:		\
	 * 1. swapping the effective user ID with the real user ID,	\
	 *    or;							\
	 * 2. setting one to the value of the other, or ;		\
	 * 3. setting the effective user ID to the value of the saved	\
	 *    set-user-ID						\
	 * was specified." -- man setreuid				\
	 *								\
	 * Is it possible to "ruid <- euid" and "euid <- suid" at the	\
	 * same time?  */						\
	allowed = (config->euid == 0 /* TODO: || HAS_CAP(SETUID) */	\
		|| (UNCHANGED_ID(e ## id) && UNCHANGED_ID(r ## id))	\
		|| (r ## id == config->e ## id && (e ## id == config->r ## id || UNCHANGED_ID(e ## id))) \
		|| (e ## id == config->r ## id && (r ## id == config->e ## id || UNCHANGED_ID(r ## id))) \
		|| (e ## id == config->s ## id && UNCHANGED_ID(r ## id))); \
	if (!allowed)							\
		return -EPERM;						\
									\
	/* "Supplying a value of -1 for either the real or effective	\
	 * user ID forces the system to leave that ID unchanged.	\
	 * [...]  If the real user ID is set or the effective user ID	\
	 * is set to a value not equal to the previous real user ID,	\
	 * the saved set-user-ID will be set to the new effective user	\
	 * ID." -- man setreuid */					\
	if (!UNSET_ID(e ## id)) {					\
		if (e ## id != config->r ## id)				\
			config->s ## id = e ## id;			\
									\
		config->e ## id  = e ## id;				\
		config->fs ## id = e ## id;				\
	}								\
									\
	/* Since it changes the current ruid value, this has to be	\
	 * done after euid handling. */					\
	if (!UNSET_ID(r ## id)) {					\
		if (!UNSET_ID(e ## id))					\
			config->s ## id = e ## id;			\
		config->r ## id = r ## id;				\
	}								\
									\
	poke_reg(tracee, SYSARG_RESULT, 0);				\
	return 0;							\
} while (0)

/**
 * Check if @var is equal to any config->r{@type}id's.
 */
#define EQUALS_ANY_ID(var, type)  (var == config->r ## type ## id \
				|| var == config->e ## type ## id \
				|| var == config->s ## type ## id)

/**
 * Emulate setresuid(2) and setresgid(2).
 */
#define SETRESXID(type,version) do {						\
	type ## id_t r ## type ## id = peek_reg(tracee, version, SYSARG_1);	\
	type ## id_t e ## type ## id = peek_reg(tracee, version, SYSARG_2);	\
	type ## id_t s ## type ## id = peek_reg(tracee, version, SYSARG_3);	\
	bool allowed;							\
									\
	/* "Unprivileged user processes may change the real UID,	\
	 * effective UID, and saved set-user-ID, each to one of: the	\
	 * current real UID, the current effective UID or the current	\
	 * saved set-user-ID.						\
	 *								\
	 * Privileged processes (on Linux, those having the CAP_SETUID	\
	 * capability) may set the real UID, effective UID, and saved	\
	 * set-user-ID to arbitrary values." -- man setresuid */	\
	allowed = (config->euid == 0 /* || HAS_CAP(SETUID) */		\
		|| ((UNSET_ID(r ## type ## id) || EQUALS_ANY_ID(r ## type ## id, type)) \
		 && (UNSET_ID(e ## type ## id) || EQUALS_ANY_ID(e ## type ## id, type)) \
		 && (UNSET_ID(s ## type ## id) || EQUALS_ANY_ID(s ## type ## id, type)))); \
	if (!allowed)							\
		return -EPERM;						\
									\
	/* "If one of the arguments equals -1, the corresponding value	\
	 * is not changed." -- man setresuid */				\
	if (!UNSET_ID(r ## type ## id))					\
		config->r ## type ## id = r ## type ## id;		\
									\
	if (!UNSET_ID(e ## type ## id)) {				\
		/* "the file system UID is always set to the same	\
		 * value as the (possibly new) effective UID." -- man	\
		 * setresuid */						\
		config->e ## type ## id  = e ## type ## id;		\
		config->fs ## type ## id = e ## type ## id;		\
	}								\
									\
	if (!UNSET_ID(s ## type ## id))					\
		config->s ## type ## id = s ## type ## id;		\
									\
	poke_reg(tracee, SYSARG_RESULT, 0);				\
	return 0;							\
} while (0)

/**
 * Emulate setfsuid(2) and setfsgid(2).
 */
#define SETFSXID(type) do {						\
	uid_t fs ## type ## id = peek_reg(tracee, ORIGINAL, SYSARG_1); 	\
	uid_t old_fs ## type ## id = config->fs ## type ## id;		\
	bool allowed;							\
									\
	/* "setfsuid() will succeed only if the caller is the		\
	 * superuser or if fsuid matches either the real user ID,	\
	 * effective user ID, saved set-user-ID, or the current value	\
	 * of fsuid." -- man setfsuid */				\
	allowed = (config->euid == 0 /* TODO: || HAS_CAP(SETUID) */	\
		|| fs ## type ## id == config->fs ## type ## id		\
		|| EQUALS_ANY_ID(fs ## type ## id, type));		\
	if (allowed)							\
		config->fs ## type ## id = fs ## type ## id;		\
									\
	/* "On success, the previous value of fsuid is returned.  On	\
	 * error, the current value of fsuid is returned." -- man	\
	 * setfsuid */							\
	poke_reg(tracee, SYSARG_RESULT, old_fs ## type ## id);		\
	return 0;							\
} while (0)

typedef struct {
	char *path;
	mode_t mode;
} ModifiedNode;

/* Without ownership records, set/getgroups(2) reach the kernel and
 * EPERM is turned into success at their exit; with them, they're
 * emulated at entry.  */
#ifdef USERLAND
#define GROUPS_FLAGS 0
#else
#define GROUPS_FLAGS FILTER_SYSEXIT
#endif

/* List of syscalls handled by this extensions.  */
static FilteredSysnum filtered_sysnums[] = {
#ifdef USERLAND
	/* Their entry handlers do all the work (an error they return
	 * is set at entry too), so they don't need an exit stop, which
	 * costs the tracee another round trip through PRoot.  */
	{ PR_access,		0 },
	{ PR_creat,		FILTER_SYSEXIT },
	{ PR_faccessat,		0 },
	{ PR_faccessat2,	0 },
	{ PR_link,		0 },
	{ PR_linkat,		0 },
	{ PR_mkdir,		0 },
	{ PR_mkdirat,		0 },
	{ PR_rename,		0 },
	{ PR_renameat,		0 },
	{ PR_renameat2,		0 },
	{ PR_rmdir,		0 },
	{ PR_symlink,		0 },
	{ PR_symlinkat,		0 },
	{ PR_umask,		0 },	/* emulated at entry */
	{ PR_unlink,		0 },
	{ PR_unlinkat,		0 },
	{ PR_utimensat,		0 },
#endif
	{ PR_capset,		FILTER_SYSEXIT },
	{ PR_chmod,		FILTER_SYSEXIT },
	{ PR_chown,		FILTER_SYSEXIT },
	{ PR_chown32,		FILTER_SYSEXIT },
	{ PR_chroot,		FILTER_SYSEXIT },
	{ PR_execve,		FILTER_SYSEXIT },
	{ PR_fchmod,		FILTER_SYSEXIT },
	{ PR_fchmodat,		FILTER_SYSEXIT },
	{ PR_fchown,		FILTER_SYSEXIT },
	{ PR_fchown32,		FILTER_SYSEXIT },
	{ PR_fchownat,		FILTER_SYSEXIT },
	/* fstat(2) and fstatat(2) are answered at entry (STAT_SYSCALL),
	 * PRoot asks for the exit stage when it can't.  */
	{ PR_fstat,		0 },
	{ PR_fstat64,		FILTER_SYSEXIT },
	{ PR_fstatat64,		0 },
	{ PR_getegid,		0 },
	{ PR_getegid32,		0 },
	{ PR_geteuid,		0 },
	{ PR_geteuid32,		0 },
	{ PR_getgid,		0 },
	{ PR_getgid32,		0 },
	{ PR_getgroups,		GROUPS_FLAGS },
	{ PR_getgroups32,	GROUPS_FLAGS },
	{ PR_getresgid,		0 },
	{ PR_getresgid32,	0 },
	{ PR_getresuid,		0 },
	{ PR_getresuid32,	0 },
	{ PR_getuid,		0 },
	{ PR_getuid32,		0 },
	{ PR_getsockopt,	FILTER_SYSEXIT },
	{ PR_lchown,		FILTER_SYSEXIT },
	{ PR_lchown32,		FILTER_SYSEXIT },
	{ PR_lstat,		FILTER_SYSEXIT },
	{ PR_lstat64,		FILTER_SYSEXIT },
	{ PR_mknod,		FILTER_SYSEXIT },
	{ PR_mknodat,		FILTER_SYSEXIT },
	{ PR_newfstatat,	0 },
	{ PR_oldlstat,		FILTER_SYSEXIT },
	{ PR_oldstat,		FILTER_SYSEXIT },
	{ PR_sendmsg,		0 },
	{ PR_setfsgid,		0 },
	{ PR_setfsgid32,	0 },
	{ PR_setfsuid,		0 },
	{ PR_setfsuid32,	0 },
	{ PR_setgid,		0 },
	{ PR_setgid32,		0 },
	{ PR_setgroups,		GROUPS_FLAGS },
	{ PR_setgroups32,	GROUPS_FLAGS },
	{ PR_setregid,		0 },
	{ PR_setregid32,	0 },
	{ PR_setreuid,		0 },
	{ PR_setreuid32,	0 },
	{ PR_setresgid,		0 },
	{ PR_setresgid32,	0 },
	{ PR_setresuid,		0 },
	{ PR_setresuid32,	0 },
	{ PR_setuid,		0 },
	{ PR_setuid32,		0 },
	{ PR_setxattr,		FILTER_SYSEXIT },
	{ PR_setdomainname,	FILTER_SYSEXIT },
	{ PR_sethostname,	FILTER_SYSEXIT },
	{ PR_socket,		FILTER_SYSEXIT },
	{ PR_lsetxattr,		FILTER_SYSEXIT },
	{ PR_fsetxattr,		FILTER_SYSEXIT },
	{ PR_stat,		FILTER_SYSEXIT },
	{ PR_stat64,		FILTER_SYSEXIT },
	{ PR_statfs,		FILTER_SYSEXIT },
	{ PR_statfs64,		FILTER_SYSEXIT },
	FILTERED_SYSNUM_END,
};

/**
 * Restore the @node->mode for the given @node->path.
 *
 * Note: this is a Talloc destructor.
 */
static int restore_mode(ModifiedNode *node)
{
	(void) chmod(node->path, node->mode);
	return 0;
}

/**
 * Force permissions of @path to "rwx" during the path translation of
 * current @tracee's syscall, in order to simulate CAP_DAC_OVERRIDE.
 * The original permissions are restored through talloc destructors.
 * See canonicalize() for the meaning of @is_final and for the
 * lstat(2) of @path it made (@lstat_result, NULL if that failed).
 */
static void override_permissions(const Tracee *tracee, const char *path, bool is_final,
				const struct stat *lstat_result)
{
	ModifiedNode *node;
	struct stat perms;
	mode_t new_mode;

	/* Nothing to open up if it doesn't exist, or is a directory
	 * canonicalize() knew already, which its owner can search.  A
	 * symlink's own mode doesn't matter: its target gets translated
	 * (and opened up) as a path of its own when it is followed.  */
	if (lstat_result == NULL || S_ISLNK(lstat_result->st_mode))
		return;
	perms = *lstat_result;

	/* Copy the current permissions */
	new_mode = perms.st_mode & (S_IRWXU | S_IRWXG | S_IRWXO);

	/* Add read and write permissions to everything.  */
	new_mode |= (S_IRUSR | S_IWUSR);

	/* Always add 'x' bit to directories */
	if (S_ISDIR(perms.st_mode))
		new_mode |= S_IXUSR;

	/* Patch the permissions only if needed.  */
	if (new_mode == (perms.st_mode & (S_IRWXU | S_IRWXG | S_IRWXO)))
		return;

	node = talloc_zero(tracee->ctx, ModifiedNode);
	if (node == NULL)
		return;

	if (!is_final) {
		/* Restore the previous mode of any non final components.  */
		node->mode = perms.st_mode;
	}
	else {
		switch (get_sysnum(tracee, ORIGINAL)) {
		/* For chmod syscalls: restore the new mode of the final component.  */
		case PR_chmod:
			node->mode = peek_reg(tracee, ORIGINAL, SYSARG_2);
			break;

		case PR_fchmodat:
			node->mode = peek_reg(tracee, ORIGINAL, SYSARG_3);
			break;

		/* For stat syscalls: don't touch the mode of the final component.  */
		case PR_fstatat64:
		case PR_lstat:
		case PR_lstat64:
		case PR_newfstatat:
		case PR_oldlstat:
		case PR_oldstat:
		case PR_stat:
		case PR_stat64:
		case PR_statfs:
		case PR_statfs64:
			return;

		/* Otherwise: restore the previous mode of the final component.  */
		default:
			node->mode = perms.st_mode;
			break;
		}
	}

	node->path = talloc_strdup(node, path);
	if (node->path == NULL) {
		/* Keep only consistent nodes.  */
		TALLOC_FREE(node);
		return;
	}

	/* The mode restoration works because Talloc destructors are
	 * called in reverse order.  */
	talloc_set_destructor(node, restore_mode);

	(void) chmod(path, new_mode);

	return;
}

/**
 * Adjust some ELF auxiliary vectors.  This function assumes the
 * "argv, envp, auxv" stuff is pointed to by @tracee's stack pointer,
 * as expected right after a successful call to execve(2).
 */
static int adjust_elf_auxv(Tracee *tracee, Config *config)
{
	ElfAuxVector *vectors;
	ElfAuxVector *vector;
	word_t vectors_address;

	vectors_address = get_elf_aux_vectors_address(tracee);
	if (vectors_address == 0)
		return 0;

	vectors = fetch_elf_aux_vectors(tracee, vectors_address);
	if (vectors == NULL)
		return 0;

	for (vector = vectors; vector->type != AT_NULL; vector++) {
		switch (vector->type) {
		case AT_UID:
			vector->value = config->ruid;
			break;

		case AT_EUID:
			vector->value = config->euid;
			break;

		case AT_GID:
			vector->value = config->rgid;
			break;

		case AT_EGID:
			vector->value = config->egid;
			break;

		default:
			break;
		}
	}

	push_elf_aux_vectors(tracee, vectors, vectors_address);

	return 0;
}

static int handle_perm_err_exit_end(Tracee *tracee, Config *config, bool even_if_not_root) {
	word_t result;

	/* Override only permission errors.  */
	result = peek_reg(tracee, CURRENT, SYSARG_RESULT);

#ifdef USERLAND
	/** If the call has been set to PR_void, it "succeeded" in
	 *  altering a meta file correctly.
	 */ 
	if(get_sysnum(tracee, CURRENT) == PR_getuid && (int) result != 0) 
		poke_reg(tracee, SYSARG_RESULT, 0);
	if(get_sysnum(tracee, CURRENT) == PR_void && (int) result != 0) 
		poke_reg(tracee, SYSARG_RESULT, 0);
#endif

	if ((int) result != -EPERM && (int) result != -EACCES)
		return 0;

	/* Force success if the tracee was supposed to have
	 * the capability.  */
	if (even_if_not_root || config->euid == 0) /* TODO: || HAS_CAP(...) */
		poke_reg(tracee, SYSARG_RESULT, 0);

	return 0;
}

static int handle_getresuid_exit_end(Tracee *tracee, Config *config) {
	POKE_MEM_ID(SYSARG_1, ruid);
	POKE_MEM_ID(SYSARG_2, euid);
	POKE_MEM_ID(SYSARG_3, suid);
	return 0;
}

static int handle_getresgid_exit_end(Tracee *tracee, Config *config) {
	POKE_MEM_ID(SYSARG_1, rgid);
	POKE_MEM_ID(SYSARG_2, egid);
	POKE_MEM_ID(SYSARG_3, sgid);
	return 0;
}

/**
 * Adjust current @tracee's syscall parameters according to @config.
 * This function always returns 0.
 */
static int handle_sysenter_end(Tracee *tracee, Config *config)
{
	word_t sysnum;
	Reg uid_sysarg = SYSARG_2;
	Reg gid_sysarg = SYSARG_3;

	sysnum = get_sysnum(tracee, ORIGINAL);
	switch (sysnum) {

#ifdef USERLAND
	/* handle_open(tracee, fd_sysarg, path_sysarg, flags_sysarg, mode_sysarg, config) */
	/* int openat(int dirfd, const char *pathname, int flags, mode_t mode) */
	case PR_openat:
		return handle_open_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, SYSARG_4, config);
	/* int open(const char *pathname, int flags, mode_t mode) */
	case PR_open:
		return handle_open_enter_end(tracee, IGNORE_SYSARG, SYSARG_1, SYSARG_2, SYSARG_3, config); 
	/* int creat(const char *pathname, mode_t mode) */
	case PR_creat:
		return handle_open_enter_end(tracee, IGNORE_SYSARG, SYSARG_1, IGNORE_SYSARG, SYSARG_2, config);

	/* handle_mk(tracee, fd_sysarg, path_sysarg, mode_sysarg, config) */
	/* int mkdirat(int dirfd, const char *pathname, mode_t mode) */
	case PR_mkdirat:
		return handle_mk_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, config);
	/* int mkdir(const char *pathname, mode_t mode) */
	case PR_mkdir:
		return handle_mk_enter_end(tracee, IGNORE_SYSARG, SYSARG_1, SYSARG_2, config); 

	/* handle_mk(tracee, fd_sysarg, path_sysarg, mode_sysarg, config) */
	/* int mknodat(int dirfd, const char *pathname, mode_t mode, dev_t dev); */
	case PR_mknodat:
		return handle_mk_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, config);
	/* int mknod(const char *pathname, mode_t mode, dev_t dev); */
	case PR_mknod:
		return handle_mk_enter_end(tracee, IGNORE_SYSARG, SYSARG_1, SYSARG_2, config);

	/* handle_unlink(tracee, fd_sysarg, path_sysarg, config) */
	/* int unlinkat(int dirfd, const char *pathname, int flags) */
	case PR_unlinkat:
		return handle_unlink_enter_end(tracee, SYSARG_1, SYSARG_2, config);
	/* int rmdir(const char *pathname */
	case PR_rmdir:
	/* int unlink(const char *pathname) */
	case PR_unlink:
		return handle_unlink_enter_end(tracee, IGNORE_SYSARG, SYSARG_1, config);

	/* handle_rename(tracee, oldfd_sysarg, oldpath_sysarg, newfd_sysarg, newpath_sysarg, flags_sysarg, config) */
	/* int renameat2(int olddirfd, const char *oldpath, int newdirfd, const char *newpath, unsigned int flags)
	 * The only rename syscall on arm64, so glibc's rename() and mv use it. */
	case PR_renameat2:
		return handle_rename_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, SYSARG_4, SYSARG_5, config);
	/* int renameat(int olddirfd, const char *oldpath, int newdirfd, const char *newpath) */
	case PR_renameat:
		return handle_rename_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, SYSARG_4, IGNORE_SYSARG, config);
	/* int rename(const char *oldpath, const char *newpath) */
	case PR_rename:
		return handle_rename_enter_end(tracee, IGNORE_SYSARG, SYSARG_1, IGNORE_SYSARG, SYSARG_2, IGNORE_SYSARG, config);

	/* handle_chmod(tracee, path_sysarg, mode_sysarg, fd_sysarg, dirfd_sysarg, config) */
	/* int chmod(const char *pathname, mode_t mode) */
	case PR_chmod:
		return handle_chmod_enter_end(tracee, SYSARG_1, SYSARG_2, IGNORE_SYSARG, IGNORE_SYSARG, config); 
	/* int fchmod(int fd, mode_t mode) */
	case PR_fchmod: 
		return handle_chmod_enter_end(tracee, IGNORE_SYSARG, SYSARG_2, 
			SYSARG_1, IGNORE_SYSARG, config);
	/* int fchmodat(int dirfd, const char *pathname, mode_t mode, int flags (unused)) */
	case PR_fchmodat:
		return handle_chmod_enter_end(tracee, SYSARG_2, SYSARG_3, 
			IGNORE_SYSARG, SYSARG_1, config);

	/* handle_chown(tracee, path_sysarg, owner_sysarg, group_sysarg, fd_sysarg, dirfd_sysarg, config) */
	/* int chown(const char *pathname, uid_t owner, gid_t group) */
	case PR_chown:
	case PR_chown32:
		return handle_chown_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, 
			IGNORE_SYSARG, IGNORE_SYSARG, config);
	/* int fchown(int fd, uid_t owner, gid_t group) */
	case PR_fchown:
	case PR_fchown32:
		return handle_chown_enter_end(tracee, IGNORE_SYSARG, SYSARG_2, SYSARG_3,
			SYSARG_1, IGNORE_SYSARG, config);
	/* int lchown(const char *pathname, uid_t owner, gid_t group) */
	case PR_lchown:
	case PR_lchown32:
		return handle_chown_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3,
			IGNORE_SYSARG, IGNORE_SYSARG, config);
	/* int fchownat(int dirfd, const char *pathname, uid_t owner, gid_t group, int flags (unused)) */
	case PR_fchownat:
		return handle_chown_enter_end(tracee, SYSARG_2, SYSARG_3, SYSARG_4,
			IGNORE_SYSARG, SYSARG_1, config);

	/* handle_utimensat(tracee, dirfd_sysarg, path_sysarg, times_sysarg, config) */
	/* int utimensat(int dirfd, const char *pathname, const struct timespec times[2], int flags) */
	case PR_utimensat:
		return handle_utimensat_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, config);

	/* handle_access(tracee path_sysarg, mode_sysarg, dirfd_sysarg, config) */
	/* int access(const char *pathname, int mode) */
	case PR_access: 
		return handle_access_enter_end(tracee, SYSARG_1, SYSARG_2, IGNORE_SYSARG, config);
	/* int faccessat(int dirfd, const char *pathname, int mode, int flags) */
	case PR_faccessat:
	case PR_faccessat2:
		return handle_access_enter_end(tracee, SYSARG_2, SYSARG_3, SYSARG_1, config); 

	/* handle_exec(tracee, filename_sysarg, config) */
	case PR_execve:
		return handle_exec_enter_end(tracee, SYSARG_1, config);

	/* handle_link(tracee, olddirfd_sysarg, oldpath_sysarg, newdirfd_sysarg, newpath_sysarg, config) */
	/* int link(const char *oldpath, const char *newpath) */
	case PR_link:
		return handle_link_enter_end(tracee, IGNORE_SYSARG, SYSARG_1, IGNORE_SYSARG, SYSARG_2, config);
	/* int linkat(int olddirfd, const char *oldpath, int newdirfd, const char *newpath, int flags) */
	case PR_linkat:
		return handle_link_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, SYSARG_4, config);

	/* handle_symlink(tracee, oldpath_sysarg, newdirfd_sysarg, newpath_sysarg, config) */
	/* int symlink(const char *target, const char *linkpath); */
	case PR_symlink:
		return handle_symlink_enter_end(tracee, SYSARG_1, IGNORE_SYSARG, SYSARG_2, config);
	/* int symlinkat(const char *target, int newdirfd, const char *linkpath); */
	case PR_symlinkat:
		return handle_symlink_enter_end(tracee, SYSARG_1, SYSARG_2, SYSARG_3, config);
#endif
	case PR_sendmsg:
	case PR_socketcall:
		return handle_sendmsg_enter_end(tracee, sysnum);

	/* These syscalls are fully emulated, right here: the kernel
	 * skips a voided syscall and returns what is in the result
	 * register, so they don't need an exit stop.  */
	case PR_setuid:
	case PR_setuid32:
		set_sysnum(tracee, PR_void);
		SETXID(uid, ORIGINAL);

	case PR_setgid:
	case PR_setgid32:
		set_sysnum(tracee, PR_void);
		SETXID(gid, ORIGINAL);

	case PR_setreuid:
	case PR_setreuid32:
		set_sysnum(tracee, PR_void);
		SETREXID(uid, ORIGINAL);

	case PR_setregid:
	case PR_setregid32:
		set_sysnum(tracee, PR_void);
		SETREXID(gid, ORIGINAL);

	case PR_setresuid:
	case PR_setresuid32:
		set_sysnum(tracee, PR_void);
		SETRESXID(u, ORIGINAL);

	case PR_setresgid:
	case PR_setresgid32:
		set_sysnum(tracee, PR_void);
		SETRESXID(g, ORIGINAL);

	case PR_setfsuid:
	case PR_setfsuid32:
		set_sysnum(tracee, PR_void);
		SETFSXID(u);

	case PR_setfsgid:
	case PR_setfsgid32:
		set_sysnum(tracee, PR_void);
		SETFSXID(g);

	case PR_getuid:
	case PR_getuid32:
		set_sysnum(tracee, PR_void);
		poke_reg(tracee, SYSARG_RESULT, config->ruid);
		return 0;

	case PR_getgid:
	case PR_getgid32:
		set_sysnum(tracee, PR_void);
		poke_reg(tracee, SYSARG_RESULT, config->rgid);
		return 0;

	case PR_geteuid:
	case PR_geteuid32:
		set_sysnum(tracee, PR_void);
		poke_reg(tracee, SYSARG_RESULT, config->euid);
		return 0;

	case PR_getegid:
	case PR_getegid32:
		set_sysnum(tracee, PR_void);
		poke_reg(tracee, SYSARG_RESULT, config->egid);
		return 0;

	case PR_getresuid:
	case PR_getresuid32: {
		int status;
		set_sysnum(tracee, PR_void);
		status = handle_getresuid_exit_end(tracee, config);
		if (status < 0)
			return status;
		poke_reg(tracee, SYSARG_RESULT, 0);
		return 0;
	}

	case PR_getresgid:
	case PR_getresgid32: {
		int status;
		set_sysnum(tracee, PR_void);
		status = handle_getresgid_exit_end(tracee, config);
		if (status < 0)
			return status;
		poke_reg(tracee, SYSARG_RESULT, 0);
		return 0;
	}

#ifdef USERLAND
	case PR_umask: {
		mode_t previous = config->umask;
		set_sysnum(tracee, PR_void);
		config->umask = (mode_t) peek_reg(tracee, ORIGINAL, SYSARG_1) & 0777;
		poke_reg(tracee, SYSARG_RESULT, previous);
		return 0;
	}
#endif

#ifndef USERLAND
	case PR_fchownat: 
		uid_sysarg = SYSARG_3;
		gid_sysarg = SYSARG_4;
	case PR_chown:
	case PR_chown32:
	case PR_lchown:
	case PR_lchown32:
	case PR_fchown:
	case PR_fchown32:
		return handle_chown_enter_end(tracee, config, uid_sysarg, gid_sysarg);
#endif

	case PR_setgroups:
	case PR_setgroups32:
	case PR_getgroups:
	case PR_getgroups32:
		/* TODO */
#ifdef USERLAND
	/* TODO: need to actually emulate these */
	//On Android, the system is returning gids that our rootfs knows nothing about
	//which is generating errors
	set_sysnum(tracee, PR_void);
	poke_reg(tracee, SYSARG_RESULT, 0);
	return 0;
#endif

	default:
		return 0;
	}

	/* Never reached  */
	assert(0);
	return 0;

}

/**
 * Adjust current @tracee's syscall result according to @config.  This
 * function returns -errno if an error occured, otherwise 0.
 */
static int handle_sysexit_end(Tracee *tracee, Config *config)
{
	word_t sysnum;
#ifndef USERLAND
	Reg stat_sysarg = SYSARG_2;
#endif

	sysnum = get_sysnum(tracee, ORIGINAL);

	switch (sysnum) {

	case PR_setuid:
	case PR_setuid32:
		SETXID(uid, ORIGINAL);

	case PR_setgid:
	case PR_setgid32:
		SETXID(gid, ORIGINAL);

	case PR_setreuid:
	case PR_setreuid32:
		SETREXID(uid, ORIGINAL);

	case PR_setregid:
	case PR_setregid32:
		SETREXID(gid, ORIGINAL);

	case PR_setresuid:
	case PR_setresuid32:
		SETRESXID(u, ORIGINAL);

	case PR_setresgid:
	case PR_setresgid32:
		SETRESXID(g, ORIGINAL);

	case PR_setfsuid:
	case PR_setfsuid32:
		SETFSXID(u);

	case PR_setfsgid:
	case PR_setfsgid32:
		SETFSXID(g);

	case PR_getuid:
	case PR_getuid32:
		poke_reg(tracee, SYSARG_RESULT, config->ruid);
		return 0;

	case PR_getgid:
	case PR_getgid32:
		poke_reg(tracee, SYSARG_RESULT, config->rgid);
		return 0;

	case PR_geteuid:
	case PR_geteuid32:
		poke_reg(tracee, SYSARG_RESULT, config->euid);
		return 0;

	case PR_getegid:
	case PR_getegid32:
		poke_reg(tracee, SYSARG_RESULT, config->egid);
		return 0;

	case PR_getresuid:
	case PR_getresuid32:
		return handle_getresuid_exit_end(tracee, config);

	case PR_getresgid:
	case PR_getresgid32:
		return handle_getresgid_exit_end(tracee, config);

#ifdef USERLAND
	case PR_setgroups:
	case PR_setgroups32:
	case PR_getgroups:
	case PR_getgroups32:
		/*TODO: need to really emulate*/
		poke_reg(tracee, SYSARG_RESULT, 0);
		return 0;
#endif

	case PR_setdomainname:
	case PR_sethostname:
#ifndef USERLAND
	case PR_setgroups:
	case PR_setgroups32:
#endif
	case PR_mknod:
	case PR_mknodat:
	case PR_capset:
	case PR_chmod:
	case PR_chown:
	case PR_fchmod:
	case PR_fchown:
	case PR_lchown:
	case PR_chown32:
	case PR_fchown32:
	case PR_lchown32:
	case PR_fchmodat:
	case PR_fchownat: 
		return handle_perm_err_exit_end(tracee, config, false);

	case PR_setxattr:
	case PR_lsetxattr:
	case PR_fsetxattr:
		return handle_perm_err_exit_end(tracee, config, true);

	case PR_socket: 
		return handle_socket_exit_end(tracee, config);

#ifndef USERLAND
	case PR_fstatat64:
	case PR_newfstatat:
		stat_sysarg = SYSARG_3;
	case PR_stat64:
	case PR_lstat64:
	case PR_fstat64:
	case PR_stat:
	case PR_lstat:
	case PR_fstat: 
		return handle_stat_exit_end(tracee, config, stat_sysarg);
#endif /* ifndef USERLAND */

#ifdef USERLAND
	case PR_fstatat64:
	case PR_newfstatat:
	case PR_stat64:
	case PR_lstat64:
	case PR_stat:
	case PR_lstat:
		return handle_stat_exit_end(tracee, config, sysnum);

	case PR_fstat64:
	case PR_fstat:
		return handle_fstat_exit_end(tracee, config);
#endif /* ifdef USERLAND */

	case PR_chroot: 
		return handle_chroot_exit_end(tracee, config, false);

	case PR_getsockopt:
		return handle_getsockopt_exit_end(tracee);

#ifdef USERLAND
/** Check to see if a meta was created for a file that no longer exists.
 *  If so, delete it.
 */
	case PR_open:
	case PR_openat:
	case PR_creat: {
		int status;
		Reg sysarg;
		char path[PATH_MAX];
		char meta_path[PATH_MAX];

		if(sysnum == PR_open || sysnum == PR_creat)
			sysarg = SYSARG_1;
		else
			sysarg = SYSARG_2;

		status = read_sysarg_path(tracee, path, sysarg, MODIFIED);
		if(status < 0) 
			return status;
		if(status == 1) 
			return 0;

		/* If the file exists, it doesn't matter if a metafile exists. */
		if(path_exists(path) == 0) 
			return 0; 

		status = get_meta_path(path, meta_path);
		if(status < 0) 
			return status;

		/* If the metafile exists and the original file does not, delete it. */
		if(path_exists(meta_path) == 0) 
			status = unlink_meta(meta_path);

		return 0;
	}	
#endif

	default:
		return 0;
	}
}

static int handle_sigsys(Tracee *tracee, Config *config)
{
	word_t sysnum;

	sysnum = get_sysnum(tracee, CURRENT);
	switch (sysnum) {

	case PR_setuid:
	case PR_setuid32:
		SETXID(uid, CURRENT);

	case PR_setgid:
	case PR_setgid32:
		SETXID(gid, CURRENT);

	case PR_setreuid:
	case PR_setreuid32:
		SETREXID(uid, CURRENT);

	case PR_setregid:
	case PR_setregid32:
		SETREXID(gid, CURRENT);

	case PR_setresuid:
	case PR_setresuid32:
		SETRESXID(u, CURRENT);

	case PR_setresgid:
	case PR_setresgid32:
		SETRESXID(g, CURRENT);

	case PR_chroot:
		return handle_chroot_exit_end(tracee, config, true);

	default:
		return 0;
	}
}

static int handle_sysexit_start(Tracee *tracee, Config *config) {
	word_t result = peek_reg(tracee, CURRENT, SYSARG_RESULT);
	word_t sysnum = get_sysnum(tracee, ORIGINAL);
	struct stat mode;
	int status;

	if ((int) result < 0 || tracee->status < 0 || sysnum != PR_execve)
		return 0;

	/* This has to be done before PRoot pushes the load
	 * script into tracee's stack.  */
	if (!tracee->skip_proot_loader)
		adjust_elf_auxv(tracee, config);

	status = stat(tracee->host_exe, &mode);
	if (status < 0)
		return 0; /* Not fatal.  */

	if ((mode.st_mode & S_ISUID) != 0) {
		config->euid = 0;
		config->suid = 0;
	}

	if ((mode.st_mode & S_ISGID) != 0) {
		config->egid = 0;
		config->sgid = 0;
	}

	return 0;
}

/**
 * Handler for this @extension.  It is triggered each time an @event
 * occurred.  See ExtensionEvent for the meaning of @data1 and @data2.
 */
int fake_id0_callback(Extension *extension, ExtensionEvent event, intptr_t data1, intptr_t data2)
{
	switch (event) {
	case INITIALIZATION: {
		const char *uid_string = (const char *) data1;
		const char *gid_string;
		Config *config;
		int uid, gid;

		errno = 0;
		uid = strtol(uid_string, NULL, 10);
		if (errno != 0)
			uid = getuid();

		gid_string = strchr(uid_string, ':');
		if (gid_string == NULL) {
			errno = EINVAL;
		}
		else {
			errno = 0;
			gid = strtol(gid_string + 1, NULL, 10);
		}
		/* Fallback to the current gid if an error occured.  */
		if (errno != 0)
			gid = getgid();

		extension->config = talloc(extension, Config);
		if (extension->config == NULL)
			return -1;

		config = talloc_get_type_abort(extension->config, Config);
		config->ruid  = uid;
		config->euid  = uid;
		config->suid  = uid;
		config->fsuid = uid;
		config->rgid  = gid;
		config->egid  = gid;
		config->sgid  = gid;
		config->fsgid = gid;
			/* Set the umask to the typical linux value. */
			config->umask = 022;

		extension->filtered_sysnums = filtered_sysnums;
		return 0;
	}

	case INHERIT_PARENT: /* Inheritable for sub reconfiguration ...  */
		return 1;

	case INHERIT_CHILD: {
		/* Copy the parent configuration to the child.  The
		 * structure should not be shared as uid/gid changes
		 * in one process should not affect other processes.
		 * This assertion is not true for POSIX threads
		 * sharing the same group, however Linux threads never
		 * share uid/gid information.  As a consequence, the
		 * GlibC emulates the POSIX behavior on Linux by
		 * sending a signal to all group threads to cause them
		 * to invoke the system call too.  Finally, PRoot
		 * doesn't have to worry about clone flags.
		 */

		Extension *parent = (Extension *) data1;
		extension->config = talloc_zero(extension, Config);
		if (extension->config == NULL)
			return -1;

		memcpy(extension->config, parent->config, sizeof(Config));
		return 0;
	}

	case HOST_PATH: {
		Tracee *tracee = EXTENSION_TRACEE(extension);
		Config *config = talloc_get_type_abort(extension->config, Config);
		const HostPath *info = (const HostPath *) data2;

		/* Force permissions if the tracee was supposed to
		 * have the capability.  */
		if (config->euid == 0) /* TODO: || HAS_CAP(DAC_OVERRIDE) */
			override_permissions(tracee, (char*) data1, info->is_final, info->stat);
		return 0;
	}

#ifdef USERLAND
	/** LINK2SYMLINK is an extension intended to emulate hard links on
	 *  platforms that do not have the capability to create them. In order to
	 *  retain functionality of metafiles, it's necessary to move the metafile
	 *  associated with the file being linked to the end of a symlink chain.
	 */
	case LINK2SYMLINK_RENAME: {
		int status;
		char old_meta[PATH_MAX];
		char new_meta[PATH_MAX];
	
		status = get_meta_path((char *) data1, old_meta);
		if(status < 0)
			return status;

		/* If meta doesn't exist, get out. */
		if(path_exists(old_meta) != 0)
			return 0; 

		status = get_meta_path((char *) data2, new_meta);
		if(status < 0)
			return status;

		status = rename_meta(old_meta, new_meta);
		if(status < 0)
			return status;

		return 0;
	}

	case LINK2SYMLINK_UNLINK: {
		int status;
		char meta_path[PATH_MAX];

		status = get_meta_path((char *) data1, meta_path);
		if(status < 0)
			return status;

		/* If metafile doesn't already exist, get out */
		if(path_exists(meta_path) != 0)
			return 0;

		status = unlink_meta(meta_path);
		if(status < 0) 
			return status;

		return 0;
	}
#endif

	case SYSCALL_ENTER_END: {
		Tracee *tracee = EXTENSION_TRACEE(extension);
		Config *config = talloc_get_type_abort(extension->config, Config);

		return handle_sysenter_end(tracee, config);
	}

	case SYSCALL_EXIT_END: {
		Tracee *tracee = EXTENSION_TRACEE(extension);
		Config *config = talloc_get_type_abort(extension->config, Config);

		return handle_sysexit_end(tracee, config);
	}

	case SIGSYS_OCC: {
		Tracee *tracee = EXTENSION_TRACEE(extension);
		Config *config = talloc_get_type_abort(extension->config, Config);
		word_t sysnum = get_sysnum(tracee, CURRENT);
		int status;

		switch (sysnum) {

		case PR_setuid:
		case PR_setuid32:
		case PR_setgid:
		case PR_setgid32:
		case PR_setreuid:
		case PR_setreuid32:
		case PR_setregid:
		case PR_setregid32:
		case PR_setresuid:
		case PR_setresuid32:
		case PR_setresgid:
		case PR_setresgid32:
		case PR_chroot:
			status = handle_sigsys(tracee, config);
			if (status < 0)
				return status;
			break;

		default:
			return 0;
		}

		return 1; 

	}

	case SYSCALL_EXIT_START: {
		Tracee *tracee = EXTENSION_TRACEE(extension);
		Config *config = talloc_get_type_abort(extension->config, Config);

		return handle_sysexit_start(tracee, config);
	}

	case STAT_SYSCALL: {
		Tracee *tracee = EXTENSION_TRACEE(extension);
		Config *config = talloc_get_type_abort(extension->config, Config);

		return fake_id0_handle_stat_syscall(tracee, config, data1);
	}

	case STATX_SYSCALL: {
		Tracee *tracee = EXTENSION_TRACEE(extension);
		Config *config = talloc_get_type_abort(extension->config, Config);

		return fake_id0_handle_statx_syscall(tracee, config, data1);
	}

	default:
		return 0;
	}
}

#undef POKE_MEM_ID
#undef SETXID
#undef UNSET_ID
#undef UNCHANGED_ID
#undef SETREXID
#undef EQUALS_ANY_ID
#undef SETRESXID
#undef SETFSXID
