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

#include "build.h"
#include "arch.h"

#if defined(HAVE_SECCOMP_FILTER)

#include <sys/prctl.h>     /* prctl(2), PR_* */
#include <linux/filter.h>  /* struct sock_*, */
#include <linux/seccomp.h> /* SECCOMP_MODE_FILTER, */
#include <linux/filter.h>  /* struct sock_*, */
#include <linux/audit.h>   /* AUDIT_, */
#include <sys/queue.h>     /* LIST_FOREACH, */
#include <sys/types.h>     /* size_t, */
#include <talloc.h>        /* talloc_*, */
#include <errno.h>         /* E*, */
#include <string.h>        /* memcpy(3), */
#include <stddef.h>        /* offsetof(3), */
#include <stdint.h>        /* uint*_t, UINT*_MAX, */
#include <assert.h>        /* assert(3), */
#include <stdbool.h>       /* bool, */
#include <stdlib.h>        /* qsort(3), */
#include <endian.h>        /* __BYTE_ORDER, */
#include <termios.h>       /* TCSETS, TCGETS2, */
#include <sys/ioctl.h>     /* _IOW, */

#include "syscall/seccomp.h"
#include "tracee/tracee.h"
#include "syscall/syscall.h"
#include "syscall/sysnum.h"
#include "extension/extension.h"
#include "cli/note.h"

#include "compat.h"
#include "attribute.h"

#define DEBUG_FILTER(...) /* fprintf(stderr, __VA_ARGS__) */

/**
 * Allocate an empty @program->filter.  This function returns -errno
 * if an error occurred, otherwise 0.
 */
static int new_program_filter(struct sock_fprog *program)
{
	program->filter = talloc_array(NULL, struct sock_filter, 0);
	if (program->filter == NULL)
		return -ENOMEM;

	program->len = 0;
	return 0;
}

/**
 * Append to @program->filter the given @statements (@nb_statements
 * items).  This function returns -errno if an error occurred,
 * otherwise 0.
 */
static int add_statements(struct sock_fprog *program, size_t nb_statements,
			struct sock_filter *statements)
{
	size_t length;
	void *tmp;
	size_t i;

	length = talloc_array_length(program->filter);
	tmp  = talloc_realloc(NULL, program->filter, struct sock_filter, length + nb_statements);
	if (tmp == NULL)
		return -ENOMEM;
	program->filter = tmp;

	for (i = 0; i < nb_statements; i++, length++)
		memcpy(&program->filter[length], &statements[i], sizeof(struct sock_filter));

	return 0;
}

/**
 * Free @program->filter and set @program->len to 0.
 */
static void free_program_filter(struct sock_fprog *program)
{
	TALLOC_FREE(program->filter);
	program->len = 0;
}

#ifdef __ANDROID__
/* Only these requests are rewritten, see translate_syscall_enter() and
 * translate_syscall_exit().  Tracing every ioctl(2) cost two stops per
 * GPU (KGSL) or terminal request.  */
static const ArgumentValue ioctl_requests[] = {
	{ TCSETS + 2, 0 },
	{ TCGETS2, 0 },
	{ TCSETS2, 0 },
	{ TCSETSW2, 0 },
	{ TCSETSF2, 0 },
	{ _IOW(0x94, 9, int) /* FICLONE */, FILTER_SYSEXIT },
};
static const ArgumentFilter ioctl_filter = {
	1, sizeof(ioctl_requests) / sizeof(ArgumentValue), ioctl_requests
};
#endif

/* Only PR_SET_DUMPABLE is handled, see translate_syscall_enter().  */
static const ArgumentValue prctl_options[] = { { PR_SET_DUMPABLE, 0 } };
static const ArgumentFilter prctl_filter = { 0, 1, prctl_options };

/* A traced syscall of one architecture, as the BPF program sees it.  */
typedef struct {
	uint32_t number;
	word_t flags;
	const ArgumentFilter *filter;
} TracedSyscall;

/* Below this many syscalls, compare them one by one.  */
#define LINEAR_SEARCH 4

static int compare_traced_syscalls(const void *a, const void *b)
{
	uint32_t number_a = ((const TracedSyscall *) a)->number;
	uint32_t number_b = ((const TracedSyscall *) b)->number;
	return number_a < number_b ? -1 : number_a > number_b ? 1 : 0;
}

/**
 * Number of statements emit_search() emits for @syscalls (@nb_syscalls
 * items).
 */
