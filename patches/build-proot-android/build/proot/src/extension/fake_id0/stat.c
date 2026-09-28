#include <linux/limits.h>
#include <sys/types.h>   /* uid_t, gid_t, get*id(2), */
#include <unistd.h>	  /* get*id(2),  */
#include <assert.h>	  /* assert(3), */
#include <errno.h>	  /* errno, */
#include <stdio.h>	  /* snprintf(3), */
#include <string.h>	  /* strcmp(3), */

#include "tracee/mem.h"
#include "syscall/syscall.h"
#include "syscall/sysnum.h"
#include "syscall/seccomp.h"
#include "extension/fake_id0/stat.h"
#include "extension/fake_id0/helper_functions.h"
#include "tracee/statx.h"
#include "path/path.h"

#ifndef USERLAND
int handle_stat_exit_end(Tracee *tracee, Config *config, Reg stat_sysarg) {
	word_t address;
	uid_t uid;
	gid_t gid;
	word_t result;

	/* Override only if it succeed.  */
	result = peek_reg(tracee, CURRENT, SYSARG_RESULT);
	if (result != 0)
		return 0;

	address = peek_reg(tracee, ORIGINAL, stat_sysarg);

	/* Sanity checks.  */
	assert(__builtin_types_compatible_p(uid_t, uint32_t));
	assert(__builtin_types_compatible_p(gid_t, uint32_t));

	/* Get the uid & gid values from the 'stat' structure.  */
	uid = peek_uint32(tracee, address + offsetof_stat_uid(tracee));
	if (errno != 0)
		uid = 0; /* Not fatal.  */

	gid = peek_uint32(tracee, address + offsetof_stat_gid(tracee));
	if (errno != 0)
		gid = 0; /* Not fatal.  */

	/* Override only if the file is owned by the current user.
	 * Errors are not fatal here.  */
	if (uid == getuid())
		poke_uint32(tracee, address + offsetof_stat_uid(tracee), config->suid);

	if (gid == getgid())
		poke_uint32(tracee, address + offsetof_stat_gid(tracee), config->sgid);

	return 0;
}
#endif /* ifndef USERLAND */

#ifdef USERLAND
/** If there is a meta file for the host path @path, apply it to the stat
 *  structure at @address in the tracee: returns 1 if it did, 0 if there is
 *  none, -errno on failure.
 */
static int apply_meta_file(Tracee *tracee, const char path[PATH_MAX], word_t address)
{
	char meta_path[PATH_MAX];
	struct stat my_stat;
	mode_t mode;
	uid_t uid;
	gid_t gid;
	int status;

	status = get_meta_path((char *) path, meta_path);
	if (status < 0 || load_record(meta_path, &mode, &uid, &gid) < 0)
		return 0;

	/** Get the file type and sticky/set-id bits of the original
	 *  file and add them to the mode found in the meta_file.
	 */
	status = read_data(tracee, &my_stat, address, sizeof(struct stat));
	if (status < 0)
		return status;
	my_stat.st_mode = (mode & 07777) | (my_stat.st_mode & (S_IFMT | 07000));
	my_stat.st_uid = uid;
	my_stat.st_gid = gid;
	status = write_data(tracee, address, &my_stat, sizeof(struct stat));
	if (status < 0)
		return status;
	return 1;
}

/** Report files owned by the real user as owned by the emulated one.  */
static void fake_owner(Tracee *tracee, Config *config, word_t address)
{
	uid_t uid;
	gid_t gid;

	/* Sanity checks.  */
	assert(__builtin_types_compatible_p(uid_t, uint32_t));
	assert(__builtin_types_compatible_p(gid_t, uint32_t));

	/* Get the uid & gid values from the 'stat' structure.  */
	uid = peek_uint32(tracee, address + offsetof_stat_uid(tracee));
	if (errno != 0)
		uid = 0; /* Not fatal.  */

	gid = peek_uint32(tracee, address + offsetof_stat_gid(tracee));
	if (errno != 0)
		gid = 0; /* Not fatal.  */

	/* Override only if the file is owned by the current user.
	 * Errors are not fatal here.  */
	if (uid == getuid())
		poke_uint32(tracee, address + offsetof_stat_uid(tracee), config->suid);

	if (gid == getgid())
		poke_uint32(tracee, address + offsetof_stat_gid(tracee), config->sgid);
}

/** fstat(2) runs as is; at its exit, find the file behind the descriptor
 *  through /proc/<pid>/fd/<fd> and apply its meta file. This used to turn
 *  fstat into a readlink(2) of that link plus a chained stat(2) of the
 *  result: two more stops, a 4 KiB copy into the tracee, and a stat of the
 *  wrong file for anything that isn't a path (sockets, pipes, eventfds).
 */
