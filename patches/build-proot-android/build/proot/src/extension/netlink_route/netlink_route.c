/* -*- c-set-style: "K&R"; c-basic-offset: 8 -*-
 *
 * Answers for the route netlink requests Android refuses.
 *
 * Since Android 11, apps may not bind() route netlink sockets or ask them for the network
 * interfaces (RTM_GETLINK), and the SIOCGIF* ioctls only work on internet sockets: this keeps
 * hardware addresses private.  Addresses (RTM_GETADDR) and the ioctls on internet sockets are
 * still allowed.  glibc's getifaddrs(), if_nameindex() and if_nametoindex(), Go's
 * net.Interfaces(), and what is built on them (Node's os.networkInterfaces(), which Vite's dev
 * server calls, Python's socket.if_nameindex()) fail with EACCES instead.  This extension:
 *
 * - turns a refused bind() to an address of the kernel's choosing into connect(), which picks
 *   one the same way;
 *
 * - answers RTM_GETLINK requests sent with sendto() or sendmsg() itself, from what the SIOCGIF*
 *   ioctls report, with blank hardware addresses.  getsockopt() and getsockname(), chained
 *   after the refused request, tell whether the socket is a route netlink one and its port.
 *   The tracee then receives the answer from proot: it is stopped at each syscall until its
 *   recvmsg(), recvfrom() or read() on that socket, which are emulated;
 *
 * - redoes refused SIOCGIF* ioctls on an internet socket (glibc uses a Unix one).
 *
 * Requests sent with send(), like iproute2's and musl's, aren't seen: tracing every send()
 * would cost too much.
 */

#include <errno.h>           /* E*, */
#include <stdbool.h>         /* bool, */
#include <stddef.h>          /* offsetof, */
#include <stdint.h>          /* uint*_t, */
#include <string.h>          /* memcpy(3), memset(3), strcmp(3), strlen(3), */
#include <sys/ioctl.h>       /* ioctl(2), */
#include <sys/socket.h>      /* socket(2), AF_*, SOCK_*, MSG_*, SO_PROTOCOL, */
#include <sys/uio.h>         /* struct iovec, */
#include <signal.h>          /* SIGTRAP, */
#include <sys/wait.h>        /* WIFSTOPPED, */
#include <unistd.h>          /* close(2), */
#include <net/if.h>          /* struct ifreq, struct ifconf, IFF_*, IF_OPER_*, */
#include <linux/if_arp.h>    /* ARPHRD_*, */
#include <linux/if_ether.h>  /* ETH_ALEN, */
#include <linux/netlink.h>   /* struct sockaddr_nl, struct nlmsghdr, NLM*, */
#include <linux/rtnetlink.h> /* RTM_*, IFLA_*, struct ifinfomsg, struct ifaddrmsg, */
#include <linux/sockios.h>   /* SIOCGIF*, */
#include <talloc.h>          /* talloc*, */

#include "extension/extension.h"
#include "syscall/chain.h"
#include "syscall/syscall.h"
#include "syscall/sysnum.h"
#include "tracee/tracee.h"
#include "tracee/abi.h"
#include "tracee/reg.h"
#include "tracee/mem.h"
#include "cli/note.h"

/* A link flag that doesn't fit in the 16 bits SIOCGIFFLAGS reports.  */
#define LINK_LOWER_UP 0x10000

/* The kernel sends a dump in datagrams of up to about a page (NLMSG_GOODSIZE).  */
#define DATAGRAM_SIZE 3776

/* How many syscalls a tracee may make before it receives an answer, after which proot stops
 * holding the answer for it.  */
#define RECEIVE_DEADLINE 256

/* Interfaces are looked for up to this many unused indexes past the last one found, and past
 * the highest index with an address in any case.  */
#define INDEX_GAP 32
#define MAX_INDEX 4096

/* The largest messages put in an answer.  */
#define LINK_MESSAGE_SIZE 96
#define ERROR_MESSAGE_SIZE NLMSG_ALIGN(NLMSG_LENGTH(sizeof(struct nlmsgerr)))
#define DONE_MESSAGE_SIZE NLMSG_ALIGN(NLMSG_LENGTH(sizeof(int)))

/* Memory in the tracee for the chained getsockopt(2) and getsockname(2).  */
typedef struct {
	struct sockaddr_nl address;
	socklen_t address_length;
	int protocol;
	socklen_t protocol_length;
} Scratch;

typedef struct {
	/* A bind(2) to an address of the kernel's choosing.  */
	bool binding;

	/* An RTM_GETLINK request sent on "fd", "length" bytes long.  */
	struct {
		enum { NO_REQUEST, SENDING, CHECKING_PROTOCOL, GETTING_PORT } state;
		int fd;
		word_t length;
		struct nlmsghdr header;

		/* Only the family is set when the request carries a struct rtgenmsg.  */
		struct ifinfomsg link;
		char name[IFNAMSIZ];

		word_t scratch;
	} request;

	/* The answer, to be received on "fd" from "offset".  */
	struct {
		int fd;
		uint8_t *data;
		size_t size;
		size_t offset;
		unsigned int deadline;
	} answer;
} Config;

