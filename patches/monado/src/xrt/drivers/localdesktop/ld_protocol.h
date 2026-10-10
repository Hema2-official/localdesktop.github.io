// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  Messages between Local Desktop and this driver.
 *
 * Local Desktop is an Android app that runs this Linux system in proot. On a headset it listens
 * on a SOCK_SEQPACKET socket at @ref LD_SOCKET_PATH, the control connection: it describes the
 * headset, the driver says when apps run OpenXR sessions, and the app switches to immersive mode
 * for them, an OpenXR session of its own on the headset's runtime. Then it hands over a channel
 * (another SOCK_SEQPACKET socket) and the buffers frames go into, as dma-bufs (linear layout, all
 * views side by side). On the channel the app sends the head's tracking every display frame, and
 * the driver sends each frame with the poses it was rendered for, so the headset's compositor can
 * reproject it to where the head is when it's shown. The controllers' state and the hands'
 * joints come every display frame too, and haptic pulses and refresh rate requests go back. The channel closes when
 * immersive mode ends.
 *
 * Messages are little-endian structs, of a fixed size per type, without padding; descriptors
 * travel as SCM_RIGHTS. Times are CLOCK_MONOTONIC nanoseconds. Poses are in the app's stage space
 * (metres, Y up) unless said otherwise.
 *
 * The app's side is src/android/guest/xr.rs and src/android/xr/frames.rs in Local Desktop.
 *
 * @ingroup drv_localdesktop
 */

#pragma once

#include <assert.h>
#include <stdint.h>

#define LD_SOCKET_PATH "/tmp/localdesktop-xr.sock"
#define LD_PROTOCOL_MAGIC 0x52584c44u // "LDXR"
#define LD_PROTOCOL_VERSION 4u

#define LD_MAX_BUFFERS 8
#define LD_MAX_VIEWS 2
#define LD_MAX_REFRESH_RATES 8
#define LD_MAX_HEAD_SAMPLES 4
//! As XR_EXT_hand_tracking has them, in its order.
#define LD_HAND_JOINTS 26

//! DRM_FORMAT_ABGR8888: bytes R, G, B, A. The values are sRGB-encoded.
#define LD_DRM_FORMAT_ABGR8888 0x34324241u

enum ld_message_type
{
	//! Channel, Linux → app: @ref ld_frame.
	LD_MESSAGE_FRAME = 1,
	//! Channel, app → Linux: @ref ld_release.
	LD_MESSAGE_RELEASE = 2,
	//! Channel, app → Linux: @ref ld_tracking.
	LD_MESSAGE_TRACKING = 3,
	//! Control, app → Linux: @ref ld_immersive.
	LD_MESSAGE_IMMERSIVE = 4,
	//! Control, Linux → app: @ref ld_session.
	LD_MESSAGE_SESSION = 5,
	//! Channel, app → Linux: @ref ld_controllers.
	LD_MESSAGE_CONTROLLERS = 6,
	//! Channel, Linux → app: @ref ld_haptic.
	LD_MESSAGE_HAPTIC = 7,
	//! Channel, Linux → app: @ref ld_refresh_rate.
	LD_MESSAGE_REFRESH_RATE = 8,
	//! Channel, app → Linux: @ref ld_hands.
	LD_MESSAGE_HANDS = 9,
};

//! A controller's buttons and touches, in @ref ld_controller::buttons.
enum ld_button
{
	//! A on the right controller, X on the left.
	LD_BUTTON_LOWER_CLICK = 1u << 0u,
	LD_BUTTON_LOWER_TOUCH = 1u << 1u,
	//! B on the right controller, Y on the left.
	LD_BUTTON_UPPER_CLICK = 1u << 2u,
	LD_BUTTON_UPPER_TOUCH = 1u << 3u,
	//! The left controller's menu button.
	LD_BUTTON_MENU_CLICK = 1u << 4u,
	LD_BUTTON_TRIGGER_TOUCH = 1u << 5u,
	LD_BUTTON_THUMBSTICK_CLICK = 1u << 6u,
	LD_BUTTON_THUMBSTICK_TOUCH = 1u << 7u,
	LD_BUTTON_THUMBREST_TOUCH = 1u << 8u,
};

//! In @ref ld_controller::flags: the runtime has the controller (in a hand, or at least on).
#define LD_CONTROLLER_ACTIVE 1u

//! In @ref ld_hand::flags: the runtime tracks the hand.
#define LD_HAND_ACTIVE 1u

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
 * Control, app → Linux, first on the connection: the headset as the app last saw it in immersive
 * mode, or its best guess before that. Each view's size is the one to render it at.
 */
