/* Stand-in for NETLINK_KOBJECT_UEVENT sockets.
 *
 * Android doesn't let apps create uevent netlink sockets, so libudev's
 * udev_monitor_new_from_netlink() fails, and programs that expect a monitor (KWin's GPU manager,
 * for instance) crash or give up on devices.  This extension hands out a datagram Unix socket
 * instead: a valid, pollable descriptor that never delivers an event.  bind() and getsockname()
 * on it act like on a netlink socket, so libudev can enable receiving.
 */

#include <errno.h>        /* E*, */
#include <string.h>       /* memset, */
#include <sys/socket.h>   /* AF_*, SOCK_*, socklen_t, */
#include <linux/netlink.h> /* NETLINK_KOBJECT_UEVENT, struct sockaddr_nl, */

#include "extension/extension.h"
#include "syscall/syscall.h"
#include "syscall/sysnum.h"
#include "tracee/tracee.h"
#include "tracee/reg.h"
#include "tracee/mem.h"

/* Stand-in sockets handed out, by process and descriptor.  An entry goes stale when its
 * descriptor is closed, and is dropped when that number is reused for another socket.  */
#define MAX_STUBS 64
static struct {
	pid_t pid;
	int fd;
} stubs[MAX_STUBS];
static size_t next_stub;

static int find_stub(pid_t pid, int fd)
{
	size_t i;

	for (i = 0; i < MAX_STUBS; i++) {
		if (stubs[i].pid == pid && stubs[i].fd == fd)
			return i;
	}
	return -1;
}

static void forget_stub(pid_t pid, int fd)
{
	int i = find_stub(pid, fd);

	if (i >= 0)
		stubs[i].pid = 0;
}

static void remember_stub(pid_t pid, int fd)
{
	if (find_stub(pid, fd) >= 0)
		return;
	stubs[next_stub].pid = pid;
	stubs[next_stub].fd = fd;
	next_stub = (next_stub + 1) % MAX_STUBS;
}

static bool is_uevent_socket(const Tracee *tracee, RegVersion version)
{
	return peek_reg(tracee, version, SYSARG_1) == AF_NETLINK
		&& peek_reg(tracee, version, SYSARG_3) == NETLINK_KOBJECT_UEVENT;
}

static int handle_sysenter_end(Tracee *tracee)
{
	switch (get_sysnum(tracee, ORIGINAL)) {
	case PR_socket: {
		word_t type;

		if (!is_uevent_socket(tracee, CURRENT))
			return 0;

		type = peek_reg(tracee, CURRENT, SYSARG_2);
		poke_reg(tracee, SYSARG_1, AF_UNIX);
		poke_reg(tracee, SYSARG_2, SOCK_DGRAM | (type & (SOCK_NONBLOCK | SOCK_CLOEXEC)));
		poke_reg(tracee, SYSARG_3, 0);
		return 0;
	}

	case PR_bind:
		if (find_stub(tracee->pid, peek_reg(tracee, CURRENT, SYSARG_1)) < 0)
			return 0;

		/* Nothing to bind to: accept the netlink address.  */
		poke_reg(tracee, SYSARG_RESULT, 0);
		set_sysnum(tracee, PR_void);
		return 0;

	case PR_getsockname: {
		struct sockaddr_nl address;
		word_t address_pointer = peek_reg(tracee, CURRENT, SYSARG_2);
		word_t length_pointer = peek_reg(tracee, CURRENT, SYSARG_3);
		socklen_t length;
		int status;

		if (find_stub(tracee->pid, peek_reg(tracee, CURRENT, SYSARG_1)) < 0)
			return 0;

		status = read_data(tracee, &length, length_pointer, sizeof(length));
		if (status < 0)
			return status;

		memset(&address, 0, sizeof(address));
		address.nl_family = AF_NETLINK;
		address.nl_pid = tracee->pid;
		status = write_data(tracee, address_pointer, &address,
				    length < sizeof(address) ? length : sizeof(address));
		if (status < 0)
			return status;

		length = sizeof(address);
		status = write_data(tracee, length_pointer, &length, sizeof(length));
		if (status < 0)
			return status;

		poke_reg(tracee, SYSARG_RESULT, 0);
		set_sysnum(tracee, PR_void);
		return 0;
	}

	default:
		return 0;
	}
}

static int handle_sysexit_end(Tracee *tracee)
{
	int fd;

	if (get_sysnum(tracee, ORIGINAL) != PR_socket)
		return 0;

	fd = (int) peek_reg(tracee, CURRENT, SYSARG_RESULT);
	if (fd < 0)
		return 0;

	if (is_uevent_socket(tracee, ORIGINAL))
		remember_stub(tracee->pid, fd);
	else
		forget_stub(tracee->pid, fd);
	return 0;
}

int uevent_stub_callback(Extension *extension, ExtensionEvent event,
			 intptr_t data1 UNUSED, intptr_t data2 UNUSED)
{
	switch (event) {
	case INITIALIZATION: {
		static FilteredSysnum filtered_sysnums[] = {
			{ PR_socket,		FILTER_SYSEXIT },
			{ PR_bind,		0 },
			{ PR_getsockname,	0 },
			FILTERED_SYSNUM_END,
		};
		extension->filtered_sysnums = filtered_sysnums;
		return 0;
	}

	case SYSCALL_ENTER_END:
		return handle_sysenter_end(TRACEE(extension));

	case SYSCALL_EXIT_END:
		return handle_sysexit_end(TRACEE(extension));

	default:
		return 0;
	}
}