static size_t search_length(const TracedSyscall *syscalls, size_t nb_syscalls)
{
	size_t length = 0;
	size_t i;

	if (nb_syscalls > LINEAR_SEARCH) {
		size_t middle = nb_syscalls / 2;
		return 2 + search_length(syscalls + middle, nb_syscalls - middle)
			+ search_length(syscalls, middle);
	}

	for (i = 0; i < nb_syscalls; i++)
		length += syscalls[i].filter == NULL ? 2 : 3 + 2 * syscalls[i].filter->nb_values;
	return length + 1;
}

/**
 * Append to @program->filter a search for the syscall number in the
 * accumulator among the sorted @syscalls (@nb_syscalls items): a binary
 * search, so a syscall that isn't traced (most of them) costs a few
 * comparisons instead of one per traced syscall.  It ends with "trace"
 * for a traced syscall, "allow" for any other.  This function returns
 * -errno if an error occurred, otherwise 0.
 */
static int emit_search(struct sock_fprog *program, const TracedSyscall *syscalls, size_t nb_syscalls)
{
	size_t i, j;
	int status;

	if (nb_syscalls > LINEAR_SEARCH) {
		size_t middle = nb_syscalls / 2;
		size_t upper_length = search_length(syscalls + middle, nb_syscalls - middle);
		struct sock_filter statements[2] = {
			/* The upper half right after, the lower half after it.  */
			BPF_JUMP(BPF_JMP + BPF_JGE + BPF_K, syscalls[middle].number, 1, 0),
			BPF_STMT(BPF_JMP + BPF_JA + BPF_K, upper_length),
		};

		status = add_statements(program, 2, statements);
		if (status < 0)
			return status;
		status = emit_search(program, syscalls + middle, nb_syscalls - middle);
		if (status < 0)
			return status;
		return emit_search(program, syscalls, middle);
	}

	for (i = 0; i < nb_syscalls; i++) {
		const ArgumentFilter *filter = syscalls[i].filter;

		if (filter == NULL) {
			struct sock_filter statements[2] = {
				BPF_JUMP(BPF_JMP + BPF_JEQ + BPF_K, syscalls[i].number, 0, 1),
				BPF_STMT(BPF_RET + BPF_K, SECCOMP_RET_TRACE + syscalls[i].flags),
			};
			DEBUG_FILTER("FILTER:     trace if syscall == %u\n", syscalls[i].number);
			status = add_statements(program, 2, statements);
			if (status < 0)
				return status;
			continue;
		}

		{
			size_t argument_offset = offsetof(struct seccomp_data, args)
				+ filter->argument * sizeof(uint64_t);
#if __BYTE_ORDER == __BIG_ENDIAN
			argument_offset += sizeof(uint32_t);
#endif
			struct sock_filter statements[2] = {
				BPF_JUMP(BPF_JMP + BPF_JEQ + BPF_K, syscalls[i].number,
					0, 1 + 2 * filter->nb_values + 1),
				BPF_STMT(BPF_LD + BPF_W + BPF_ABS, argument_offset),
			};
			status = add_statements(program, 2, statements);
			if (status < 0)
				return status;
		}

		for (j = 0; j < filter->nb_values; j++) {
			struct sock_filter statements[2] = {
				BPF_JUMP(BPF_JMP + BPF_JEQ + BPF_K, filter->values[j].value, 0, 1),
				BPF_STMT(BPF_RET + BPF_K, SECCOMP_RET_TRACE + filter->values[j].flags),
			};
			DEBUG_FILTER("FILTER:     trace if syscall == %u and argument %u == %u\n",
				syscalls[i].number, filter->argument, filter->values[j].value);
			status = add_statements(program, 2, statements);
			if (status < 0)
				return status;
		}

		{
			struct sock_filter statements[1] = {
				BPF_STMT(BPF_RET + BPF_K, SECCOMP_RET_ALLOW),
			};
			status = add_statements(program, 1, statements);
			if (status < 0)
				return status;
		}
	}

	{
		struct sock_filter statements[1] = {
			BPF_STMT(BPF_RET + BPF_K, SECCOMP_RET_ALLOW),
		};
		return add_statements(program, 1, statements);
	}
}