typedef struct {
	int index;
	char name[IFNAMSIZ];
	unsigned int flags;
	uint32_t mtu;
} Link;

static const unsigned int interface_requests[] = {
	SIOCGIFNAME, SIOCGIFCONF, SIOCGIFFLAGS, SIOCGIFADDR, SIOCGIFDSTADDR, SIOCGIFBRDADDR,
	SIOCGIFNETMASK, SIOCGIFMETRIC, SIOCGIFMTU, SIOCGIFINDEX, SIOCGIFTXQLEN,
};

static bool is_interface_request(unsigned int request)
{
	size_t i;

	for (i = 0; i < sizeof(interface_requests) / sizeof(interface_requests[0]); i++) {
		if (interface_requests[i] == request)
			return true;
	}
	return false;
}

/* A socket of the tracer's for the SIOCGIF* ioctls, which Android allows on internet sockets.  */
static int internet_socket(void)
{
	static int sock = -1;

	if (sock < 0)
		sock = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
	return sock;
}

/**
 * Whether the @length bytes at @address in the tracee are the kernel's netlink address, which
 * requests are sent to, and which bind(2) takes to mean a port of the kernel's choosing.
 */
static bool is_kernel_address(const Tracee *tracee, word_t address, word_t length)
{
	struct sockaddr_nl netlink;

	if (address == 0 || length < sizeof(netlink))
		return false;
	if (read_data(tracee, &netlink, address, sizeof(netlink)) < 0)
		return false;
	return netlink.nl_family == AF_NETLINK && netlink.nl_pid == 0 && netlink.nl_groups == 0;
}

/**
 * Remember the RTM_GETLINK request at @buffer (@size bytes) in the tracee, if that's what it
 * is, which is about to be sent on @fd; sending it returns @length on success.  Its exit stage
 * will tell whether the kernel refused it.
 */
static void check_request(Tracee *tracee, Config *config, int fd, word_t buffer, word_t size,
			word_t length)
{
	uint8_t request[256];
	struct nlmsghdr header;
	size_t end;
	word_t scratch;

	if (size > sizeof(request))
		size = sizeof(request);
	if (size < NLMSG_LENGTH(sizeof(struct rtgenmsg)))
		return;
	if (read_data(tracee, request, buffer, size) < 0)
		return;

	memcpy(&header, request, sizeof(header));
	if (header.nlmsg_type != RTM_GETLINK
	    || (header.nlmsg_flags & NLM_F_REQUEST) == 0
	    || header.nlmsg_len < NLMSG_LENGTH(sizeof(struct rtgenmsg))
	    || header.nlmsg_len > size)
		return;

	memset(&config->request.link, 0, sizeof(config->request.link));
	config->request.link.ifi_family = request[NLMSG_HDRLEN];
	config->request.name[0] = '\0';

	/* A request for one interface names it by index or by name.  */
	end = header.nlmsg_len;
	if (end >= NLMSG_LENGTH(sizeof(struct ifinfomsg))) {
		size_t offset = NLMSG_LENGTH(sizeof(struct ifinfomsg));

		memcpy(&config->request.link, request + NLMSG_HDRLEN, sizeof(struct ifinfomsg));
		while (offset + sizeof(struct rtattr) <= end) {
			struct rtattr attribute;

			memcpy(&attribute, request + offset, sizeof(attribute));
			if (attribute.rta_len < sizeof(attribute) || offset + attribute.rta_len > end)
				break;

			if (attribute.rta_type == IFLA_IFNAME) {
				size_t name_length = attribute.rta_len - sizeof(attribute);

				if (name_length > IFNAMSIZ - 1)
					name_length = IFNAMSIZ - 1;
				memcpy(config->request.name, request + offset + sizeof(attribute),
				       name_length);
				config->request.name[name_length] = '\0';
			}
			offset += RTA_ALIGN(attribute.rta_len);
		}
	}

	scratch = alloc_mem(tracee, sizeof(Scratch));
	if (scratch == 0)
		return;

	config->request.state = SENDING;
	config->request.fd = fd;
	config->request.length = length;
	config->request.header = header;
	config->request.scratch = scratch;

	/* Stop at the exit stage.  */
	tracee->restart_how = PTRACE_SYSCALL;
}

/**
 * Check the request sendmsg(2) is about to send with the struct msghdr at @address in the
 * tracee; it may be a connected socket's, without an address.
 */
