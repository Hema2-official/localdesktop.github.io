// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  The connection to Local Desktop: the headset, immersive mode, tracking and frames.
 *
 * The headset and both controllers share it, each with a reference.
 * @ingroup drv_localdesktop
 */

#include "ld_interface.h"
#include "ld_protocol.h"

#include "math/m_api.h"
#include "math/m_predict.h"
#include "math/m_space.h"
#include "os/os_time.h"
#include "util/u_debug.h"
#include "util/u_logging.h"
#include "util/u_misc.h"
#include "util/u_time.h"

#include "xrt/xrt_session.h"

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>


DEBUG_GET_ONCE_LOG_OPTION(ld_log, "LOCALDESKTOP_LOG", U_LOGGING_INFO)

#define LD_DEBUG(...) U_LOG_IFL_D(debug_get_log_option_ld_log(), __VA_ARGS__)
#define LD_INFO(...) U_LOG_IFL_I(debug_get_log_option_ld_log(), __VA_ARGS__)
#define LD_WARN(...) U_LOG_IFL_W(debug_get_log_option_ld_log(), __VA_ARGS__)
#define LD_ERROR(...) U_LOG_IFL_E(debug_get_log_option_ld_log(), __VA_ARGS__)

//! How long the app may take to describe the headset.
#define HELLO_TIMEOUT_MS 2000

//! Head poses are never predicted further than this from the app's samples.
#define MAX_PREDICTION_NS (50 * (int64_t)U_TIME_1MS_IN_NS)

//! Tracking this much older than its newest sample's time counts as lost.
#define STALE_TRACKING_NS (500 * (int64_t)U_TIME_1MS_IN_NS)

//! Room for the largest message the app sends, and then some.
#define MAX_MESSAGE_SIZE 2048


struct ld_link
{
	struct xrt_reference reference;

	//! The control connection.
	int control;
	struct ld_hello hello;

	pthread_t thread;

	//! Protects everything below.
	pthread_mutex_t mutex;
	//! Signalled when a buffer comes back and when immersive mode starts or ends.
	pthread_cond_t cond;

	//! Where the sessions hear whether the headset shows them.
	struct xrt_session_event_sink *events;
	//! What they heard last.
	bool visible, focused;

	//! The control connection is up.
	bool connected;
	//! What the app was told last.
	bool session_running;

	//! Immersive mode's channel, -1 while it's off.
	int channel;
	//! The newest buffers, which stay until newer ones come.
	struct ld_immersive immersive;
	int dma_bufs[LD_MAX_BUFFERS];
	uint64_t generation;

	bool has_tracking;
	struct ld_tracking tracking;
	bool has_controllers;
	struct ld_controllers controllers;
	bool has_hands;
	struct ld_hands hands;

	//! The app holds it: sent in a frame and not released yet.
	bool lent[LD_MAX_BUFFERS];
	//! Acquired and not presented yet.
	bool acquired[LD_MAX_BUFFERS];
	//! The sync file a buffer came back with, -1 for none.
	int release_fences[LD_MAX_BUFFERS];
	//! Where the search for a free buffer starts, so that they take turns.
	uint32_t next_buffer;
	uint64_t frame_number;
};


/*
 *
 * Messages.
 *
 */

/*!
 * One message into @p data, and the descriptors that came with it: up to @p max of them into
 * @p fds, the rest closed. Returns the message's size, 0 at the end, -1 with errno on errors.
 */
static ssize_t
receive(int sock, void *data, size_t size, int *fds, uint32_t max, uint32_t *out_count)
{
	union {
		char buffer[CMSG_SPACE(sizeof(int) * (1 + LD_MAX_BUFFERS))];
		struct cmsghdr align;
	} control;
	struct iovec iov = {.iov_base = data, .iov_len = size};
	struct msghdr msg = {
	    .msg_iov = &iov,
	    .msg_iovlen = 1,
	    .msg_control = control.buffer,
	    .msg_controllen = sizeof(control.buffer),
	};

	*out_count = 0;
	ssize_t received = recvmsg(sock, &msg, MSG_CMSG_CLOEXEC | MSG_DONTWAIT);
	if (received < 0) {
		return received;
	}

	for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c != NULL; c = CMSG_NXTHDR(&msg, c)) {
		if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS) {
			continue;
		}
		size_t count = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
		for (size_t i = 0; i < count; i++) {
			int fd;
			memcpy(&fd, CMSG_DATA(c) + i * sizeof(int), sizeof(int));
			if (*out_count < max) {
				fds[(*out_count)++] = fd;
			} else {
				close(fd);
			}
		}
	}

	return received;
}

