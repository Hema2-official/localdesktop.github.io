// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  Interface to the Local Desktop driver.
 * @ingroup drv_localdesktop
 */

#pragma once

#include "xrt/xrt_defines.h"

#include "ld_protocol.h"

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*!
 * @defgroup drv_localdesktop Local Desktop driver
 * @ingroup drv
 *
 * @brief The headset Local Desktop runs on: tracking from, and frames to, the OpenXR session the
 * Android app shows on the headset's own runtime. See @ref ld_protocol.h.
 */

struct xrt_device;

/*!
 * The connection to the app, owned by the HMD device.
 * @ingroup drv_localdesktop
 */
struct ld_link;

/*!
 * The buffers of the newest immersive mode, to render into.
 * @ingroup drv_localdesktop
 */
struct ld_buffers
{
	//! Counts the times immersive mode handed over buffers.
	uint64_t generation;
	struct ld_immersive description;
	//! Duplicates, the caller's to close.
	int dma_bufs[LD_MAX_BUFFERS];
};

enum ld_acquire_result
{
	LD_ACQUIRE_OK,
	//! Newer buffers came: render into those.
	LD_ACQUIRE_CHANGED,
};

/*!
 * Whether the app is listening, which it does on headsets.
 * @ingroup drv_localdesktop
 */
bool
ld_link_available(void);

/*!
 * Connect to the app and take the headset's description; NULL if that fails.
 * @ingroup drv_localdesktop
 */
struct ld_link *
ld_link_create(void);

/*!
 * @ingroup drv_localdesktop
 */
void
ld_link_destroy(struct ld_link **link_ptr);

/*!
 * The app's description of the headset.
 * @ingroup drv_localdesktop
 */
const struct ld_hello *
ld_link_hello(struct ld_link *link);

/*!
 * The buffers of the newest immersive mode; false before the first.
 * @ingroup drv_localdesktop
 */
bool
ld_link_get_buffers(struct ld_link *link, struct ld_buffers *out_buffers);

/*!
 * Whether immersive mode has handed over buffers yet.
 * @ingroup drv_localdesktop
 */
bool
ld_link_has_buffers(struct ld_link *link);

/*!
 * Tell the app whether apps run OpenXR sessions, so immersive mode should be on.
 * @ingroup drv_localdesktop
 */
void
ld_link_set_session_running(struct ld_link *link, bool running);

/*!
 * The head's pose at a time, from the app's newest predictions.
 * @ingroup drv_localdesktop
 */
void
ld_link_get_head(struct ld_link *link, int64_t at_timestamp_ns, struct xrt_space_relation *out_relation);

/*!
 * The views' poses relative to the head.
 * @ingroup drv_localdesktop
 */
void
ld_link_get_view_poses(struct ld_link *link, uint32_t view_count, struct xrt_pose *out_poses);

/*!
 * The newest display frame's timing: when it shows, the display's period, and when the app took
 * a frame for it. False until the app sent one.
 * @ingroup drv_localdesktop
 */
bool
ld_link_get_timing(struct ld_link *link,
                   int64_t *out_display_time_ns,
                   int64_t *out_display_period_ns,
                   int64_t *out_latch_time_ns);

/*!
 * Wait for a buffer of @p generation the app doesn't hold, and until it's done reading it. While
 * immersive mode is off, every buffer is free, and frames go nowhere.
 * @ingroup drv_localdesktop
 */
enum ld_acquire_result
ld_link_acquire(struct ld_link *link, uint64_t generation, uint32_t *out_index);

/*!
 * Hand an acquired buffer to the app, with a sync file that signals once it's rendered (-1 for
 * none; it stays the caller's), and the display time and views it was rendered for. False when
 * the frame goes nowhere.
 * @ingroup drv_localdesktop
 */
bool
ld_link_present(struct ld_link *link,
                uint64_t generation,
                uint32_t index,
                int render_fence,
                int64_t display_time_ns,
                uint32_t view_count,
                const struct xrt_pose *poses,
                const struct xrt_fov *fovs);

/*!
 * The HMD, which takes the link.
 * @ingroup drv_localdesktop
 */
struct xrt_device *
ld_hmd_create(struct ld_link *link);

/*!
 * The link behind a device this driver made, NULL for any other device.
 * @ingroup drv_localdesktop
 */
struct ld_link *
ld_hmd_get_link(struct xrt_device *xdev);


#ifdef __cplusplus
}
#endif