static void check_sendmsg(Tracee *tracee, Config *config, int fd, word_t address)
{
	struct iovec vectors[16];
	struct msghdr message;
	size_t nb_vectors;
	word_t length = 0;
	size_t i;

	if (read_data(tracee, &message, address, sizeof(message)) < 0)
		return;
	if (message.msg_name != NULL
	    && !is_kernel_address(tracee, (word_t) message.msg_name, message.msg_namelen))
		return;
	if (message.msg_iovlen == 0 || message.msg_iovlen > sizeof(vectors) / sizeof(vectors[0]))
		return;

	nb_vectors = message.msg_iovlen;
	if (read_data(tracee, vectors, (word_t) message.msg_iov, nb_vectors * sizeof(vectors[0])) < 0)
		return;
	for (i = 0; i < nb_vectors; i++)
		length += vectors[i].iov_len;

	check_request(tracee, config, fd, (word_t) vectors[0].iov_base, vectors[0].iov_len, length);
}

static void drop_answer(Config *config)
{
	TALLOC_FREE(config->answer.data);
	config->answer.size = 0;
	config->answer.offset = 0;
}

/**
 * The size of the answer's next datagram: as many whole messages as fit in DATAGRAM_SIZE, and
 * at least one.
 */
static size_t next_datagram_size(const Config *config)
{
	size_t start = config->answer.offset;
	size_t end = start;

	while (end < config->answer.size) {
		struct nlmsghdr header;
		size_t length;

		memcpy(&header, config->answer.data + end, sizeof(header));
		length = NLMSG_ALIGN(header.nlmsg_len);
		if (end > start && end + length - start > DATAGRAM_SIZE)
			break;
		end += length;
	}
	return end - start;
}

/**
 * Copy @size bytes of @data into the @nb_vectors buffers described by the struct iovec array at
 * @address in the tracee.  This function returns the number of bytes copied, or -errno.
 */
static int copy_to_vectors(Tracee *tracee, word_t address, size_t nb_vectors,
			const uint8_t *data, size_t size)
{
	struct iovec vectors[64];
	size_t copied = 0;
	size_t i;
	int status;

	if (nb_vectors > sizeof(vectors) / sizeof(vectors[0]))
		nb_vectors = sizeof(vectors) / sizeof(vectors[0]);
	if (nb_vectors == 0)
		return 0;

	status = read_data(tracee, vectors, address, nb_vectors * sizeof(vectors[0]));
	if (status < 0)
		return status;

	for (i = 0; i < nb_vectors && copied < size; i++) {
		size_t length = vectors[i].iov_len < size - copied ? vectors[i].iov_len : size - copied;

		if (length == 0)
			continue;
		status = write_data(tracee, (word_t) vectors[i].iov_base, data + copied, length);
		if (status < 0)
			return status;
		copied += length;
	}
	return copied;
}

/**
 * Write the kernel's netlink address, the sender of every answer, at @address in the tracee,
 * which has room for *@length bytes, and set *@length to its size, like recvmsg(2) does.
 */
static int write_sender(Tracee *tracee, word_t address, socklen_t *length)
{
	struct sockaddr_nl kernel;
	int status;

	memset(&kernel, 0, sizeof(kernel));
	kernel.nl_family = AF_NETLINK;
	status = write_data(tracee, address, &kernel,
			*length < sizeof(kernel) ? *length : sizeof(kernel));
	if (status < 0)
		return status;

	*length = sizeof(kernel);
	return 0;
}

/**
 * Emulate the receive the tracee enters (recvmsg(2), recvfrom(2) or read(2) on the socket the
 * answer is for), with the answer's next datagram.  This function returns -errno if the tracee
 * gave a bad address, otherwise 0.
 */