//! One message, with @p fd (unless it's -1) as SCM_RIGHTS; never waits for room.
static bool
send_message(int sock, const void *data, size_t size, int fd)
{
	union {
		char buffer[CMSG_SPACE(sizeof(int))];
		struct cmsghdr align;
	} control;
	struct iovec iov = {.iov_base = (void *)data, .iov_len = size};
	struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};

	if (fd >= 0) {
		memset(&control, 0, sizeof(control));
		msg.msg_control = control.buffer;
		msg.msg_controllen = sizeof(control.buffer);
		struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
		c->cmsg_level = SOL_SOCKET;
		c->cmsg_type = SCM_RIGHTS;
		c->cmsg_len = CMSG_LEN(sizeof(int));
		memcpy(CMSG_DATA(c), &fd, sizeof(int));
	}

	while (sendmsg(sock, &msg, MSG_NOSIGNAL | MSG_DONTWAIT) < 0) {
		if (errno != EINTR) {
			return false;
		}
	}
	return true;
}

static void
close_all(int *fds, uint32_t count)
{
	for (uint32_t i = 0; i < count; i++) {
		if (fds[i] >= 0) {
			close(fds[i]);
			fds[i] = -1;
		}
	}
}

static struct xrt_pose
pose_from(const struct ld_pose *pose)
{
	return (struct xrt_pose){
	    .orientation = {pose->orientation[0], pose->orientation[1], pose->orientation[2], pose->orientation[3]},
	    .position = {pose->position[0], pose->position[1], pose->position[2]},
	};
}

static struct ld_pose
pose_to(const struct xrt_pose *pose)
{
	return (struct ld_pose){
	    .position = {pose->position.x, pose->position.y, pose->position.z},
	    .orientation = {pose->orientation.x, pose->orientation.y, pose->orientation.z, pose->orientation.w},
	};
}

static struct ld_fov
fov_to(const struct xrt_fov *fov)
{
	return (struct ld_fov){fov->angle_left, fov->angle_right, fov->angle_up, fov->angle_down};
}

static struct xrt_space_relation
relation_from(const struct ld_pose_sample *sample)
{
	return (struct xrt_space_relation){
	    .relation_flags = (enum xrt_space_relation_flags)sample->flags,
	    .pose = pose_from(&sample->pose),
	    .linear_velocity = {sample->linear_velocity[0], sample->linear_velocity[1], sample->linear_velocity[2]},
	    .angular_velocity = {sample->angular_velocity[0], sample->angular_velocity[1], sample->angular_velocity[2]},
	};
}

static bool
check_hello(const struct ld_hello *hello)
{
	if (hello->magic != LD_PROTOCOL_MAGIC) {
		LD_ERROR("Local Desktop didn't describe the headset");
		return false;
	}
	if (hello->version != LD_PROTOCOL_VERSION) {
		LD_ERROR("Local Desktop speaks protocol version %u, this driver %u", hello->version,
		         LD_PROTOCOL_VERSION);
		return false;
	}
	if (hello->view_count == 0 || hello->view_count > LD_MAX_VIEWS) {
		LD_ERROR("%u views", hello->view_count);
		return false;
	}
	for (uint32_t i = 0; i < hello->view_count; i++) {
		if (hello->views[i].width == 0 || hello->views[i].height == 0) {
			LD_ERROR("View %u has no size", i);
			return false;
		}
	}
	if (hello->refresh_rate <= 0.0f || hello->refresh_rate_count > LD_MAX_REFRESH_RATES) {
		LD_ERROR("Refresh rate %.1f Hz, %u choices", hello->refresh_rate, hello->refresh_rate_count);
		return false;
	}
	return true;
}

