#ifndef FAKE_ID0_STAT_H
#define FAKE_ID0_STAT_H

#include "tracee/tracee.h"
#include "tracee/reg.h"
#include "extension/fake_id0/config.h"

int fake_id0_handle_stat_syscall(Tracee *tracee, Config *config, uintptr_t stat_state_raw);
int fake_id0_handle_statx_syscall(Tracee *tracee, Config *config, uintptr_t statx_state_raw);
#ifndef USERLAND
int handle_stat_exit_end(Tracee *tracee, Config *config, Reg stat_sysarg);
#endif
#ifdef USERLAND
int handle_stat_exit_end(Tracee *tracee, Config *config, word_t sysnum);
int handle_fstat_exit_end(Tracee *tracee, Config *config);
#endif

#endif /* FAKE_ID0_STAT_H */
