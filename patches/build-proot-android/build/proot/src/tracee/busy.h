/* -*- c-set-style: "K&R"; c-basic-offset: 8 -*-
 *
 * This file is part of PRoot.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License as
 * published by the Free Software Foundation; either version 2 of the
 * License, or (at your option) any later version.
 */

#ifndef TRACEE_BUSY_H
#define TRACEE_BUSY_H

#include <stdbool.h>

/* Set when PROOT_BUSY_BOOST=1: PRoot then asks for the full clock
 * while it handles stops in quick succession.  */
extern bool busy_enabled;

extern void busy_init(void);
extern void busy_note_stop(void);

#endif /* TRACEE_BUSY_H */