/**
 * Convert the given @sysnums into BPF filters according to the
 * following pseudo-code, then enabled them for the given @tracee and
 * all of its future children:
 *
 *     for each handled architectures
 *         search the syscall among the filtered ones
 *             trace (for some, only with given argument values)
 *         allow
 *     kill
 *
 * This function returns -errno if an error occurred, otherwise 0.
 */
static int set_seccomp_filters(const FilteredSysnum *sysnums)
{
	SeccompArch seccomp_archs[] = SECCOMP_ARCHS;
	size_t nb_archs = sizeof(seccomp_archs) / sizeof(SeccompArch);
	const size_t arch_offset    = offsetof(struct seccomp_data, arch);
	const size_t syscall_offset = offsetof(struct seccomp_data, nr);

	struct sock_fprog program = { .len = 0, .filter = NULL };
	TracedSyscall *syscalls = NULL;
	size_t i, j, k;
	int status;

	status = new_program_filter(&program);
	if (status < 0)
		goto end;

	for (i = 0; i < nb_archs; i++) {
		size_t nb_syscalls = 0;
		size_t nb_unique = 0;

		/* Collect this architecture's syscall numbers.  */
		for (j = 0; j < seccomp_archs[i].nb_abis; j++) {
			for (k = 0; sysnums[k].value != PR_void; k++) {
				word_t number = detranslate_sysnum(seccomp_archs[i].abis[j], sysnums[k].value);
				TracedSyscall *grown;

				if (number == SYSCALL_AVOIDER)
					continue;
				if (number > UINT32_MAX) {
					status = -ERANGE;
					goto end;
				}

				grown = talloc_realloc(NULL, syscalls, TracedSyscall, nb_syscalls + 1);
				if (grown == NULL) {
					status = -ENOMEM;
					goto end;
				}
				syscalls = grown;
				syscalls[nb_syscalls].number = number;
				syscalls[nb_syscalls].flags  = sysnums[k].flags;
				syscalls[nb_syscalls].filter = sysnums[k].filter;
				nb_syscalls++;
			}
		}

		/* Sort them, and merge duplicates: a syscall traced
		 * whatever its arguments wins over an argument filter.  */
		if (nb_syscalls > 0)
			qsort(syscalls, nb_syscalls, sizeof(TracedSyscall), compare_traced_syscalls);
		for (j = 0; j < nb_syscalls; j++) {
			if (nb_unique > 0 && syscalls[nb_unique - 1].number == syscalls[j].number) {
				syscalls[nb_unique - 1].flags |= syscalls[j].flags;
				if (syscalls[j].filter == NULL)
					syscalls[nb_unique - 1].filter = NULL;
				continue;
			}
			syscalls[nb_unique++] = syscalls[j];
		}

		{
			size_t length = search_length(syscalls, nb_unique);
			struct sock_filter statements[4] = {
				/* Load the current architecture into the
				 * accumulator.  */
				BPF_STMT(BPF_LD + BPF_W + BPF_ABS, arch_offset),

				/* If it's the expected architecture, skip
				 * the following statement.  */
				BPF_JUMP(BPF_JMP + BPF_JEQ + BPF_K, seccomp_archs[i].value, 1, 0),

				/* Otherwise jump to the end of this
				 * section.  */
				BPF_STMT(BPF_JMP + BPF_JA + BPF_K, length + 1),

				/* Load the current syscall into the
				 * accumulator.  */
				BPF_STMT(BPF_LD + BPF_W + BPF_ABS, syscall_offset),
			};

			DEBUG_FILTER("FILTER: if arch == %u, %zu syscalls\n", seccomp_archs[i].value, nb_unique);
			status = add_statements(&program, 4, statements);
			if (status < 0)
				goto end;

			status = emit_search(&program, syscalls, nb_unique);
			if (status < 0)
				goto end;
		}
	}

	{
		/* Kill anything from an unexpected architecture.  */
		struct sock_filter statements[1] = {
			BPF_STMT(BPF_RET + BPF_K, SECCOMP_RET_KILL),
		};
		status = add_statements(&program, 1, statements);
		if (status < 0)
			goto end;
	}
	program.len = talloc_array_length(program.filter);

	status = prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
	if (status < 0)
		goto end;

	/* To output this BPF program for debug purpose:
	 *
	 *     write(2, program.filter, program.len * sizeof(struct sock_filter));
	 */

	status = prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program);
	if (status < 0)
		goto end;

	status = 0;
end:
	TALLOC_FREE(syscalls);
	free_program_filter(&program);
	return status;
}

