// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  Messages between Local Desktop's immersive mode and this driver.
 *
 * Local Desktop is an Android app that runs this Linux system in proot. While it shows an
 * OpenXR session on the headset's own runtime, it listens on a SOCK_SEQPACKET socket at
 * @ref LD_SOCKET_PATH. It allocates the buffers frames go into and lends them as dma-bufs
 * (linear layout, all views side by side), and it sends the head's tracking every display
 * frame. The driver renders into the buffers and sends each frame back with the poses it was
 * rendered for, so the headset's compositor can reproject it to where the head is when it's
 * shown.
 *
 * Messages are little-endian structs, of a fixed size per type, without padding; descriptors
 * travel as SCM_RIGHTS. Times are CLOCK_MONOTONIC nanoseconds. Poses are in the app's stage
 * space (metres, Y up) unless said otherwise.
 *
 * The app's side is src/android/xr/frames.rs in Local Desktop.
 *
 * @ingroup drv_localdesktop
 */

#pragma once

#include <assert.h>
#include <stdint.h>

#define LD_SOCKET_PATH "/tmp/localdesktop-xr.sock"
#define LD_PROTOCOL_MAGIC 0x52584c44u // "LDXR"
#define LD_PROTOCOL_VERSION 2u

#define LD_MAX_BUFFERS 8
#define LD_MAX_VIEWS 2
#define LD_MAX_REFRESH_RATES 8
#define LD_MAX_HEAD_SAMPLES 4

//! DRM_FORMAT_ABGR8888: bytes R, G, B, A. The values are sRGB-encoded.
#define LD_DRM_FORMAT_ABGR8888 0x34324241u

enum ld_message_type
{
	//! Linux → app, @ref ld_frame.
	LD_MESSAGE_FRAME = 1,
	//! App → Linux, @ref ld_release.
	LD_MESSAGE_RELEASE = 2,
	//! App → Linux, @ref ld_tracking.
	LD_MESSAGE_TRACKING = 3,
};

//! Angles in radians, as XrFovf: left and down are negative.
struct ld_fov
{
	float left, right, up, down;
};

struct ld_pose
{
	float position[3];
	//! x, y, z, w.
	float orientation[4];
};

struct ld_view
{
	struct ld_pose pose;
	struct ld_fov fov;
};

/*!
 * App → Linux, first on the connection: the headset and the buffers, with one dma-buf per
 * buffer, in order.
 */
struct ld_hello
{
	uint32_t magic;
	uint32_t version;
	//! Of each buffer, all views side by side.
	uint32_t width, height;
	//! Bytes per row.
	uint32_t stride;
	uint32_t drm_format;
	uint32_t buffer_count;
	uint32_t view_count;
	struct
	{
		//! The view's rectangle in each buffer, also the size to render it at.
		uint32_t x, y, width, height;
		struct ld_fov fov;
	} views[LD_MAX_VIEWS];
	//! Hz.
	float refresh_rate;
	uint32_t refresh_rate_count;
	float refresh_rates[LD_MAX_REFRESH_RATES];
};

//! A head pose the headset's runtime predicted for a time.
struct ld_head_sample
{
	int64_t time_ns;
	struct ld_pose pose;
	//! Metres per second.
	float linear_velocity[3];
	//! Radians per second, in the stage space.
	float angular_velocity[3];
	//! enum xrt_space_relation_flags bits.
	uint32_t flags;
};

/*!
 * App → Linux, every display frame of the headset: when it shows, the views, and the head's pose
 * predicted for that time and the next few periods, in increasing time order.
 */
struct ld_tracking
{
	uint32_t type;
	uint32_t sample_count;
	//! When this display frame shows.
	int64_t display_time_ns;
	int64_t display_period_ns;
	//! When the app takes the newest frame for this display frame: frames must arrive before.
	int64_t latch_time_ns;
	//! Each view's pose relative to the head.
	struct ld_view views[LD_MAX_VIEWS];
	struct ld_head_sample head[LD_MAX_HEAD_SAMPLES];
};

/*!
 * Linux → app: a frame in one of the buffers, with a sync file that signals once it's rendered
 * (or none if it already is). The app holds the buffer until it releases it.
 */
struct ld_frame
{
	uint32_t type;
	uint32_t buffer;
	uint64_t number;
	//! The display time it was rendered for.
	int64_t display_time_ns;
	//! Each view's pose and field of view it was rendered with.
	struct ld_view views[LD_MAX_VIEWS];
};

/*!
 * App → Linux: the app is done with a buffer once the sync file that comes with it (if any)
 * signals.
 */
struct ld_release
{
	uint32_t type;
	uint32_t buffer;
};

static_assert(sizeof(struct ld_hello) == 136, "ld_hello layout");
static_assert(sizeof(struct ld_head_sample) == 64, "ld_head_sample layout");
static_assert(sizeof(struct ld_tracking) == 376, "ld_tracking layout");
static_assert(sizeof(struct ld_frame) == 112, "ld_frame layout");
static_assert(sizeof(struct ld_release) == 8, "ld_release layout");
