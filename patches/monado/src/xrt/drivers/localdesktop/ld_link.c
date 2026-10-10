// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  The connection to Local Desktop's immersive mode: buffers, tracking and frames.
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

#include <errno.h>
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
#define MAX_MESSAGE_SIZE 512


struct ld_link
{
	int fd;
	struct ld_hello hello;
	int dma_bufs[LD_MAX_BUFFERS];

	pthread_t thread;

	//! Protects everything below.
	pthread_mutex_t mutex;
	//! Signalled when a buffer comes back and when the app goes away.
	pthread_cond_t cond;

	bool connected;
	bool has_tracking;
	struct ld_tracking tracking;

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
		char buffer[CMSG_SPACE(sizeof(int) * LD_MAX_BUFFERS)];
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
	ssize_t received = recvmsg(sock, &msg, MSG_CMSG_CLOEXEC);
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

//! One message, with @p fd (unless it's -1) as SCM_RIGHTS.
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

	while (sendmsg(sock, &msg, MSG_NOSIGNAL) < 0) {
		if (errno != EINTR) {
			return false;
		}
	}
	return true;
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
relation_from(const struct ld_head_sample *sample)
{
	return (struct xrt_space_relation){
	    .relation_flags = (enum xrt_space_relation_flags)sample->flags,
	    .pose = pose_from(&sample->pose),
	    .linear_velocity = {sample->linear_velocity[0], sample->linear_velocity[1], sample->linear_velocity[2]},
	    .angular_velocity = {sample->angular_velocity[0], sample->angular_velocity[1], sample->angular_velocity[2]},
	};
}


/*
 *
 * The reader thread.
 *
 */

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

static void *
run_reader(void *ptr)
{
	struct ld_link *link = (struct ld_link *)ptr;

	union {
		uint32_t type;
		struct ld_tracking tracking;
		struct ld_release release;
		uint8_t bytes[MAX_MESSAGE_SIZE];
	} message;

	for (;;) {
		int fd = -1;
		uint32_t fd_count = 0;
		ssize_t size = receive(link->fd, &message, sizeof(message), &fd, 1, &fd_count);
		if (size < 0 && errno == EINTR) {
			continue;
		}
		// The app closing the connection with frames unread resets it.
		if (size == 0 || (size < 0 && errno == ECONNRESET)) {
			break;
		}
		if (size < 0) {
			LD_ERROR("Reading from the app: %s", strerror(errno));
			break;
		}

		if (size == sizeof(message.tracking) && message.type == LD_MESSAGE_TRACKING) {
			pthread_mutex_lock(&link->mutex);
			link->tracking = message.tracking;
			link->has_tracking = true;
			pthread_mutex_unlock(&link->mutex);
		} else if (size == sizeof(message.release) && message.type == LD_MESSAGE_RELEASE &&
		           message.release.buffer < link->hello.buffer_count) {
			take_release(link, &message.release, fd);
			fd = -1;
		} else {
			LD_WARN("Unexpected message from the app: %zd bytes, type %u", size,
			        size >= 4 ? message.type : 0);
		}

		if (fd >= 0) {
			close(fd);
		}
	}

	// Every buffer is ours again; frames rendered into them go nowhere.
	pthread_mutex_lock(&link->mutex);
	link->connected = false;
	for (uint32_t i = 0; i < LD_MAX_BUFFERS; i++) {
		link->lent[i] = false;
		if (link->release_fences[i] >= 0) {
			close(link->release_fences[i]);
			link->release_fences[i] = -1;
		}
	}
	pthread_cond_broadcast(&link->cond);
	pthread_mutex_unlock(&link->mutex);

	LD_INFO("The app left immersive mode");
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

static bool
check_hello(const struct ld_hello *hello, uint32_t fd_count)
{
	if (hello->magic != LD_PROTOCOL_MAGIC) {
		LD_ERROR("The app didn't describe the headset");
		return false;
	}
	if (hello->version != LD_PROTOCOL_VERSION) {
		LD_ERROR("The app speaks protocol version %u, this driver %u", hello->version, LD_PROTOCOL_VERSION);
		return false;
	}
	if (hello->drm_format != LD_DRM_FORMAT_ABGR8888 || hello->width == 0 || hello->height == 0 ||
	    hello->stride < hello->width * 4) {
		LD_ERROR("Unusable buffers: %ux%u, %u bytes per row, format 0x%08x", hello->width, hello->height,
		         hello->stride, hello->drm_format);
		return false;
	}
	if (hello->buffer_count == 0 || hello->buffer_count > LD_MAX_BUFFERS || hello->buffer_count != fd_count) {
		LD_ERROR("%u buffers, %u dma-bufs", hello->buffer_count, fd_count);
		return false;
	}
	if (hello->view_count == 0 || hello->view_count > LD_MAX_VIEWS) {
		LD_ERROR("%u views", hello->view_count);
		return false;
	}
	for (uint32_t i = 0; i < hello->view_count; i++) {
		uint64_t right = (uint64_t)hello->views[i].x + hello->views[i].width;
		uint64_t bottom = (uint64_t)hello->views[i].y + hello->views[i].height;
		if (hello->views[i].width == 0 || hello->views[i].height == 0 || right > hello->width ||
		    bottom > hello->height) {
			LD_ERROR("View %u lies outside the buffers", i);
			return false;
		}
	}
	if (hello->refresh_rate <= 0.0f || hello->refresh_rate_count > LD_MAX_REFRESH_RATES) {
		LD_ERROR("Refresh rate %.1f Hz, %u choices", hello->refresh_rate, hello->refresh_rate_count);
		return false;
	}
	return true;
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
	link->fd = fd;
	for (uint32_t i = 0; i < LD_MAX_BUFFERS; i++) {
		link->dma_bufs[i] = -1;
		link->release_fences[i] = -1;
	}

	// The app describes the headset as soon as it takes the connection.
	struct pollfd readable = {.fd = fd, .events = POLLIN};
	uint32_t fd_count = 0;
	ssize_t size = -1;
	if (poll(&readable, 1, HELLO_TIMEOUT_MS) == 1) {
		size = receive(fd, &link->hello, sizeof(link->hello), link->dma_bufs, LD_MAX_BUFFERS, &fd_count);
	}
	if (size != sizeof(link->hello)) {
		LD_ERROR("The app didn't describe the headset (%zd bytes)", size);
		goto error;
	}
	if (!check_hello(&link->hello, fd_count)) {
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
	LD_INFO("Headset: %u views of %ux%u at %.1f Hz; %u buffers of %ux%u", hello->view_count,
	        hello->views[0].width, hello->views[0].height, hello->refresh_rate, hello->buffer_count,
	        hello->width, hello->height);

	return link;

error:
	for (uint32_t i = 0; i < fd_count; i++) {
		close(link->dma_bufs[i]);
	}
	close(fd);
	free(link);
	return NULL;
}

void
ld_link_destroy(struct ld_link **link_ptr)
{
	struct ld_link *link = *link_ptr;
	if (link == NULL) {
		return;
	}

	// Ends the reader's wait for a message.
	shutdown(link->fd, SHUT_RDWR);
	pthread_join(link->thread, NULL);

	close(link->fd);
	for (uint32_t i = 0; i < LD_MAX_BUFFERS; i++) {
		if (link->dma_bufs[i] >= 0) {
			close(link->dma_bufs[i]);
		}
		if (link->release_fences[i] >= 0) {
			close(link->release_fences[i]);
		}
	}
	pthread_cond_destroy(&link->cond);
	pthread_mutex_destroy(&link->mutex);

	free(link);
	*link_ptr = NULL;
}

const struct ld_hello *
ld_link_hello(struct ld_link *link)
{
	return &link->hello;
}

int
ld_link_dma_buf(struct ld_link *link, uint32_t index)
{
	return index < link->hello.buffer_count ? link->dma_bufs[index] : -1;
}

void
ld_link_get_head(struct ld_link *link, int64_t at_timestamp_ns, struct xrt_space_relation *out_relation)
{
	struct ld_head_sample samples[LD_MAX_HEAD_SAMPLES];
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

	// Between two samples, interpolate; outside them, predict from the nearest, but not far.
	uint32_t i = 0;
	while (i + 1 < count && samples[i + 1].time_ns <= at_timestamp_ns) {
		i++;
	}
	struct xrt_space_relation relation = relation_from(&samples[i]);
	if (i + 1 < count && at_timestamp_ns > samples[i].time_ns) {
		struct xrt_space_relation next = relation_from(&samples[i + 1]);
		float t = (float)(at_timestamp_ns - samples[i].time_ns) /
		          (float)(samples[i + 1].time_ns - samples[i].time_ns);
		enum xrt_space_relation_flags flags = relation.relation_flags & next.relation_flags;
		m_space_relation_interpolate(&relation, &next, t, flags, out_relation);
	} else {
		int64_t delta_ns = at_timestamp_ns - samples[i].time_ns;
		if (delta_ns > MAX_PREDICTION_NS) {
			delta_ns = MAX_PREDICTION_NS;
		} else if (delta_ns < -MAX_PREDICTION_NS) {
			delta_ns = -MAX_PREDICTION_NS;
		}
		m_predict_relation(&relation, time_ns_to_s(delta_ns), out_relation);
	}

	// The app stopped sending (the headset's asleep, say): this is only the last known pose.
	if (os_monotonic_get_ns() - samples[count - 1].time_ns > STALE_TRACKING_NS) {
		out_relation->relation_flags &= ~(XRT_SPACE_RELATION_ORIENTATION_TRACKED_BIT |
		                                  XRT_SPACE_RELATION_POSITION_TRACKED_BIT);
	}
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

bool
ld_link_acquire(struct ld_link *link, uint32_t *out_index)
{
	uint32_t count = link->hello.buffer_count;
	uint32_t index = count;

	pthread_mutex_lock(&link->mutex);
	for (;;) {
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
	bool connected = link->connected;
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
	return connected;
}

bool
ld_link_present(struct ld_link *link,
                uint32_t index,
                int render_fence,
                int64_t display_time_ns,
                uint32_t view_count,
                const struct xrt_pose *poses,
                const struct xrt_fov *fovs)
{
	if (index >= link->hello.buffer_count) {
		return false;
	}

	struct ld_frame frame = {
	    .type = LD_MESSAGE_FRAME,
	    .buffer = index,
	    .display_time_ns = display_time_ns,
	};
	for (uint32_t i = 0; i < view_count && i < LD_MAX_VIEWS; i++) {
		frame.views[i].pose = pose_to(&poses[i]);
		frame.views[i].fov = fov_to(&fovs[i]);
	}

	pthread_mutex_lock(&link->mutex);
	bool connected = link->connected;
	link->acquired[index] = false;
	// Lent before it's sent: the app may give it back before sending returns.
	link->lent[index] = connected;
	frame.number = ++link->frame_number;
	pthread_mutex_unlock(&link->mutex);

	if (!connected) {
		return false;
	}
	if (!send_message(link->fd, &frame, sizeof(frame), render_fence)) {
		LD_ERROR("Sending a frame to the app: %s", strerror(errno));
		pthread_mutex_lock(&link->mutex);
		link->lent[index] = false;
		pthread_mutex_unlock(&link->mutex);
		return false;
	}
	return true;
}