static int receive_answer(Tracee *tracee, Config *config, Sysnum sysnum)
{
	const uint8_t *datagram = config->answer.data + config->answer.offset;
	size_t size = next_datagram_size(config);
	size_t copied;
	int flags = 0;
	int status;

	switch (sysnum) {
	case PR_recvmsg: {
		word_t address = peek_reg(tracee, CURRENT, SYSARG_2);
		struct msghdr message;

		flags = (int) peek_reg(tracee, CURRENT, SYSARG_3);
		if ((flags & MSG_ERRQUEUE) != 0)
			return 0;

		status = read_data(tracee, &message, address, sizeof(message));
		if (status < 0)
			return status;

		status = copy_to_vectors(tracee, (word_t) message.msg_iov, message.msg_iovlen,
					datagram, size);
		if (status < 0)
			return status;
		copied = status;

		if (message.msg_name != NULL) {
			status = write_sender(tracee, (word_t) message.msg_name, &message.msg_namelen);
			if (status < 0)
				return status;
		}
		message.msg_controllen = 0;
		message.msg_flags = copied < size ? MSG_TRUNC : 0;

		status = write_data(tracee, address, &message, sizeof(message));
		if (status < 0)
			return status;
		break;
	}

	case PR_recvfrom: {
		word_t buffer = peek_reg(tracee, CURRENT, SYSARG_2);
		word_t length = peek_reg(tracee, CURRENT, SYSARG_3);
		word_t address = peek_reg(tracee, CURRENT, SYSARG_5);
		word_t address_length = peek_reg(tracee, CURRENT, SYSARG_6);

		flags = (int) peek_reg(tracee, CURRENT, SYSARG_4);
		if ((flags & MSG_ERRQUEUE) != 0)
			return 0;

		copied = length < size ? length : size;
		status = write_data(tracee, buffer, datagram, copied);
		if (status < 0)
			return status;

		if (address != 0 && address_length != 0) {
			socklen_t room;

			status = read_data(tracee, &room, address_length, sizeof(room));
			if (status < 0)
				return status;
			status = write_sender(tracee, address, &room);
			if (status < 0)
				return status;
			status = write_data(tracee, address_length, &room, sizeof(room));
			if (status < 0)
				return status;
		}
		break;
	}

	case PR_read: {
		word_t buffer = peek_reg(tracee, CURRENT, SYSARG_2);
		word_t length = peek_reg(tracee, CURRENT, SYSARG_3);

		copied = length < size ? length : size;
		status = write_data(tracee, buffer, datagram, copied);
		if (status < 0)
			return status;
		break;
	}

	default:
		return 0;
	}

	/* The result is kept at the exit stage, see translate_syscall_exit().  */
	set_sysnum(tracee, PR_void);
	poke_reg(tracee, SYSARG_RESULT, (flags & MSG_TRUNC) != 0 ? size : copied);

	if ((flags & MSG_PEEK) == 0) {
		config->answer.offset += size;
		if (config->answer.offset >= config->answer.size) {
			VERBOSE(tracee, 2, "netlink-route: answer received");
			drop_answer(config);
		}
	}
	return 0;
}

/**
 * Handle the syscall a tracee enters while an answer waits for it.  This function returns
 * -errno if the syscall has to fail, otherwise 0.
 */
static int wait_for_receive(Tracee *tracee, Config *config, Sysnum sysnum)
{
	int fd = (int) peek_reg(tracee, CURRENT, SYSARG_1);

	switch (sysnum) {
	case PR_recvmsg:
	case PR_recvfrom:
	case PR_read:
		if (fd == config->answer.fd)
			return receive_answer(tracee, config, sysnum);
		break;

	case PR_close:
		if (fd == config->answer.fd)
			drop_answer(config);
		break;

	case PR_dup2:
	case PR_dup3:
		if ((int) peek_reg(tracee, CURRENT, SYSARG_2) == config->answer.fd)
			drop_answer(config);
		break;

	case PR_execve:
	case PR_execveat:
		drop_answer(config);
		break;

	default:
		break;
	}

	if (config->answer.data != NULL && --config->answer.deadline == 0) {
		VERBOSE(tracee, 1, "netlink-route: the answer to RTM_GETLINK was never received");
		drop_answer(config);
	}
	return 0;
}

/**
 * The highest index of an interface with an address, which RTM_GETADDR still reports.
 */
static int highest_address_index(void)
{
	struct {
		struct nlmsghdr header;
		struct ifaddrmsg message;
	} request;
	struct sockaddr_nl kernel;
	uint8_t buffer[16384] __attribute__((aligned(NLMSG_ALIGNTO)));
	int highest = 0;
	int sock;

	sock = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
	if (sock < 0)
		return 0;

	memset(&request, 0, sizeof(request));
	request.header.nlmsg_len = sizeof(request);
	request.header.nlmsg_type = RTM_GETADDR;
	request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
	request.header.nlmsg_seq = 1;
	request.message.ifa_family = AF_UNSPEC;

	memset(&kernel, 0, sizeof(kernel));
	kernel.nl_family = AF_NETLINK;

	if (sendto(sock, &request, sizeof(request), 0, (struct sockaddr *) &kernel, sizeof(kernel)) < 0)
		goto end;

	/* The kernel queues each part of a dump while the previous one is received, so this
	 * never has to wait.  */
	for (;;) {
		int size = recv(sock, buffer, sizeof(buffer), MSG_DONTWAIT);
		struct nlmsghdr *header;

		if (size <= 0)
			goto end;

		for (header = (struct nlmsghdr *) buffer; NLMSG_OK(header, size);
		     header = NLMSG_NEXT(header, size)) {
			const struct ifaddrmsg *address = NLMSG_DATA(header);

			if (header->nlmsg_type == NLMSG_DONE || header->nlmsg_type == NLMSG_ERROR)
				goto end;
			if (header->nlmsg_type == RTM_NEWADDR && (int) address->ifa_index > highest)
				highest = address->ifa_index;
		}
	}

end:
	close(sock);
	return highest;
}

