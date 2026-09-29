#ifndef STATX_H
#define STATX_H

#include "tracee/tracee.h"
#include "sys/vfs.h"
#include <sys/stat.h>
#include "path/path.h"
#include "extension/extension.h"

/*
 * This structure is passed to extensions
 * for STATX_SYSCALL event
 */
struct statx_syscall_state {
	/* Host path to statx()'d file */
	char host_path[PATH_MAX];

	/* This is statx structure that will be returned
	 * Extensions can fill additional data in it
	 *
	 * After changing data there set updated_stats to true
	 */
	struct statx statx_buf;

	/* Flag indicating that statx_buf was changed
	 * and needs to be copied back to tracee
	 */
	bool updated_stats;
};

/*
 * What STAT_SYSCALL passes in "data1": stat(2), lstat(2), fstatat(2) or
 * fstat(2) answered at the entry stage, see answer_stat_at_entry().
 */
struct stat_syscall_state {
	/* Host path of the file; for a descriptor, from its link in /proc,
	 * which isn't always a path.  */
	char host_path[PATH_MAX];

	/* Of a descriptor: fstat(2), or an empty path with AT_EMPTY_PATH.  */
	bool of_descriptor;

	/* The result, for the extensions to correct.  */
	struct stat stat_buf;
};

int handle_statx_syscall(Tracee *tracee, bool from_sigsys);
int answer_stat_at_entry(Tracee *tracee, const char host_path[PATH_MAX], int flags, word_t buffer);
int answer_stat_of_descriptor_at_entry(Tracee *tracee, int fd, word_t buffer);
int answer_statx_at_entry(Tracee *tracee, const char host_path[PATH_MAX]);
int answer_statx_of_descriptor_at_entry(Tracee *tracee, int dir_fd);


#endif // STATX_H