static bool
check_immersive(const struct ld_immersive *immersive, uint32_t fd_count)
{
	if (immersive->drm_format != LD_DRM_FORMAT_ABGR8888 || immersive->width == 0 || immersive->height == 0 ||
	    immersive->stride < immersive->width * 4) {
		LD_ERROR("Unusable buffers: %ux%u, %u bytes per row, format 0x%08x", immersive->width,
		         immersive->height, immersive->stride, immersive->drm_format);
		return false;
	}
	if (immersive->buffer_count == 0 || immersive->buffer_count > LD_MAX_BUFFERS ||
	    immersive->buffer_count + 1 != fd_count) {
		LD_ERROR("%u buffers, %u descriptors", immersive->buffer_count, fd_count);
		return false;
	}
	if (immersive->view_count == 0 || immersive->view_count > LD_MAX_VIEWS) {
		LD_ERROR("%u views", immersive->view_count);
		return false;
	}
	for (uint32_t i = 0; i < immersive->view_count; i++) {
		uint64_t right = (uint64_t)immersive->views[i].x + immersive->views[i].width;
		uint64_t bottom = (uint64_t)immersive->views[i].y + immersive->views[i].height;
		if (immersive->views[i].width == 0 || immersive->views[i].height == 0 || right > immersive->width ||
		    bottom > immersive->height) {
			LD_ERROR("View %u lies outside the buffers", i);
			return false;
		}
	}
	return true;
}


/*
 *
 * Immersive mode.
 *
 */

//! Immersive mode started: its channel and buffers replace those of the last.
static void
install_channel(struct ld_link *link, const struct ld_immersive *immersive, int *fds)
{
	int channel = fds[0];
	fcntl(channel, F_SETFL, fcntl(channel, F_GETFL) | O_NONBLOCK);

	pthread_mutex_lock(&link->mutex);
	if (link->channel >= 0) {
		close(link->channel);
	}
	close_all(link->dma_bufs, LD_MAX_BUFFERS);
	close_all(link->release_fences, LD_MAX_BUFFERS);
	link->channel = channel;
	link->immersive = *immersive;
	for (uint32_t i = 0; i < immersive->buffer_count; i++) {
		link->dma_bufs[i] = fds[1 + i];
	}
	for (uint32_t i = 0; i < LD_MAX_BUFFERS; i++) {
		link->lent[i] = false;
		link->acquired[i] = false;
	}
	link->next_buffer = 0;
	link->generation++;
	pthread_cond_broadcast(&link->cond);
	pthread_mutex_unlock(&link->mutex);

	LD_INFO("Immersive mode: %u buffers of %ux%u, %u views of %ux%u at %.1f Hz", immersive->buffer_count,
	        immersive->width, immersive->height, immersive->view_count, immersive->views[0].width,
	        immersive->views[0].height, immersive->refresh_rate);
}

//! Tell the sessions whether the headset shows them, if that changed.
static void
tell_sessions(struct ld_link *link, bool visible, bool focused)
{
	pthread_mutex_lock(&link->mutex);
	struct xrt_session_event_sink *events = link->events;
	bool changed = link->visible != visible || link->focused != focused;
	link->visible = visible;
	link->focused = focused;
	pthread_mutex_unlock(&link->mutex);

	if (!changed || events == NULL) {
		return;
	}
	LD_INFO("The headset %s the sessions%s", visible ? "shows" : "doesn't show",
	        focused ? ", and takes their input" : "");
	union xrt_session_event event = XRT_STRUCT_INIT;
	event.type = XRT_SESSION_EVENT_STATE_CHANGE;
	event.state.visible = visible;
	event.state.focused = focused;
	event.state.timestamp_ns = os_monotonic_get_ns();
	if (xrt_session_event_sink_push(events, &event) != XRT_SUCCESS) {
		LD_WARN("The sessions didn't take the news");
	}
}

//! Immersive mode ended: every buffer is ours again, and frames go nowhere until it's back.
static void
drop_channel(struct ld_link *link)
{
	pthread_mutex_lock(&link->mutex);
	bool had_channel = link->channel >= 0;
	if (had_channel) {
		close(link->channel);
		link->channel = -1;
	}
	for (uint32_t i = 0; i < LD_MAX_BUFFERS; i++) {
		link->lent[i] = false;
	}
	close_all(link->release_fences, LD_MAX_BUFFERS);
	pthread_cond_broadcast(&link->cond);
	pthread_mutex_unlock(&link->mutex);

	if (had_channel) {
		LD_INFO("Immersive mode ended");
		tell_sessions(link, false, false);
	}
}

