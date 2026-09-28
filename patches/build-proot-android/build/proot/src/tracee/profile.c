/* -*- c-set-style: "K&R"; c-basic-offset: 8 -*-
 *
 * This file is part of PRoot.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License as
 * published by the Free Software Foundation; either version 2 of the
 * License, or (at your option) any later version.
 *
 * A profiler for PRoot itself: with PROOT_PROFILE=<file>, each stop of
 * a tracee is counted per syscall (entry and exit stages apart, stops
 * of chained syscalls under the syscall that started the chain) along
 * with the time PRoot took from waitpid() to restarting the tracee.
 * The table is written to <file>.<pid> every few seconds and at exit.
 *
 * With PROOT_PROFILE_HZ=<n> as well, PRoot also samples its own call
 * stacks <n> times per second of CPU time (frame pointers, so build
 * with -fno-omit-frame-pointer) and appends them to <file>.<pid>.samples
 * as raw addresses, after a copy of its memory map; profile-report.py
 * symbolizes them.
 */

#include <signal.h>     /* SIG*, sigaction(2), */
#include <stdint.h>     /* uintptr_t, */
#include <sys/time.h>   /* setitimer(2), */
#include <ucontext.h>   /* ucontext_t, */
#include <stdio.h>      /* fopen(3), fprintf(3), */
#include <stdlib.h>     /* getenv(3), qsort(3), */
#include <string.h>     /* memset(3), */
#include <sys/ptrace.h> /* PTRACE_EVENT_*, */
#include <sys/wait.h>   /* WIF*, */
#include <time.h>       /* clock_gettime(2), */
#include <unistd.h>     /* getpid(2), */

#include "tracee/profile.h"
#include "syscall/sysnum.h"
#include "compat.h"

bool profile_enabled = false;
bool profile_paths_enabled = false;
static FILE *paths_file;

typedef struct {
	unsigned long count;
	unsigned long long ns;
} Counter;

enum {
	EVENT_EXITED,
	EVENT_SIGNALED,
	EVENT_FORK,
	EVENT_VFORK,
	EVENT_CLONE,
	EVENT_EXEC,
	EVENT_EXIT,
	EVENT_VFORK_DONE,
	EVENT_SIGNAL,
	EVENT_NB,
};

static const char *event_names[EVENT_NB] = {
	"(exited)", "(signaled)", "(fork)", "(vfork)", "(clone)", "(exec)",
	"(exit)", "(vfork-done)", "(signal)",
};

static const char *output;

#define MAX_FRAMES 12
#define MAX_SAMPLES 100000
static uintptr_t samples[MAX_SAMPLES][MAX_FRAMES];
static volatile size_t nb_samples;

static void take_sample(int signum, siginfo_t *siginfo, void *context)
{
	ucontext_t *ucontext = context;
	uintptr_t *sample;
	uintptr_t fp, sp;
	size_t i = 0;

	(void) signum;
	(void) siginfo;
	if (nb_samples >= MAX_SAMPLES)
		return;
	sample = samples[nb_samples];

#if defined(__aarch64__)
	sample[i++] = ucontext->uc_mcontext.pc;
	sample[i++] = ucontext->uc_mcontext.regs[30];
	fp = ucontext->uc_mcontext.regs[29];
	sp = ucontext->uc_mcontext.sp;
#elif defined(__x86_64__)
	sample[i++] = ucontext->uc_mcontext.gregs[REG_RIP];
	fp = ucontext->uc_mcontext.gregs[REG_RBP];
	sp = ucontext->uc_mcontext.gregs[REG_RSP];
#else
	fp = sp = 0;
#endif
	/* Follow the frame records: [previous fp, return address].  */
	while (i < MAX_FRAMES && fp >= sp && fp < sp + (8 << 20) && (fp & 7) == 0) {
		uintptr_t *frame = (uintptr_t *) fp;
		if (frame[1] == 0)
			break;
		sample[i++] = frame[1];
		if (frame[0] <= fp)
			break;
		fp = frame[0];
	}
	if (i < MAX_FRAMES)
		sample[i] = 0;
	nb_samples++;
}