/**
 * List the network interfaces in *@links, allocated with @context.  This function returns how
 * many there are.
 */
static size_t list_links(TALLOC_CTX *context, Link **links)
{
	int sock = internet_socket();
	int highest;
	int index;
	int misses;
	size_t count = 0;

	*links = NULL;
	if (sock < 0)
		return 0;

	/* Every interface RTM_GETADDR reports must be listed: glibc's getifaddrs() starts over
	 * as long as one is missing.  */
	highest = highest_address_index();

	for (index = 1, misses = 0; index <= MAX_INDEX && (index <= highest || misses < INDEX_GAP);
	     index++) {
		struct ifreq request;
		Link *grown;
		Link *link;

		memset(&request, 0, sizeof(request));
		request.ifr_ifindex = index;
		if (ioctl(sock, SIOCGIFNAME, &request) < 0) {
			misses++;
			continue;
		}
		misses = 0;

		grown = talloc_realloc(context, *links, Link, count + 1);
		if (grown == NULL)
			break;
		*links = grown;

		link = &(*links)[count++];
		memset(link, 0, sizeof(*link));
		link->index = index;
		memcpy(link->name, request.ifr_name, IFNAMSIZ - 1);

		if (ioctl(sock, SIOCGIFFLAGS, &request) == 0)
			link->flags = (unsigned short) request.ifr_flags;
		if (ioctl(sock, SIOCGIFMTU, &request) == 0)
			link->mtu = request.ifr_mtu;
	}

	return count;
}

static void put_header(uint8_t *message, size_t length, uint16_t type, uint16_t flags,
		const struct nlmsghdr *request, uint32_t port)
{
	struct nlmsghdr header;

	header.nlmsg_len = length;
	header.nlmsg_type = type;
	header.nlmsg_flags = flags;
	header.nlmsg_seq = request->nlmsg_seq;
	header.nlmsg_pid = port;
	memcpy(message, &header, sizeof(header));
}

/**
 * Append attribute @type with the @length bytes of @data to @message, *@size bytes long so far.
 */
static void put_attribute(uint8_t *message, size_t *size, uint16_t type, const void *data,
			size_t length)
{
	struct rtattr attribute;

	attribute.rta_len = RTA_LENGTH(length);
	attribute.rta_type = type;
	memcpy(message + *size, &attribute, sizeof(attribute));
	memcpy(message + *size + RTA_LENGTH(0), data, length);
	memset(message + *size + RTA_LENGTH(length), 0, RTA_SPACE(length) - RTA_LENGTH(length));
	*size += RTA_SPACE(length);
}

/**
 * Append to @answer, *@size bytes long so far, an RTM_NEWLINK message about @link.
 */
static void put_link(uint8_t *answer, size_t *size, const Link *link,
		const struct nlmsghdr *request, uint32_t port, uint16_t flags)
{
	uint8_t *message = answer + *size;
	size_t length = NLMSG_LENGTH(sizeof(struct ifinfomsg));
	uint8_t hardware_address[ETH_ALEN];
	struct ifinfomsg info;
	uint8_t operstate;

	memset(&info, 0, sizeof(info));
	info.ifi_family = AF_UNSPEC;
	info.ifi_index = link->index;
	info.ifi_flags = link->flags | ((link->flags & IFF_RUNNING) != 0 ? LINK_LOWER_UP : 0);
	if ((link->flags & IFF_LOOPBACK) != 0)
		info.ifi_type = ARPHRD_LOOPBACK;
	else if ((link->flags & IFF_POINTOPOINT) != 0)
		info.ifi_type = ARPHRD_NONE;
	else
		info.ifi_type = ARPHRD_ETHER;
	memcpy(message + NLMSG_HDRLEN, &info, sizeof(info));

	put_attribute(message, &length, IFLA_IFNAME, link->name, strlen(link->name) + 1);
	put_attribute(message, &length, IFLA_MTU, &link->mtu, sizeof(link->mtu));

	operstate = (link->flags & IFF_LOOPBACK) != 0 ? IF_OPER_UNKNOWN
		: (link->flags & IFF_RUNNING) != 0 ? IF_OPER_UP : IF_OPER_DOWN;
	put_attribute(message, &length, IFLA_OPERSTATE, &operstate, sizeof(operstate));

	/* Hardware addresses are what Android keeps from apps.  */
	if (info.ifi_type != ARPHRD_NONE) {
		memset(hardware_address, 0, sizeof(hardware_address));
		put_attribute(message, &length, IFLA_ADDRESS, hardware_address,
			sizeof(hardware_address));
	}

	put_header(message, length, RTM_NEWLINK, flags, request, port);
	*size += NLMSG_ALIGN(length);
}

/**
 * Append to @answer, *@size bytes long so far, the error @error, or an acknowledgement if it
 * is 0, for @request.
 */