/* List of sysnums handled by PRoot.  */
static FilteredSysnum proot_sysnums[] = {
	{ PR_accept,		FILTER_SYSEXIT },
	{ PR_accept4,		FILTER_SYSEXIT },
	{ PR_access,		0 },
	{ PR_acct,		0 },
	{ PR_bind,		0 },
	{ PR_brk,		FILTER_SYSEXIT },
	{ PR_chdir,		FILTER_SYSEXIT },
	{ PR_chmod,		0 },
	{ PR_chown,		0 },
	{ PR_chown32,		0 },
	{ PR_chroot,		0 },
	{ PR_connect,		0 },
	{ PR_creat,		0 },
	{ PR_execve,		FILTER_SYSEXIT },
	{ PR_execveat,		FILTER_SYSEXIT },
	{ PR_faccessat,		0 },
	{ PR_faccessat2,	FILTER_SYSEXIT },
	{ PR_fchdir,		FILTER_SYSEXIT },
	{ PR_fchmodat,		0 },
	{ PR_fchownat,		0 },
	{ PR_fstatat64,		0 },
	{ PR_futimesat,		0 },
	{ PR_getcwd,		FILTER_SYSEXIT },
	{ PR_getpeername,	FILTER_SYSEXIT },
	{ PR_getsockname,	FILTER_SYSEXIT },
	{ PR_getxattr,		0 },
	{ PR_inotify_add_watch,	0 },
#ifdef __ANDROID__
	{ PR_ioctl,		FILTER_SYSEXIT, &ioctl_filter },
#endif
	{ PR_lchown,		0 },
	{ PR_lchown32,		0 },
	{ PR_lgetxattr,		0 },
	{ PR_link,		0 },
	{ PR_linkat,		0 },
	{ PR_listxattr,		0 },
	{ PR_llistxattr,	0 },
	{ PR_lremovexattr,	0 },
	{ PR_lsetxattr,		0 },
	{ PR_lstat,		0 },
	{ PR_lstat64,		0 },
#ifdef __ANDROID__
	{ PR_memfd_create,	0 },
#endif
	{ PR_mkdir,		0 },
	{ PR_mkdirat,		0 },
	{ PR_mknod,		0 },
	{ PR_mknodat,		0 },
	{ PR_mount,		0 },
	{ PR_name_to_handle_at,	0 },
	{ PR_newfstatat,	0 },
	{ PR_oldlstat,		0 },
	{ PR_oldstat,		0 },
	{ PR_open,		0 },
	{ PR_openat,		0 },
	{ PR_pivot_root,	0 },
	{ PR_prctl, 		0, &prctl_filter },
	{ PR_prlimit64,		FILTER_SYSEXIT },
	{ PR_ptrace,		FILTER_SYSEXIT },
	/* The exit stage only for a symlink, see translate_readlink().  */
	{ PR_readlink,		0 },
	{ PR_readlinkat,	0 },
	{ PR_removexattr,	0 },
	{ PR_rename,		FILTER_SYSEXIT },
	{ PR_renameat,		FILTER_SYSEXIT },
	{ PR_renameat2,		FILTER_SYSEXIT },
	{ PR_rmdir,		0 },
	{ PR_setrlimit,		FILTER_SYSEXIT },
	{ PR_setxattr,		0 },
	{ PR_socketcall,	FILTER_SYSEXIT },
	{ PR_stat,		0 },
	{ PR_stat64,		0 },
	{ PR_statfs,		FILTER_SYSEXIT },
	{ PR_statfs64,		FILTER_SYSEXIT },
	{ PR_statx,		FILTER_SYSEXIT },
	{ PR_swapoff,		0 },
	{ PR_swapon,		0 },
	{ PR_symlink,		0 },
	{ PR_symlinkat,		0 },
	{ PR_truncate,		0 },
	{ PR_truncate64,	0 },
	{ PR_umount,		0 },
	{ PR_umount2,		0 },
	{ PR_uname,		FILTER_SYSEXIT },
	{ PR_unlink,		0 },
	{ PR_unlinkat,		0 },
	{ PR_uselib,		0 },
	{ PR_utime,		FILTER_SYSEXIT },
	{ PR_utimensat,		0 },
	{ PR_utimes,		0 },
	{ PR_wait4,		FILTER_SYSEXIT },
	{ PR_waitpid,		FILTER_SYSEXIT },
	FILTERED_SYSNUM_END,
};

