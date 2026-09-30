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

#include <linux/auxvec.h>  /* AT_*,  */
#include <assert.h>        /* assert(3),  */
#include <errno.h>         /* E*,  */
#include <unistd.h>        /* write(3), close(3), */
#include <sys/types.h>     /* open(2), */
#include <sys/stat.h>      /* open(2), */
#include <fcntl.h>         /* open(2), */
#include <stdint.h>        /* uint*_t, */
#include <string.h>        /* memcpy(3), memset(3), */

#include "execve/auxv.h"
#include "syscall/sysnum.h"
#include "tracee/tracee.h"
#include "tracee/mem.h"
#include "tracee/reg.h"
#include "tracee/abi.h"
#include "arch.h"


/**
 * Add the given vector [@type, @value] to @vectors.  This function
 * returns -errno if an error occurred, otherwise 0.
 */
int add_elf_aux_vector(ElfAuxVector **vectors, word_t type, word_t value)
{
	ElfAuxVector *tmp;
	size_t nb_vectors;

	assert(*vectors != NULL);

	nb_vectors = talloc_array_length(*vectors);

	/* Sanity checks.  */
	assert(nb_vectors > 0);
	assert((*vectors)[nb_vectors - 1].type == AT_NULL);

	tmp = talloc_realloc(talloc_parent(*vectors), *vectors, ElfAuxVector, nb_vectors + 1);
	if (tmp == NULL)
		return -ENOMEM;
	*vectors = tmp;

	/* Replace the sentinel with the new vector.  */
	(*vectors)[nb_vectors - 1].type  = type;
	(*vectors)[nb_vectors - 1].value = value;

	/* Restore the sentinel.  */
	(*vectors)[nb_vectors].type  = AT_NULL;
	(*vectors)[nb_vectors].value = 0;

	return 0;
}

/**
 * Get the address of the the ELF auxiliary vectors table for the
 * given @tracee.  This function returns 0 if an error occurred.
 */
word_t get_elf_aux_vectors_address(const Tracee *tracee)
{
	word_t words[READ_WORDS_MAX];
	word_t address;
	size_t count;
	size_t i;

	/* Sanity check: this works only in execve sysexit.  */
	assert(IS_IN_SYSEXIT2(tracee, PR_execve));

	/* Right after execve, the stack layout is:
	 *
	 *     argc, argv[0], ..., 0, envp[0], ..., 0, auxv[0].type, auxv[0].value, ..., 0, 0
	 */
	address = peek_reg(tracee, CURRENT, STACK_POINTER);

	/* Read: argc */
	if (read_words(tracee, address, words, 1) != 1)
		return 0;

	/* Skip: argc, argv, 0 */
	address += (1 + words[0] + 1) * sizeof_word(tracee);

	/* Skip: envp, 0 (a block of words at a time) */
	while (1) {
		count = read_words(tracee, address, words, READ_WORDS_MAX);
		if (count == 0)
			return 0;

		for (i = 0; i < count; i++) {
			if (words[i] == 0)
				return address + (i + 1) * sizeof_word(tracee);
		}
		address += count * sizeof_word(tracee);
	}
}

/**
 * Fetch ELF auxiliary vectors stored at the given @address in
 * @tracee's memory.  This function returns NULL if an error occurred,
 * otherwise it returns a pointer to the new vectors, in an ABI
 * independent form (the Talloc parent of this pointer is
 * @tracee->ctx).
 */
ElfAuxVector *fetch_elf_aux_vectors(const Tracee *tracee, word_t address)
{
	ElfAuxVector *vectors = NULL;
	word_t words[READ_WORDS_MAX];
	size_t count;
	size_t i;
	int status;

	/* It is assumed the sentinel always exists.  */
	vectors = talloc_array(tracee->ctx, ElfAuxVector, 1);
	if (vectors == NULL)
		return NULL;
	vectors[0].type  = AT_NULL;
	vectors[0].value = 0;

	/* A block of [type, value] pairs at a time.  */
	while (1) {
		count = read_words(tracee, address, words, READ_WORDS_MAX);
		count -= count % 2;
		if (count == 0)
			return NULL;

		for (i = 0; i < count; i += 2) {
			if (words[i] == AT_NULL)
				return vectors; /* Already added.  */

			status = add_elf_aux_vector(&vectors, words[i], words[i + 1]);
			if (status < 0)
				return NULL;
		}
		address += count * sizeof_word(tracee);
	}
}

/**
 * Push ELF auxiliary @vectors to the given @address in @tracee's
 * memory, in one write.  This function returns -errno if an error
 * occurred, otherwise 0.
 */
int push_elf_aux_vectors(Tracee *tracee, ElfAuxVector *vectors, word_t address)
{
	size_t word_size = sizeof_word(tracee);
	size_t nb_vectors;
	uint8_t *buffer;
	size_t i;
	int status;

	/* Up to the sentinel, included.  */
	for (nb_vectors = 1; vectors[nb_vectors - 1].type != AT_NULL; nb_vectors++)
		;

	buffer = talloc_size(tracee->ctx, nb_vectors * 2 * word_size);
	if (buffer == NULL)
		return -ENOMEM;

	for (i = 0; i < nb_vectors; i++) {
		word_t pair[2] = { vectors[i].type, vectors[i].value };

		if (word_size == sizeof(word_t))
			memcpy(buffer + i * 2 * word_size, pair, sizeof(pair));
		else {
			uint32_t small_pair[2] = { (uint32_t) pair[0], (uint32_t) pair[1] };
			memcpy(buffer + i * 2 * word_size, small_pair, sizeof(small_pair));
		}
	}
	/* The sentinel's value is 0, whatever the array says.  */
	memset(buffer + (nb_vectors * 2 - 1) * word_size, 0, word_size);

	status = write_data(tracee, address, buffer, nb_vectors * 2 * word_size);
	talloc_free(buffer);
	return status;
}