static void put_error(uint8_t *answer, size_t *size, int error, const struct nlmsghdr *request,
		uint32_t port)
{
	uint8_t *message = answer + *size;
	struct nlmsgerr body;

	body.error = error;
	body.msg = *request;
	memcpy(message + NLMSG_HDRLEN, &body, sizeof(body));

	/* Capped: without the request's payload.  */
	put_header(message, NLMSG_LENGTH(sizeof(body)), NLMSG_ERROR, NLM_F_CAPPED, request, port);
	*size += ERROR_MESSAGE_SIZE;
}

static void put_done(uint8_t *answer, size_t *size, const struct nlmsghdr *request, uint32_t port)
{
	uint8_t *message = answer + *size;
	int status = 0;

	memcpy(message + NLMSG_HDRLEN, &status, sizeof(status));
	put_header(message, NLMSG_LENGTH(sizeof(status)), NLMSG_DONE, NLM_F_MULTI, request, port);
	*size += DONE_MESSAGE_SIZE;
}

/**
 * Prepare the answer to the tracee's request, to be sent to its @port.  This function returns
 * -errno if an error occurred, otherwise 0.
 */
static int prepare_answer(Tracee *tracee, Config *config, uint32_t port)
{
	const struct nlmsghdr *request = &config->request.header;
	const struct ifinfomsg *wanted = &config->request.link;
	bool dump = (request->nlmsg_flags & NLM_F_DUMP) == NLM_F_DUMP;
	Link *links = NULL;
	size_t nb_links = 0;
	uint8_t *answer;
	size_t size = 0;
	size_t i;

	/* A bridge dump lists bridge ports, and an app sees none.  */
	if (!dump || wanted->ifi_family != AF_BRIDGE)
		nb_links = list_links(config, &links);

	answer = talloc_size(config, nb_links * LINK_MESSAGE_SIZE + 2 * ERROR_MESSAGE_SIZE
			+ DONE_MESSAGE_SIZE);
	if (answer == NULL) {
		talloc_free(links);
		return -ENOMEM;
	}

	if (dump) {
		for (i = 0; i < nb_links; i++)
			put_link(answer, &size, &links[i], request, port, NLM_F_MULTI);
		put_done(answer, &size, request, port);
	}
	else {
		const Link *found = NULL;

		for (i = 0; i < nb_links && found == NULL; i++) {
			if (wanted->ifi_index > 0 ? links[i].index == wanted->ifi_index
			    : strcmp(links[i].name, config->request.name) == 0)
				found = &links[i];
		}

		if (wanted->ifi_index <= 0 && config->request.name[0] == '\0')
			put_error(answer, &size, -EINVAL, request, port);
		else if (found == NULL)
			put_error(answer, &size, -ENODEV, request, port);
		else {
			put_link(answer, &size, found, request, port, 0);
			if ((request->nlmsg_flags & NLM_F_ACK) != 0)
				put_error(answer, &size, 0, request, port);
		}
	}
	talloc_free(links);

	drop_answer(config);
	config->answer.fd = config->request.fd;
	config->answer.data = answer;
	config->answer.size = size;
	config->answer.offset = 0;
	config->answer.deadline = RECEIVE_DEADLINE;

	VERBOSE(tracee, 2, "netlink-route: answering RTM_GETLINK on fd %d with %zu interfaces",
		config->answer.fd, nb_links);
	return 0;
}

/**
 * Ask the tracee's kernel, with syscalls chained after its refused request, whether the socket
 * is a route netlink one and what its port is.
 */
static void ask_about_socket(Tracee *tracee, Config *config)
{
	word_t scratch = config->request.scratch;
	socklen_t address_length = sizeof(struct sockaddr_nl);
	socklen_t protocol_length = sizeof(int);
	int fd = config->request.fd;
	int status;

	status = write_data(tracee, scratch + offsetof(Scratch, address_length),
			&address_length, sizeof(address_length));
	if (status < 0)
		return;
	status = write_data(tracee, scratch + offsetof(Scratch, protocol_length),
			&protocol_length, sizeof(protocol_length));
	if (status < 0)
		return;

	status = register_chained_syscall(tracee, PR_getsockopt, fd, SOL_SOCKET, SO_PROTOCOL,
					scratch + offsetof(Scratch, protocol),
					scratch + offsetof(Scratch, protocol_length), 0);
	if (status < 0)
		return;
	status = register_chained_syscall(tracee, PR_getsockname, fd,
					scratch + offsetof(Scratch, address),
					scratch + offsetof(Scratch, address_length), 0, 0, 0);
	if (status < 0)
		return;

	/* The tracee sees the refusal unless the answer gets ready.  */
	force_chain_final_result(tracee, (word_t) -EACCES);
	config->request.state = CHECKING_PROTOCOL;
}

/**
 * Redo the SIOCGIFCONF the kernel refused, with the struct ifconf at @address in the tracee,
 * on @sock.  This function returns the result for the tracee.
 */