static void start_sampling(void)
{
	const char *hz_string = getenv("PROOT_PROFILE_HZ");
	struct sigaction action;
	struct itimerval timer;
	long hz;

	if (hz_string == NULL || (hz = strtol(hz_string, NULL, 10)) <= 0)
		return;

	memset(&action, 0, sizeof(action));
	action.sa_sigaction = take_sample;
	action.sa_flags = SA_SIGINFO | SA_RESTART;
	sigemptyset(&action.sa_mask);
	if (sigaction(SIGPROF, &action, NULL) < 0)
		return;

	timer.it_interval.tv_sec = 0;
	timer.it_interval.tv_usec = 1000000 / hz;
	timer.it_value = timer.it_interval;
	(void) setitimer(ITIMER_PROF, &timer, NULL);
}

/* Append the samples taken since the last call, as "s <address>..."
 * lines, after a copy of /proc/self/maps ("m <line>") the first time.  */
static void dump_samples(void)
{
	static bool maps_written = false;
	sigset_t blocked, previous;
	char path[4096];
	char line[4096];
	size_t i, j;
	FILE *file;

	if (nb_samples == 0)
		return;

	/* Samples taken while writing would be lost.  */
	sigemptyset(&blocked);
	sigaddset(&blocked, SIGPROF);
	sigprocmask(SIG_BLOCK, &blocked, &previous);

	snprintf(path, sizeof(path), "%s.%d.samples", output, getpid());
	file = fopen(path, maps_written ? "a" : "w");
	if (file == NULL)
		goto end;

	if (!maps_written) {
		FILE *maps = fopen("/proc/self/maps", "r");
		while (maps != NULL && fgets(line, sizeof(line), maps) != NULL)
			fprintf(file, "m %s", line);
		if (maps != NULL)
			fclose(maps);
		maps_written = true;
	}

	for (i = 0; i < nb_samples; i++) {
		fputc('s', file);
		for (j = 0; j < MAX_FRAMES && samples[i][j] != 0; j++)
			fprintf(file, " %lx", (unsigned long) samples[i][j]);
		fputc('\n', file);
	}
	nb_samples = 0;
	fclose(file);
end:
	sigprocmask(SIG_SETMASK, &previous, NULL);
}

static Counter syscalls[PR_NB_SYSNUM][2];
static Counter events[EVENT_NB];
static unsigned long long started, stop_began, last_dump;

static unsigned long long now(void)
{
	struct timespec time;
	clock_gettime(CLOCK_MONOTONIC, &time);
	return (unsigned long long) time.tv_sec * 1000000000ULL + time.tv_nsec;
}

void profile_init(void)
{
	output = getenv("PROOT_PROFILE");
	if (output == NULL || output[0] == '\0')
		return;

	profile_enabled = true;
	started = now();
	last_dump = started;
	start_sampling();

	if (getenv("PROOT_PROFILE_PATHS") != NULL) {
		char path[4096];
		snprintf(path, sizeof(path), "%s.%d.paths", output, getpid());
		paths_file = fopen(path, "w");
		if (paths_file != NULL) {
			setvbuf(paths_file, NULL, _IOFBF, 1 << 16);
			profile_paths_enabled = true;
		}
	}
}

/* Log a path @tracee asked for: "<program>\t<syscall>\t<path>".  */
void profile_path(const Tracee *tracee, const char *path)
{
	Sysnum sysnum = get_sysnum(tracee, ORIGINAL);
	fprintf(paths_file, "%s\t%s\t%s\n", tracee->exe != NULL ? tracee->exe : "?",
		sysnum < PR_NB_SYSNUM ? stringify_sysnum(sysnum) : "?", path);
}

void profile_stop_begin(void)
{
	stop_began = now();
}