static void
take_release(struct ld_link *link, const struct ld_release *release, int fence)
{
	pthread_mutex_lock(&link->mutex);
	uint32_t index = release->buffer;
	if (link->release_fences[index] >= 0) {
		close(link->release_fences[index]);
	}
	link->release_fences[index] = fence;
	link->lent[index] = false;
	pthread_cond_broadcast(&link->cond);
	pthread_mutex_unlock(&link->mutex);
}


/*
 *
 * The reader thread.
 *
 */

//! What came on the control connection; false once it's gone.
static bool
read_control(struct ld_link *link)
{
	union {
		uint32_t type;
		struct ld_immersive immersive;
		uint8_t bytes[MAX_MESSAGE_SIZE];
	} message;
	int fds[1 + LD_MAX_BUFFERS];
	uint32_t fd_count = 0;

	ssize_t size = receive(link->control, &message, sizeof(message), fds, 1 + LD_MAX_BUFFERS, &fd_count);
	if (size < 0 && (errno == EINTR || errno == EAGAIN)) {
		return true;
	}
	// The app closing the connection with messages unread resets it.
	if (size == 0 || (size < 0 && errno == ECONNRESET)) {
		LD_INFO("Local Desktop closed the connection");
		return false;
	}
	if (size < 0) {
		LD_ERROR("Reading from Local Desktop: %s", strerror(errno));
		return false;
	}

	if (size == sizeof(message.immersive) && message.type == LD_MESSAGE_IMMERSIVE &&
	    check_immersive(&message.immersive, fd_count)) {
		install_channel(link, &message.immersive, fds);
		return true;
	}

	LD_WARN("Unexpected message from Local Desktop: %zd bytes, type %u", size, size >= 4 ? message.type : 0);
	close_all(fds, fd_count);
	return true;
}

//! What came on immersive mode's channel.
static void
read_channel(struct ld_link *link, int channel)
{
	union {
		uint32_t type;
		struct ld_tracking tracking;
		struct ld_controllers controllers;
		struct ld_hands hands;
		struct ld_state state;
		struct ld_release release;
		uint8_t bytes[MAX_MESSAGE_SIZE];
	} message;
	int fd = -1;
	uint32_t fd_count = 0;

	ssize_t size = receive(channel, &message, sizeof(message), &fd, 1, &fd_count);
	if (size < 0 && (errno == EINTR || errno == EAGAIN)) {
		return;
	}
	if (size <= 0) {
		if (size < 0 && errno != ECONNRESET) {
			LD_ERROR("Reading immersive mode's channel: %s", strerror(errno));
		}
		drop_channel(link);
		return;
	}

	if (size == sizeof(message.tracking) && message.type == LD_MESSAGE_TRACKING) {
		pthread_mutex_lock(&link->mutex);
		link->tracking = message.tracking;
		link->has_tracking = true;
		pthread_mutex_unlock(&link->mutex);
	} else if (size == sizeof(message.controllers) && message.type == LD_MESSAGE_CONTROLLERS) {
		pthread_mutex_lock(&link->mutex);
		link->controllers = message.controllers;
		link->has_controllers = true;
		pthread_mutex_unlock(&link->mutex);
	} else if (size == sizeof(message.state) && message.type == LD_MESSAGE_STATE) {
		tell_sessions(link, (message.state.flags & LD_STATE_VISIBLE) != 0,
		              (message.state.flags & LD_STATE_FOCUSED) != 0);
	} else if (size == sizeof(message.hands) && message.type == LD_MESSAGE_HANDS) {
		pthread_mutex_lock(&link->mutex);
		link->hands = message.hands;
		link->has_hands = true;
		pthread_mutex_unlock(&link->mutex);
	} else if (size == sizeof(message.release) && message.type == LD_MESSAGE_RELEASE &&
	           message.release.buffer < LD_MAX_BUFFERS) {
		take_release(link, &message.release, fd_count > 0 ? fd : -1);
		fd_count = 0;
	} else {
		LD_WARN("Unexpected message in immersive mode: %zd bytes, type %u", size, size >= 4 ? message.type : 0);
	}

	if (fd_count > 0) {
		close(fd);
	}
}