static int redo_ifconf(Tracee *tracee, int sock, word_t address)
{
	struct ifconf tracee_conf;
	struct ifconf conf;
	int status;

	status = read_data(tracee, &tracee_conf, address, sizeof(tracee_conf));
	if (status < 0)
		return status;

	/* Without a buffer, the kernel tells how large it has to be.  */
	conf.ifc_len = tracee_conf.ifc_buf == NULL ? 0 : tracee_conf.ifc_len;
	if (conf.ifc_len < 0)
		return -EINVAL;
	if (conf.ifc_len > 65536)
		conf.ifc_len = 65536;
	conf.ifc_buf = NULL;
	if (conf.ifc_len > 0) {
		conf.ifc_buf = talloc_size(tracee->ctx, conf.ifc_len);
		if (conf.ifc_buf == NULL)
			return -ENOMEM;
	}

	status = ioctl(sock, SIOCGIFCONF, &conf) < 0 ? -errno : 0;
	if (status == 0 && conf.ifc_buf != NULL)
		status = write_data(tracee, (word_t) tracee_conf.ifc_buf, conf.ifc_buf, conf.ifc_len);
	if (status == 0) {
		tracee_conf.ifc_len = conf.ifc_len;
		status = write_data(tracee, address, &tracee_conf, sizeof(tracee_conf));
	}

	talloc_free(conf.ifc_buf);
	return status;
}

/**
 * Redo the SIOCGIF* ioctl(2) the kernel just refused on an internet socket.
 */
static void redo_ioctl(Tracee *tracee)
{
	unsigned int request = (unsigned int) peek_reg(tracee, ORIGINAL, SYSARG_2);
	word_t address = peek_reg(tracee, ORIGINAL, SYSARG_3);
	int sock = internet_socket();
	struct ifreq interface;
	int status;

	if (!is_interface_request(request) || sock < 0)
		return;

	if (request == SIOCGIFCONF)
		status = redo_ifconf(tracee, sock, address);
	else {
		status = read_data(tracee, &interface, address, sizeof(interface));
		if (status == 0)
			status = ioctl(sock, request, &interface) < 0 ? -errno : 0;
		if (status == 0)
			status = write_data(tracee, address, &interface, sizeof(interface));
	}

	poke_reg(tracee, SYSARG_RESULT, (word_t) status);
}

/**
 * Keep stopping the tracee at each syscall as long as an answer waits for it.
 */
static void keep_stopping(Tracee *tracee, const Config *config)
{
	if (config->answer.data != NULL)
		tracee->restart_how = PTRACE_SYSCALL;
}

static int handle_sysenter_end(Tracee *tracee, Config *config)
{
	Sysnum sysnum = get_sysnum(tracee, ORIGINAL);
	int fd = (int) peek_reg(tracee, CURRENT, SYSARG_1);

	if (config->answer.data != NULL) {
		int status = wait_for_receive(tracee, config, sysnum);
		if (status < 0)
			return status;
	}

	switch (sysnum) {
	case PR_bind:
		config->binding = is_kernel_address(tracee, peek_reg(tracee, CURRENT, SYSARG_2),
						peek_reg(tracee, CURRENT, SYSARG_3));
		/* Stop at the exit stage.  */
		if (config->binding)
			tracee->restart_how = PTRACE_SYSCALL;
		return 0;

	case PR_sendto:
		if (is_kernel_address(tracee, peek_reg(tracee, CURRENT, SYSARG_5),
				      peek_reg(tracee, CURRENT, SYSARG_6)))
			check_request(tracee, config, fd, peek_reg(tracee, CURRENT, SYSARG_2),
				peek_reg(tracee, CURRENT, SYSARG_3), peek_reg(tracee, CURRENT, SYSARG_3));
		return 0;

	case PR_sendmsg:
		check_sendmsg(tracee, config, fd, peek_reg(tracee, CURRENT, SYSARG_2));
		return 0;

	default:
		return 0;
	}
}

static void handle_sysexit_end(Tracee *tracee, Config *config)
{
	Sysnum sysnum = get_sysnum(tracee, ORIGINAL);
	int result = (int) peek_reg(tracee, CURRENT, SYSARG_RESULT);

	if (config->binding) {
		config->binding = false;
		if (sysnum == PR_bind && result == -EACCES)
			(void) register_chained_syscall(tracee, PR_connect,
						peek_reg(tracee, ORIGINAL, SYSARG_1),
						peek_reg(tracee, ORIGINAL, SYSARG_2),
						peek_reg(tracee, ORIGINAL, SYSARG_3), 0, 0, 0);
	}
	else if (config->request.state == SENDING) {
		config->request.state = NO_REQUEST;
		if ((sysnum == PR_sendto || sysnum == PR_sendmsg) && result == -EACCES)
			ask_about_socket(tracee, config);
	}
	else if (sysnum == PR_ioctl && result == -EACCES)
		redo_ioctl(tracee);

	keep_stopping(tracee, config);
}