int handle_fstat_exit_end(Tracee *tracee, Config *config)
{
	char link[64];
	char path[PATH_MAX];
	word_t address;
	ssize_t length;
	int status;

	/* Override only if it succeed.  */
	if (peek_reg(tracee, CURRENT, SYSARG_RESULT) != 0)
		return 0;

	address = peek_reg(tracee, ORIGINAL, SYSARG_2);
	snprintf(link, sizeof(link), "/proc/%d/fd/%d", tracee->pid, (int) peek_reg(tracee, ORIGINAL, SYSARG_1));
	length = readlink(link, path, sizeof(path) - 1);
	if (length > 0 && path[0] == '/') {
		const char *deleted = " (deleted)";
		size_t deleted_length = strlen(deleted);

		path[length] = '\0';
		if (!((size_t) length >= deleted_length && strcmp(path + length - deleted_length, deleted) == 0)) {
			/* Like stat(2) by name: files outside the guest
			 * (bindings) keep their real owner.  */
			if (!belongs_to_guestfs(tracee, path))
				return 0;
			status = apply_meta_file(tracee, path, address);
			if (status != 0)
				return status < 0 ? status : 0;
		}
	}

	fake_owner(tracee, config, address);
	return 0;
}

int handle_stat_exit_end(Tracee *tracee, Config *config, word_t sysnum) {
	int status = 0;
	word_t address;
	Reg sysarg;
	char path[PATH_MAX];
	word_t result;

	/* Override only if it succeed.  */
	result = peek_reg(tracee, CURRENT, SYSARG_RESULT);
	if (result != 0)
		return 0;

	/* Get the pathname of the file to be 'stat'. */
	if(sysnum == PR_fstatat64 || sysnum == PR_newfstatat)
		status = read_sysarg_path(tracee, path, SYSARG_2, MODIFIED);
	else
		status = read_sysarg_path(tracee, path, SYSARG_1, MODIFIED);

	if(status < 0)
		return status;
	if(status == 1)
		return 0;

	/* Get the address of the 'stat' structure.  */
	if (sysnum == PR_fstatat64 || sysnum == PR_newfstatat)
		sysarg = SYSARG_3;
	else
		sysarg = SYSARG_2;
	address = peek_reg(tracee, ORIGINAL, sysarg);

	/** If the meta file exists, read the data from it and replace it the
	 *  relevant data in the stat structure.
	 */
	status = apply_meta_file(tracee, path, address);
	if (status != 0)
		return status < 0 ? status : 0;

	fake_owner(tracee, config, address);
	return 0;
}

#endif /* ifdef USERLAND */

int fake_id0_handle_statx_syscall(Tracee *tracee, Config *config, uintptr_t statx_state_raw) {
	struct statx_syscall_state *state = (struct statx_syscall_state *) statx_state_raw;
#ifdef USERLAND
	/* Like stat(2): the ownership record, if there's one. statx(2) used
	 * to show only the real mode, which Android's umask for apps (077)
	 * leaves owner-only: `ls -l` and file managers showed /usr/bin/bash
	 * as -rwx------ and every file as owned by whoever looked.  */
	const char *path = state->host_path;
	size_t length = strlen(path);
	const char *deleted = " (deleted)";
	size_t deleted_length = strlen(deleted);

	if (path[0] == '/'
	    && !(length >= deleted_length && strcmp(path + length - deleted_length, deleted) == 0)
	    && belongs_to_guestfs(tracee, path)) {
		char meta_path[PATH_MAX];
		mode_t mode;
		uid_t uid;
		gid_t gid;

		if (get_meta_path((char *) path, meta_path) == 0
		    && load_record(meta_path, &mode, &uid, &gid) == 0) {
			if (state->statx_buf.stx_mask & STATX_MODE)
				state->statx_buf.stx_mode = (mode & 07777)
					| (state->statx_buf.stx_mode & (S_IFMT | 07000));
			if (state->statx_buf.stx_mask & STATX_UID)
				state->statx_buf.stx_uid = uid;
			if (state->statx_buf.stx_mask & STATX_GID)
				state->statx_buf.stx_gid = gid;
			state->updated_stats = true;
			return 0;
		}
	}
#else
	(void) tracee;
#endif
	if (state->statx_buf.stx_mask & STATX_UID) {
		if (state->statx_buf.stx_uid == getuid()) {
			state->statx_buf.stx_uid = config->suid;
			state->updated_stats = true;
		}
	}
	if (state->statx_buf.stx_mask & STATX_GID) {
		if (state->statx_buf.stx_gid == getgid()) {
			state->statx_buf.stx_gid = config->sgid;
			state->updated_stats = true;
		}
	}
	return 0;
}