static void *
run_reader(void *ptr)
{
	struct ld_link *link = (struct ld_link *)ptr;

	for (;;) {
		// Only this thread changes the channel, so it stays open while this polls it.
		pthread_mutex_lock(&link->mutex);
		int channel = link->channel;
		pthread_mutex_unlock(&link->mutex);

		struct pollfd fds[2] = {
		    {.fd = link->control, .events = POLLIN},
		    {.fd = channel, .events = POLLIN},
		};
		if (poll(fds, channel >= 0 ? 2 : 1, -1) < 0) {
			if (errno == EINTR) {
				continue;
			}
			LD_ERROR("Waiting for Local Desktop: %s", strerror(errno));
			break;
		}
		if (fds[0].revents != 0 && !read_control(link)) {
			break;
		}
		if (channel >= 0 && fds[1].revents != 0) {
			read_channel(link, channel);
		}
	}

	pthread_mutex_lock(&link->mutex);
	link->connected = false;
	pthread_mutex_unlock(&link->mutex);
	drop_channel(link);
	return NULL;
}


/*
 *
 * 'Exported' functions.
 *
 */

bool
ld_link_available(void)
{
	struct stat st;
	return stat(LD_SOCKET_PATH, &st) == 0 && S_ISSOCK(st.st_mode);
}

struct ld_link *
ld_link_create(void)
{
	int fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
	if (fd < 0) {
		LD_ERROR("socket: %s", strerror(errno));
		return NULL;
	}
	struct sockaddr_un address = {.sun_family = AF_UNIX};
	snprintf(address.sun_path, sizeof(address.sun_path), "%s", LD_SOCKET_PATH);
	if (connect(fd, (struct sockaddr *)&address, sizeof(address)) < 0) {
		LD_ERROR("Connecting to %s: %s", LD_SOCKET_PATH, strerror(errno));
		close(fd);
		return NULL;
	}

	struct ld_link *link = U_TYPED_CALLOC(struct ld_link);
	link->reference.count = 1;
	link->control = fd;
	// What the compositor tells sessions until the app says otherwise.
	link->visible = true;
	link->focused = true;
	link->channel = -1;
	for (uint32_t i = 0; i < LD_MAX_BUFFERS; i++) {
		link->dma_bufs[i] = -1;
		link->release_fences[i] = -1;
	}

	// The app describes the headset as soon as it takes the connection.
	struct pollfd readable = {.fd = fd, .events = POLLIN};
	uint32_t fd_count = 0;
	int unexpected[1];
	ssize_t size = -1;
	if (poll(&readable, 1, HELLO_TIMEOUT_MS) == 1) {
		size = receive(fd, &link->hello, sizeof(link->hello), unexpected, 1, &fd_count);
		close_all(unexpected, fd_count);
	}
	if (size != sizeof(link->hello)) {
		LD_ERROR("Local Desktop didn't describe the headset (%zd bytes)", size);
		goto error;
	}
	if (!check_hello(&link->hello)) {
		goto error;
	}

	pthread_condattr_t attributes;
	pthread_condattr_init(&attributes);
	pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC);
	pthread_cond_init(&link->cond, &attributes);
	pthread_condattr_destroy(&attributes);
	pthread_mutex_init(&link->mutex, NULL);

	link->connected = true;
	if (pthread_create(&link->thread, NULL, run_reader, link) != 0) {
		LD_ERROR("No thread for the connection");
		pthread_cond_destroy(&link->cond);
		pthread_mutex_destroy(&link->mutex);
		goto error;
	}

	const struct ld_hello *hello = &link->hello;
	LD_INFO("Headset: %u views of %ux%u at %.1f Hz%s", hello->view_count, hello->views[0].width,
	        hello->views[0].height, hello->refresh_rate,
	        (hello->flags & LD_HELLO_PASSTHROUGH) != 0 ? ", with passthrough" : "");

	return link;

error:
	close(fd);
	free(link);
	return NULL;
}