struct ld_hello
{
	uint32_t magic;
	uint32_t version;
	uint32_t view_count;
	uint32_t refresh_rate_count;
	struct
	{
		uint32_t width, height;
		struct ld_fov fov;
	} views[LD_MAX_VIEWS];
	//! Hz.
	float refresh_rate;
	float refresh_rates[LD_MAX_REFRESH_RATES];
};

/*!
 * Control, app → Linux: immersive mode is on. With it come the channel (the first descriptor)
 * and a dma-buf per buffer, in order.
 */
struct ld_immersive
{
	uint32_t type;
	uint32_t buffer_count;
	//! Of each buffer, all views side by side.
	uint32_t width, height;
	//! Bytes per row.
	uint32_t stride;
	uint32_t drm_format;
	uint32_t view_count;
	uint32_t reserved;
	struct
	{
		//! The view's rectangle in each buffer, also the size to render it at.
		uint32_t x, y, width, height;
		struct ld_fov fov;
	} views[LD_MAX_VIEWS];
	//! Hz.
	float refresh_rate;
};

//! Control, Linux → app: whether apps run OpenXR sessions, so immersive mode should be on.
struct ld_session
{
	uint32_t type;
	uint32_t running;
};

//! A pose the headset's runtime predicted for a time, with its velocities.
struct ld_pose_sample
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
 * Channel, app → Linux, every display frame of the headset: when it shows, the views, and the
 * head's pose predicted for that time and the next few periods, in increasing time order.
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
	struct ld_pose_sample head[LD_MAX_HEAD_SAMPLES];
};

struct ld_controller
{
	uint32_t flags;
	uint32_t buttons;
	//! 0 to 1.
	float trigger, squeeze;
	//! -1 to 1, right and up.
	float thumbstick[2];
	struct ld_pose_sample grip, aim;
};

/*!
 * Channel, app → Linux, every display frame: the controllers' state, and their poses predicted for
 * the frame's display time.
 */
struct ld_controllers
{
	uint32_t type;
	uint32_t reserved;
	//! When the state was read.
	int64_t time_ns;
	//! Left, right.
	struct ld_controller hands[2];
};

//! Channel, Linux → app: vibrate a controller, or stop it (amplitude 0).
struct ld_haptic
{
	uint32_t type;
	//! 0 left, 1 right.
	uint32_t hand;
	//! -1 for the shortest the runtime makes.
	int64_t duration_ns;
	//! Hz, 0 for the runtime's choice.
	float frequency;
	//! 0 to 1.
	float amplitude;
};

struct ld_joint
{
	struct ld_pose pose;
	//! Metres.
	float radius;
	//! enum xrt_space_relation_flags bits.
	uint32_t flags;
};

struct ld_hand
{
	uint32_t flags;
	uint32_t reserved;
	struct ld_joint joints[LD_HAND_JOINTS];
};

//! Channel, app → Linux, every display frame: the hands' joints at the frame's display time.
struct ld_hands
{
	uint32_t type;
	uint32_t reserved;
	int64_t time_ns;
	//! Left, right.
	struct ld_hand hands[2];
};

//! Channel, Linux → app: switch the display to one of its refresh rates.
struct ld_refresh_rate
{
	uint32_t type;
	//! Hz.
	float rate;
};

/*!
 * Channel, Linux → app: a frame in one of the buffers, with a sync file that signals once it's
 * rendered (or none if it already is). The app holds the buffer until it releases it.
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
 * Channel, app → Linux: the app is done with a buffer once the sync file that comes with it (if
 * any) signals.
 */
struct ld_release
{
	uint32_t type;
	uint32_t buffer;
};

static_assert(sizeof(struct ld_hello) == 100, "ld_hello layout");
static_assert(sizeof(struct ld_immersive) == 100, "ld_immersive layout");
static_assert(sizeof(struct ld_session) == 8, "ld_session layout");
static_assert(sizeof(struct ld_pose_sample) == 64, "ld_pose_sample layout");
static_assert(sizeof(struct ld_tracking) == 376, "ld_tracking layout");
static_assert(sizeof(struct ld_frame) == 112, "ld_frame layout");
static_assert(sizeof(struct ld_release) == 8, "ld_release layout");
static_assert(sizeof(struct ld_controller) == 152, "ld_controller layout");
static_assert(sizeof(struct ld_controllers) == 320, "ld_controllers layout");
static_assert(sizeof(struct ld_haptic) == 24, "ld_haptic layout");
static_assert(sizeof(struct ld_refresh_rate) == 8, "ld_refresh_rate layout");
static_assert(sizeof(struct ld_joint) == 36, "ld_joint layout");
static_assert(sizeof(struct ld_hands) == 1904, "ld_hands layout");
