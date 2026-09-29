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

#ifndef SECCOMP_H
#define SECCOMP_H

#include <stdint.h>

#include "syscall/sysnum.h"
#include "tracee/tracee.h"
#include "attribute.h"
#include "arch.h"

/* One value of an ArgumentFilter, with the flags for syscalls that
 * have it.  */
typedef struct {
	uint32_t value;
	word_t flags;
} ArgumentValue;

/* Trace a syscall only when one of its arguments has one of a few
 * values; with any other value it goes straight to the kernel.  The
 * argument is compared as 32 bits, like the int the kernel uses for
 * ioctl(2)'s request.  */
typedef struct {
	unsigned int argument;
	size_t nb_values;
	const ArgumentValue *values;
} ArgumentFilter;

typedef struct {
	Sysnum value;
	word_t flags;

	/* NULL to trace every call, otherwise only these.  */
	const ArgumentFilter *filter;
} FilteredSysnum;

typedef struct {
	unsigned int value;
	size_t nb_abis;
	Abi abis[NB_MAX_ABIS];
} SeccompArch;

#define FILTERED_SYSNUM_END { PR_void, 0, NULL }

#define FILTER_SYSEXIT  0x1

extern int enable_syscall_filtering(const Tracee *tracee);

#endif /* SECCOMP_H */