static void
destroy(struct ld_link *link)
{
	// Ends the reader's wait.
	shutdown(link->control, SHUT_RDWR);
	pthread_join(link->thread, NULL);

	close(link->control);
	close_all(link->dma_bufs, LD_MAX_BUFFERS);
	close_all(link->release_fences, LD_MAX_BUFFERS);
	pthread_cond_destroy(&link->cond);
	pthread_mutex_destroy(&link->mutex);

	free(link);
}

void
ld_link_reference(struct ld_link **dst, struct ld_link *src)
{
	struct ld_link *old = *dst;
	if (old == src) {
		return;
	}
	if (src != NULL) {
		xrt_reference_inc(&src->reference);
	}
	*dst = src;
	if (old != NULL && xrt_reference_dec_and_is_zero(&old->reference)) {
		destroy(old);
	}
}

void
ld_link_set_event_sink(struct ld_link *link, struct xrt_session_event_sink *events)
{
	pthread_mutex_lock(&link->mutex);
	link->events = events;
	pthread_mutex_unlock(&link->mutex);
}

const struct ld_hello *
ld_link_hello(struct ld_link *link)
{
	return &link->hello;
}

bool
ld_link_get_buffers(struct ld_link *link, struct ld_buffers *out_buffers)
{
	pthread_mutex_lock(&link->mutex);
	bool has_buffers = link->generation > 0;
	if (has_buffers) {
		out_buffers->generation = link->generation;
		out_buffers->description = link->immersive;
		for (uint32_t i = 0; i < LD_MAX_BUFFERS; i++) {
			out_buffers->dma_bufs[i] = link->dma_bufs[i] >= 0 ? dup(link->dma_bufs[i]) : -1;
		}
	}
	pthread_mutex_unlock(&link->mutex);
	return has_buffers;
}

bool
ld_link_has_buffers(struct ld_link *link)
{
	pthread_mutex_lock(&link->mutex);
	bool has_buffers = link->generation > 0;
	pthread_mutex_unlock(&link->mutex);
	return has_buffers;
}

void
ld_link_set_session_running(struct ld_link *link, bool running)
{
	struct ld_session session = {.type = LD_MESSAGE_SESSION, .running = running};

	pthread_mutex_lock(&link->mutex);
	bool changed = link->connected && link->session_running != running;
	if (changed && !send_message(link->control, &session, sizeof(session), -1)) {
		LD_ERROR("Telling Local Desktop about the session: %s", strerror(errno));
		changed = false;
	}
	if (changed) {
		link->session_running = running;
	}
	pthread_mutex_unlock(&link->mutex);

	if (changed) {
		LD_INFO(running ? "Apps run OpenXR sessions: asking for immersive mode"
		                : "No OpenXR sessions left: immersive mode may end");
	}
}

void
ld_link_get_head(struct ld_link *link, int64_t at_timestamp_ns, struct xrt_space_relation *out_relation)
{
	struct ld_pose_sample samples[LD_MAX_HEAD_SAMPLES];
	uint32_t count = 0;

	pthread_mutex_lock(&link->mutex);
	if (link->has_tracking) {
		count = link->tracking.sample_count;
		if (count > LD_MAX_HEAD_SAMPLES) {
			count = LD_MAX_HEAD_SAMPLES;
		}
		memcpy(samples, link->tracking.head, sizeof(samples[0]) * count);
	}
	pthread_mutex_unlock(&link->mutex);

	if (count == 0) {
		// Nothing from the app yet: standing, looking ahead, not tracked.
		*out_relation = (struct xrt_space_relation){
		    .relation_flags = (enum xrt_space_relation_flags)(XRT_SPACE_RELATION_ORIENTATION_VALID_BIT |
		                                                      XRT_SPACE_RELATION_POSITION_VALID_BIT),
		    .pose = {.orientation = {0.0f, 0.0f, 0.0f, 1.0f}, .position = {0.0f, 1.6f, 0.0f}},
		};
		return;
	}

	// Between two samples, interpolate; outside them, predict from the nearest.
	uint32_t i = 0;
	while (i + 1 < count && samples[i + 1].time_ns <= at_timestamp_ns) {
		i++;
	}
	if (i + 1 < count && at_timestamp_ns > samples[i].time_ns) {
		struct xrt_space_relation relation = relation_from(&samples[i]);
		struct xrt_space_relation next = relation_from(&samples[i + 1]);
		float t = (float)(at_timestamp_ns - samples[i].time_ns) /
		          (float)(samples[i + 1].time_ns - samples[i].time_ns);
		enum xrt_space_relation_flags flags = relation.relation_flags & next.relation_flags;
		m_space_relation_interpolate(&relation, &next, t, flags, out_relation);
	} else {
		ld_pose_sample_predict(&samples[i], at_timestamp_ns, out_relation);
	}

	// The app stopped sending (immersive mode ended, say): this is only the last known pose.
	if (os_monotonic_get_ns() - samples[count - 1].time_ns > STALE_TRACKING_NS) {
		out_relation->relation_flags &= ~(XRT_SPACE_RELATION_ORIENTATION_TRACKED_BIT |
		                                  XRT_SPACE_RELATION_POSITION_TRACKED_BIT);
	}
}

