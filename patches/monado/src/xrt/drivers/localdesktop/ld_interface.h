// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  Interface to the Local Desktop driver.
 * @ingroup drv_localdesktop
 */

#pragma once

#include "xrt/xrt_defines.h"

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

struct ld_hello;
struct xrt_device;

/*!
 * The connection to the app, owned by the HMD device.
 * @ingroup drv_localdesktop
 */
struct ld_link;

/*!
 * Whether the app is listening, which it does while it's in immersive mode.
 * @ingroup drv_localdesktop
 */
bool
ld_link_available(void);

/*!
 * Connect to the app and take the headset's description and the buffers; NULL if that fails.
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
 * The app's description of the headset and its buffers.
 * @ingroup drv_localdesktop
 */
const struct ld_hello *
ld_link_hello(struct ld_link *link);

/*!
 * A buffer's dma-buf, which stays the link's.
 * @ingroup drv_localdesktop
 */
int
ld_link_dma_buf(struct ld_link *link, uint32_t index);

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
 * Wait for a buffer the app doesn't hold, and until it's done reading it. Once the app is gone,
 * every buffer is free; false then.
 * @ingroup drv_localdesktop
 */
bool
ld_link_acquire(struct ld_link *link, uint32_t *out_index);

/*!
 * Hand an acquired buffer to the app, with a sync file that signals once it's rendered (-1 for
 * none; it stays the caller's), and the display time and views it was rendered for.
 * @ingroup drv_localdesktop
 */
bool
ld_link_present(struct ld_link *link,
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