/**
 * The filter that traces what either @a or @b traces: NULL (every call)
 * unless both filter the same argument, in which case the union of their
 * values, allocated with @context.
 */
static const ArgumentFilter *merge_argument_filters(TALLOC_CTX *context,
					const ArgumentFilter *a, const ArgumentFilter *b)
{
	ArgumentFilter *merged;
	ArgumentValue *values;
	size_t i, j, n;

	if (a == NULL || b == NULL || a->argument != b->argument)
		return NULL;
	if (a == b)
		return a;

	merged = talloc_zero(context, ArgumentFilter);
	values = talloc_array(context, ArgumentValue, a->nb_values + b->nb_values);
	if (merged == NULL || values == NULL)
		return NULL;

	n = 0;
	for (i = 0; i < a->nb_values + b->nb_values; i++) {
		const ArgumentValue *value = i < a->nb_values ? &a->values[i] : &b->values[i - a->nb_values];

		for (j = 0; j < n && values[j].value != value->value; j++)
			;
		if (j < n)
			values[j].flags |= value->flags;
		else
			values[n++] = *value;
	}

	merged->argument = a->argument;
	merged->nb_values = n;
	merged->values = values;
	return merged;
}

/**
 * Add the @new_sysnums to the list of filtered @sysnums, using the
 * given Talloc @context.  This function returns -errno if an error
 * occurred, otherwise 0.
 */
static int merge_filtered_sysnums(TALLOC_CTX *context, FilteredSysnum **sysnums,
				const FilteredSysnum *new_sysnums)
{
	size_t i, j;

	assert(sysnums != NULL);

	if (*sysnums == NULL) {
		/* Start with no sysnums but the terminator.  */
		*sysnums = talloc_array(context, FilteredSysnum, 1);
		if (*sysnums == NULL)
			return -ENOMEM;

		(*sysnums)[0].value = PR_void;
	}

	for (i = 0; new_sysnums[i].value != PR_void; i++) {
		/* Search for the given sysnum.  */
		for (j = 0; (*sysnums)[j].value != PR_void
			 && (*sysnums)[j].value != new_sysnums[i].value; j++)
			;

		if ((*sysnums)[j].value == PR_void) {
			/* No such sysnum, allocate a new entry.  */
			(*sysnums) = talloc_realloc(context, (*sysnums), FilteredSysnum, j + 2);
			if ((*sysnums) == NULL)
				return -ENOMEM;

			(*sysnums)[j] = new_sysnums[i];

			/* The last item is the terminator.  */
			(*sysnums)[j + 1].value = PR_void;
		}
		else {
			/* The sysnum is already filtered, merge the
			 * flags and the argument filters.  */
			(*sysnums)[j].flags |= new_sysnums[i].flags;
			(*sysnums)[j].filter = merge_argument_filters(context,
				(*sysnums)[j].filter, new_sysnums[i].filter);
		}
	}

	return 0;
}

/**
 * Tell the kernel to trace only syscalls handled by PRoot and its
 * extensions.  This filter will be enabled for the given @tracee and
 * all of its future children.  This function returns -errno if an
 * error occurred, otherwise 0.
 */
int enable_syscall_filtering(const Tracee *tracee)
{
	FilteredSysnum *filtered_sysnums = NULL;
	Extension *extension;
	int status;

	assert(tracee != NULL && tracee->ctx != NULL);

	/* Add the sysnums required by PRoot to the list of filtered
	 * sysnums.  TODO: only if path translation is required.  */
	status = merge_filtered_sysnums(tracee->ctx, &filtered_sysnums, proot_sysnums);
	if (status < 0)
		return status;

	/* Merge the sysnums required by the extensions to the list
	 * of filtered sysnums.  */
	if (tracee->extensions != NULL) {
		LIST_FOREACH(extension, tracee->extensions, link) {
			if (extension->filtered_sysnums == NULL)
				continue;

			status = merge_filtered_sysnums(tracee->ctx, &filtered_sysnums,
							extension->filtered_sysnums);
			if (status < 0)
				return status;
		}
	}

	status = set_seccomp_filters(filtered_sysnums);
	if (status < 0)
		return status;

	return 0;
}

#else

#include "tracee/tracee.h"
#include "attribute.h"

int enable_syscall_filtering(const Tracee *tracee UNUSED)
{
	return 0;
}

#endif /* defined(HAVE_SECCOMP_FILTER) */