void
ld_pose_sample_predict(const struct ld_pose_sample *sample,
                       int64_t at_timestamp_ns,
                       struct xrt_space_relation *out_relation)
{
	struct xrt_space_relation relation = relation_from(sample);
	int64_t delta_ns = at_timestamp_ns - sample->time_ns;
	if (delta_ns > MAX_PREDICTION_NS) {
		delta_ns = MAX_PREDICTION_NS;
	} else if (delta_ns < -MAX_PREDICTION_NS) {
		delta_ns = -MAX_PREDICTION_NS;
	}
	m_predict_relation(&relation, time_ns_to_s(delta_ns), out_relation);

	if (os_monotonic_get_ns() - sample->time_ns > STALE_TRACKING_NS) {
		out_relation->relation_flags &= ~(XRT_SPACE_RELATION_ORIENTATION_TRACKED_BIT |
		                                  XRT_SPACE_RELATION_POSITION_TRACKED_BIT);
	}
}

bool
ld_link_get_controller(struct ld_link *link, uint32_t hand, struct ld_controller *out_controller,
                       int64_t *out_time_ns)
{
	pthread_mutex_lock(&link->mutex);
	bool has_controller = link->has_controllers && hand < 2;
	if (has_controller) {
		*out_controller = link->controllers.hands[hand];
		*out_time_ns = link->controllers.time_ns;
	}
	pthread_mutex_unlock(&link->mutex);
	return has_controller;
}

bool
ld_link_get_hand(struct ld_link *link, uint32_t hand, struct ld_hand *out_hand, int64_t *out_time_ns)
{
	pthread_mutex_lock(&link->mutex);
	bool has_hand = link->has_hands && hand < 2;
	if (has_hand) {
		*out_hand = link->hands.hands[hand];
		*out_time_ns = link->hands.time_ns;
	}
	pthread_mutex_unlock(&link->mutex);
	return has_hand;
}

bool
ld_link_request_refresh_rate(struct ld_link *link, float rate)
{
	struct ld_refresh_rate request = {.type = LD_MESSAGE_REFRESH_RATE, .rate = rate};

	// Under the lock, so that the reader can't close the channel meanwhile.
	pthread_mutex_lock(&link->mutex);
	bool sent = link->channel >= 0 && send_message(link->channel, &request, sizeof(request), -1);
	pthread_mutex_unlock(&link->mutex);

	if (sent) {
		LD_INFO("Asking for %.1f Hz", rate);
	}
	return sent;
}

void
ld_link_send_haptic(struct ld_link *link, uint32_t hand, int64_t duration_ns, float frequency, float amplitude)
{
	struct ld_haptic haptic = {
	    .type = LD_MESSAGE_HAPTIC,
	    .hand = hand,
	    .duration_ns = duration_ns,
	    .frequency = frequency,
	    .amplitude = amplitude,
	};

	// Under the lock, so that the reader can't close the channel meanwhile.
	pthread_mutex_lock(&link->mutex);
	if (link->channel >= 0 && !send_message(link->channel, &haptic, sizeof(haptic), -1)) {
		LD_DEBUG("A haptic pulse went nowhere: %s", strerror(errno));
	}
	pthread_mutex_unlock(&link->mutex);
}