void profile_stop_end(const Tracee *tracee, int tracee_status, bool was_sysenter)
{
	unsigned long long ended = now();
	unsigned long long spent = ended - stop_began;
	Counter *counter;

	if (WIFEXITED(tracee_status))
		counter = &events[EVENT_EXITED];
	else if (WIFSIGNALED(tracee_status))
		counter = &events[EVENT_SIGNALED];
	else {
		int signal = (tracee_status & 0xfff00) >> 8;

		switch (signal) {
		case SIGTRAP | 0x80:
		case SIGTRAP | PTRACE_EVENT_SECCOMP << 8:
		case SIGTRAP | PTRACE_EVENT_SECCOMP2 << 8: {
			Sysnum sysnum = get_sysnum(tracee, ORIGINAL);
			if (sysnum >= PR_NB_SYSNUM)
				sysnum = PR_void;
			counter = &syscalls[sysnum][was_sysenter ? 0 : 1];
			break;
		}
		case SIGTRAP | PTRACE_EVENT_FORK << 8:
			counter = &events[EVENT_FORK];
			break;
		case SIGTRAP | PTRACE_EVENT_VFORK << 8:
			counter = &events[EVENT_VFORK];
			break;
		case SIGTRAP | PTRACE_EVENT_CLONE << 8:
			counter = &events[EVENT_CLONE];
			break;
		case SIGTRAP | PTRACE_EVENT_EXEC << 8:
			counter = &events[EVENT_EXEC];
			break;
		case SIGTRAP | PTRACE_EVENT_EXIT << 8:
			counter = &events[EVENT_EXIT];
			break;
		case SIGTRAP | PTRACE_EVENT_VFORK_DONE << 8:
			counter = &events[EVENT_VFORK_DONE];
			break;
		default:
			counter = &events[EVENT_SIGNAL];
			break;
		}
	}

	counter->count++;
	counter->ns += spent;

	if (ended - last_dump > 5000000000ULL) {
		last_dump = ended;
		profile_dump();
	}
}

typedef struct {
	const char *name;
	Counter enter, exit;
} Row;

static int by_time(const void *a, const void *b)
{
	const Row *row_a = a, *row_b = b;
	unsigned long long time_a = row_a->enter.ns + row_a->exit.ns;
	unsigned long long time_b = row_b->enter.ns + row_b->exit.ns;
	return time_a < time_b ? 1 : time_a > time_b ? -1 : 0;
}

void profile_dump(void)
{
	static Row rows[PR_NB_SYSNUM + EVENT_NB];
	unsigned long long total_ns = 0;
	unsigned long total_stops = 0;
	char path[4096];
	size_t nb_rows = 0;
	size_t i;
	FILE *file;

	if (!profile_enabled)
		return;

	if (paths_file != NULL)
		fflush(paths_file);

	for (i = 0; i < PR_NB_SYSNUM; i++) {
		if (syscalls[i][0].count == 0 && syscalls[i][1].count == 0)
			continue;
		rows[nb_rows].name = i == PR_void ? "(void)" : stringify_sysnum(i);
		rows[nb_rows].enter = syscalls[i][0];
		rows[nb_rows].exit = syscalls[i][1];
		nb_rows++;
	}
	for (i = 0; i < EVENT_NB; i++) {
		if (events[i].count == 0)
			continue;
		rows[nb_rows].name = event_names[i];
		rows[nb_rows].enter = events[i];
		memset(&rows[nb_rows].exit, 0, sizeof(Counter));
		nb_rows++;
	}
	qsort(rows, nb_rows, sizeof(Row), by_time);

	for (i = 0; i < nb_rows; i++) {
		total_ns += rows[i].enter.ns + rows[i].exit.ns;
		total_stops += rows[i].enter.count + rows[i].exit.count;
	}

	snprintf(path, sizeof(path), "%s.%d", output, getpid());
	file = fopen(path, "w");
	if (file == NULL)
		return;

	fprintf(file, "wall %.3f s, %lu stops, %.3f s in proot, %zu samples\n",
		(now() - started) / 1e9, total_stops, total_ns / 1e9, (size_t) nb_samples);
	fprintf(file, "%-22s %10s %10s %10s %10s %8s\n",
		"syscall", "enters", "enter-ms", "exits", "exit-ms", "us/call");
	for (i = 0; i < nb_rows; i++) {
		unsigned long calls = rows[i].enter.count ? rows[i].enter.count : rows[i].exit.count;
		fprintf(file, "%-22s %10lu %10.1f %10lu %10.1f %8.1f\n", rows[i].name,
			rows[i].enter.count, rows[i].enter.ns / 1e6,
			rows[i].exit.count, rows[i].exit.ns / 1e6,
			calls ? (rows[i].enter.ns + rows[i].exit.ns) / 1e3 / calls : 0.0);
	}
	fclose(file);

	dump_samples();
}