static void handle_chained_exit(Tracee *tracee, Config *config)
{
	Sysnum sysnum = get_sysnum(tracee, CURRENT);
	int result = (int) peek_reg(tracee, CURRENT, SYSARG_RESULT);
	word_t scratch = config->request.scratch;

	if (config->request.state == CHECKING_PROTOCOL && sysnum == PR_getsockopt) {
		int protocol;

		if (result == 0
		    && read_data(tracee, &protocol, scratch + offsetof(Scratch, protocol),
				 sizeof(protocol)) == 0
		    && protocol == NETLINK_ROUTE)
			config->request.state = GETTING_PORT;
		else
			config->request.state = NO_REQUEST;
	}
	else if (config->request.state == GETTING_PORT && sysnum == PR_getsockname) {
		struct sockaddr_nl address;

		config->request.state = NO_REQUEST;
		if (result == 0
		    && read_data(tracee, &address, scratch + offsetof(Scratch, address),
				 sizeof(address)) == 0
		    && address.nl_family == AF_NETLINK
		    && prepare_answer(tracee, config, address.nl_pid) == 0)
			force_chain_final_result(tracee, config->request.length);
	}

	keep_stopping(tracee, config);
}

static const ArgumentValue kernel_address_length[] = {
	{ sizeof(struct sockaddr_nl), 0 },
};
static const ArgumentFilter sendto_filter = { 5, 1, kernel_address_length };

static const ArgumentValue ioctl_requests[] = {
	{ SIOCGIFNAME,		FILTER_SYSEXIT },
	{ SIOCGIFCONF,		FILTER_SYSEXIT },
	{ SIOCGIFFLAGS,		FILTER_SYSEXIT },
	{ SIOCGIFADDR,		FILTER_SYSEXIT },
	{ SIOCGIFDSTADDR,	FILTER_SYSEXIT },
	{ SIOCGIFBRDADDR,	FILTER_SYSEXIT },
	{ SIOCGIFNETMASK,	FILTER_SYSEXIT },
	{ SIOCGIFMETRIC,	FILTER_SYSEXIT },
	{ SIOCGIFMTU,		FILTER_SYSEXIT },
	{ SIOCGIFINDEX,		FILTER_SYSEXIT },
	{ SIOCGIFTXQLEN,	FILTER_SYSEXIT },
};
static const ArgumentFilter ioctl_filter = {
	1, sizeof(ioctl_requests) / sizeof(ioctl_requests[0]), ioctl_requests
};

static FilteredSysnum filtered_sysnums[] = {
	{ PR_bind,	0,		NULL },
	{ PR_sendto,	0,		&sendto_filter },
	{ PR_sendmsg,	0,		NULL },
	{ PR_ioctl,	FILTER_SYSEXIT,	&ioctl_filter },
	FILTERED_SYSNUM_END,
};

int netlink_route_callback(Extension *extension, ExtensionEvent event,
			intptr_t data1, intptr_t data2 UNUSED)
{
	switch (event) {
	case INITIALIZATION:
	case INHERIT_CHILD:
		/* A child starts without requests or answers of its own.  */
		extension->config = talloc_zero(extension, Config);
		if (extension->config == NULL)
			return -1;
		extension->filtered_sysnums = filtered_sysnums;
		return 0;

	case INHERIT_PARENT:
		return 1;

	case SYSCALL_ENTER_END: {
		Tracee *tracee = TRACEE(extension);

		/* The syscall already failed, or the tracee is 32-bit.  */
		if ((int) data1 < 0 || is_32on64_mode(tracee))
			return 0;
		return handle_sysenter_end(tracee, talloc_get_type_abort(extension->config, Config));
	}

	case SYSCALL_EXIT_END: {
		Tracee *tracee = TRACEE(extension);

		if (!is_32on64_mode(tracee))
			handle_sysexit_end(tracee, talloc_get_type_abort(extension->config, Config));
		return 0;
	}

	case SYSCALL_CHAINED_EXIT: {
		Tracee *tracee = TRACEE(extension);

		if (!is_32on64_mode(tracee))
			handle_chained_exit(tracee, talloc_get_type_abort(extension->config, Config));
		return 0;
	}

	case NEW_STATUS: {
		int status = (int) data1;

		/* Signals would restart the tracee without stopping at its next syscall
		 * otherwise; syscall stops set that themselves.  */
		if (WIFSTOPPED(status) && ((status >> 8) & 0xffff) != (SIGTRAP | 0x80))
			keep_stopping(TRACEE(extension),
				talloc_get_type_abort(extension->config, Config));
		return 0;
	}

	default:
		return 0;
	}
}