void
ld_link_get_view_poses(struct ld_link *link, uint32_t view_count, struct xrt_pose *out_poses)
{
	pthread_mutex_lock(&link->mutex);
	for (uint32_t i = 0; i < view_count; i++) {
		if (link->has_tracking && i < LD_MAX_VIEWS) {
			out_poses[i] = pose_from(&link->tracking.views[i].pose);
			continue;
		}
		// Until the app sends them: eyes 63 mm apart.
		float x = view_count == 2 ? (i == 0 ? -0.0315f : 0.0315f) : 0.0f;
		out_poses[i] = (struct xrt_pose){.orientation = {0.0f, 0.0f, 0.0f, 1.0f}, .position = {x, 0.0f, 0.0f}};
	}
	pthread_mutex_unlock(&link->mutex);
}

bool
ld_link_get_timing(struct ld_link *link,
                   int64_t *out_display_time_ns,
                   int64_t *out_display_period_ns,
                   int64_t *out_latch_time_ns)
{
	pthread_mutex_lock(&link->mutex);
	bool has_tracking = link->has_tracking;
	*out_display_time_ns = link->tracking.display_time_ns;
	*out_display_period_ns = link->tracking.display_period_ns;
	*out_latch_time_ns = link->tracking.latch_time_ns;
	pthread_mutex_unlock(&link->mutex);
	return has_tracking;
}

enum ld_acquire_result
ld_link_acquire(struct ld_link *link, uint64_t generation, uint32_t *out_index)
{
	pthread_mutex_lock(&link->mutex);
	uint32_t count = link->immersive.buffer_count;
	uint32_t index = count;
	for (;;) {
		if (generation != link->generation) {
			pthread_mutex_unlock(&link->mutex);
			return LD_ACQUIRE_CHANGED;
		}
		for (uint32_t n = 0; n < count && index == count; n++) {
			uint32_t i = (link->next_buffer + n) % count;
			if (!link->lent[i] && !link->acquired[i]) {
				index = i;
			}
		}
		if (index < count) {
			break;
		}
		// Every buffer is with the app, which lasts while it doesn't show frames.
		struct timespec deadline;
		clock_gettime(CLOCK_MONOTONIC, &deadline);
		deadline.tv_sec += 1;
		if (pthread_cond_timedwait(&link->cond, &link->mutex, &deadline) == ETIMEDOUT) {
			LD_DEBUG("Waiting for the app to give a buffer back");
		}
	}
	link->acquired[index] = true;
	link->next_buffer = (index + 1) % count;
	int fence = link->release_fences[index];
	link->release_fences[index] = -1;
	pthread_mutex_unlock(&link->mutex);

	// The app may still be reading it, which ends long before the buffer's turn comes again.
	if (fence >= 0) {
		struct pollfd signalled = {.fd = fence, .events = POLLIN};
		if (poll(&signalled, 1, 1000) != 1) {
			LD_WARN("Buffer %u: the app still reads it after a second", index);
		}
		close(fence);
	}

	*out_index = index;
	return LD_ACQUIRE_OK;
}

bool
ld_link_present(struct ld_link *link,
                uint64_t generation,
                uint32_t index,
                int render_fence,
                int64_t display_time_ns,
                bool alpha_blend,
                uint32_t view_count,
                const struct xrt_pose *poses,
                const struct xrt_fov *fovs)
{
	if (index >= LD_MAX_BUFFERS) {
		return false;
	}

	struct ld_frame frame = {
	    .type = LD_MESSAGE_FRAME,
	    .buffer = index,
	    .display_time_ns = display_time_ns,
	    .flags = alpha_blend ? LD_FRAME_ALPHA_BLEND : 0,
	};
	for (uint32_t i = 0; i < view_count && i < LD_MAX_VIEWS; i++) {
		frame.views[i].pose = pose_to(&poses[i]);
		frame.views[i].fov = fov_to(&fovs[i]);
	}

	// Sent under the lock, so that the reader can't close the channel meanwhile.
	pthread_mutex_lock(&link->mutex);
	link->acquired[index] = false;
	bool sent = generation == link->generation && link->channel >= 0;
	if (sent) {
		frame.number = ++link->frame_number;
		sent = send_message(link->channel, &frame, sizeof(frame), render_fence);
		// The app may give it back as soon as the lock is let go.
		link->lent[index] = sent;
	}
	pthread_mutex_unlock(&link->mutex);
	return sent;
}
