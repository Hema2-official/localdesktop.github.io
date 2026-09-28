/* -*- c-set-style: "K&R"; c-basic-offset: 8 -*-
 *
 * This file is part of PRoot.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License as
 * published by the Free Software Foundation; either version 2 of the
 * License, or (at your option) any later version.
 */

#ifndef TRACEE_PROFILE_H
#define TRACEE_PROFILE_H

#include <stdbool.h>

#include "tracee/tracee.h"

/* Set when PROOT_PROFILE names a file: every tracee stop is then
 * counted, per syscall, with the time PRoot spent handling it.  */
extern bool profile_enabled;

/* Set when PROOT_PROFILE_PATHS=1 as well: every path translated is
 * logged with the program and syscall.  */
extern bool profile_paths_enabled;

extern void profile_init(void);
extern void profile_path(const Tracee *tracee, const char *path);
extern void profile_stop_begin(void);
extern void profile_stop_end(const Tracee *tracee, int tracee_status, bool was_sysenter);
extern void profile_dump(void);

#endif /* TRACEE_PROFILE_H */
