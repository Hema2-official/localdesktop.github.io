/* -*- c-set-style: "K&R"; c-basic-offset: 8 -*-
 *
 * This file is part of PRoot.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License as
 * published by the Free Software Foundation; either version 2 of the
 * License, or (at your option) any later version.
 */

/* Every stop of a tracee waits for PRoot, so how fast PRoot runs
 * decides how fast a program that stops often runs.  PRoot is busy in
 * short bursts, and the scheduler rates such a thread low: on a phone
 * it runs it at a fraction of the clock, or on a slower core.  A floor
 * on the utilization (uclamp.min) changes that, but one set for good
 * also applies to every burst of a quiet desktop.
 *
 * So PRoot raises its own floor to the maximum while it is busy, and
 * lowers it back to what it started with once it calms down; the
 * tracees keep theirs.  On a Snapdragon 888, against a floor of 512
 * for everything: `ls -lR` of 20,000 files 2.8 -> 1.75 s, extracting
 * a package of 3,227 files 6.4 -> 4.9 s, Python stat'ing, resolving
 * and reading 2,948 files 1.45 -> 1.25 s.  */

#include <stdbool.h>    /* bool, */
#include <stdint.h>     /* uint*_t, */
#include <stdlib.h>     /* getenv(3), */
#include <string.h>     /* strcmp(3), */
#include <sys/syscall.h> /* SYS_sched_*attr, */
#include <time.h>       /* clock_gettime(2), */
#include <unistd.h>     /* syscall(2), */

#include "tracee/busy.h"

bool busy_enabled = false;

/* PRoot is busy when it handles this many stops...  */
#define BUSY_STOPS 128

/* ... within BUSY_NS, and calm again once they take CALM_NS or longer
 * (or a period that long has fewer).  */
#define BUSY_NS (100 * 1000 * 1000ULL)
#define CALM_NS (1000 * 1000 * 1000ULL)

/* The floor PRoot started with, kept while it is calm.  */
static unsigned int calm_floor;
static bool raised;

static uint64_t window_start;
static unsigned int window_stops;

/* As in <linux/sched/types.h>, which bionic lacks.  */
struct sched_attr {
	uint32_t size;
	uint32_t sched_policy;
	uint64_t sched_flags;
	int32_t sched_nice;
	uint32_t sched_priority;
	uint64_t sched_runtime;
	uint64_t sched_deadline;
	uint64_t sched_period;
	uint32_t sched_util_min;
	uint32_t sched_util_max;
};
#define SCHED_FLAG_KEEP_POLICY		0x08
#define SCHED_FLAG_KEEP_PARAMS		0x10
#define SCHED_FLAG_UTIL_CLAMP_MIN	0x20
#define UCLAMP_MAX			1024

static uint64_t now_ns(void)
{
	struct timespec now;

	/* Precise enough for these periods, and as cheap as reading
	 * memory.  */
	clock_gettime(CLOCK_MONOTONIC_COARSE, &now);
	return (uint64_t) now.tv_sec * 1000000000ULL + now.tv_nsec;
}

static void set_floor(unsigned int floor)
{
	struct sched_attr attr = {
		.size = sizeof(attr),
		.sched_flags = SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_UTIL_CLAMP_MIN,
		.sched_util_min = floor,
		.sched_util_max = UCLAMP_MAX,
	};

	(void) syscall(SYS_sched_setattr, 0, &attr, 0);
}

/**
 * Enable the floor while busy if PROOT_BUSY_BOOST=1 and the kernel
 * supports utilization clamping.  The tracees never inherit the
 * raised floor: PRoot starts only the first one, before this, and
 * the others are forked by tracees.
 */
void busy_init(void)
{
	const char *setting = getenv("PROOT_BUSY_BOOST");
	struct sched_attr attr;

	if (setting == NULL || strcmp(setting, "1") != 0)
		return;

	if (syscall(SYS_sched_getattr, 0, &attr, sizeof(attr), 0) != 0
	    || attr.size < sizeof(attr))
		return;

	calm_floor = attr.sched_util_min;
	if (calm_floor >= UCLAMP_MAX)
		return;

	window_start = now_ns();
	busy_enabled = true;
}

/**
 * Count a stop, and decide whether PRoot is busy or calm at the end
 * of each period.  Called once the tracee was restarted, so this
 * doesn't delay it.
 */
void busy_note_stop(void)
{
	uint64_t now = now_ns();
	uint64_t elapsed = now - window_start;

	window_stops++;
	if (window_stops < BUSY_STOPS && elapsed < CALM_NS)
		return;

	if (window_stops >= BUSY_STOPS && elapsed <= BUSY_NS) {
		if (!raised) {
			set_floor(UCLAMP_MAX);
			raised = true;
		}
	}
	else if (elapsed >= CALM_NS && raised) {
		set_floor(calm_floor);
		raised = false;
	}

	window_start = now;
	window_stops = 0;
}
