// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  The headset Local Desktop shows its immersive mode on.
 * @ingroup drv_localdesktop
 */

#include "ld_interface.h"
#include "ld_protocol.h"

#include "util/u_device.h"
#include "util/u_distortion_mesh.h"
#include "util/u_logging.h"
#include "util/u_misc.h"
#include "util/u_time.h"

#include "xrt/xrt_device.h"

#include <stdio.h>


struct ld_hmd
{
	struct xrt_device base;

	struct ld_link *link;
};

static inline struct ld_hmd *
ld_hmd(struct xrt_device *xdev)
{
	return (struct ld_hmd *)xdev;
}

static void
ld_hmd_destroy(struct xrt_device *xdev)
{
	struct ld_hmd *hmd = ld_hmd(xdev);

	ld_link_destroy(&hmd->link);
	u_device_free(&hmd->base);
}

static xrt_result_t
ld_hmd_get_tracked_pose(struct xrt_device *xdev,
                        enum xrt_input_name name,
                        int64_t at_timestamp_ns,
                        struct xrt_space_relation *out_relation)
{
	struct ld_hmd *hmd = ld_hmd(xdev);

	if (name != XRT_INPUT_GENERIC_HEAD_POSE) {
		U_LOG_XDEV_UNSUPPORTED_INPUT(&hmd->base, U_LOGGING_WARN, name);
		return XRT_ERROR_INPUT_UNSUPPORTED;
	}

	ld_link_get_head(hmd->link, at_timestamp_ns, out_relation);
	return XRT_SUCCESS;
}

static xrt_result_t
ld_hmd_get_view_poses(struct xrt_device *xdev,
                      const struct xrt_vec3 *default_eye_relation,
                      int64_t at_timestamp_ns,
                      enum xrt_view_type view_type,
                      uint32_t view_count,
                      struct xrt_space_relation *out_head_relation,
                      struct xrt_fov *out_fovs,
                      struct xrt_pose *out_poses)
{
	struct ld_hmd *hmd = ld_hmd(xdev);

	if (view_count > hmd->base.hmd->view_count) {
		return XRT_ERROR_UNSUPPORTED_VIEW_TYPE;
	}

	ld_link_get_head(hmd->link, at_timestamp_ns, out_head_relation);
	ld_link_get_view_poses(hmd->link, view_count, out_poses);
	for (uint32_t i = 0; i < view_count; i++) {
		out_fovs[i] = hmd->base.hmd->distortion.fov[i];
	}
	return XRT_SUCCESS;
}

struct xrt_device *
ld_hmd_create(struct ld_link *link)
{
	const struct ld_hello *hello = ld_link_hello(link);

	enum u_device_alloc_flags flags =
	    (enum u_device_alloc_flags)(U_DEVICE_ALLOC_HMD | U_DEVICE_ALLOC_TRACKING_NONE);
	struct ld_hmd *hmd = U_DEVICE_ALLOCATE(struct ld_hmd, flags, 1, 0);
	hmd->link = link;

	u_device_populate_function_pointers(&hmd->base, ld_hmd_get_tracked_pose, ld_hmd_destroy);
	hmd->base.get_view_poses = ld_hmd_get_view_poses;

	hmd->base.name = XRT_DEVICE_GENERIC_HMD;
	hmd->base.device_type = XRT_DEVICE_TYPE_HMD;
	snprintf(hmd->base.str, XRT_DEVICE_NAME_LEN, "Local Desktop headset");
	snprintf(hmd->base.serial, XRT_DEVICE_NAME_LEN, "Local Desktop headset");
	hmd->base.inputs[0].name = XRT_INPUT_GENERIC_HEAD_POSE;
	hmd->base.supported.orientation_tracking = true;
	hmd->base.supported.position_tracking = true;

	// Poses come in the app's stage space, which has its origin on the floor.
	hmd->base.tracking_origin->type = XRT_TRACKING_TYPE_OTHER;
	snprintf(hmd->base.tracking_origin->name, XRT_TRACKING_NAME_LEN, "Local Desktop stage");

	// Frames go into the app's buffers, with the views side by side: the buffer is the screen.
	struct xrt_hmd_parts *parts = hmd->base.hmd;
	parts->screens[0].w_pixels = (int)hello->width;
	parts->screens[0].h_pixels = (int)hello->height;
	parts->screens[0].nominal_frame_interval_ns = (uint64_t)(U_TIME_1S_IN_NS / hello->refresh_rate);
	parts->view_count = hello->view_count;
	for (uint32_t i = 0; i < hello->view_count; i++) {
		parts->views[i].display.w_pixels = hello->views[i].width;
		parts->views[i].display.h_pixels = hello->views[i].height;
		parts->views[i].viewport.x_pixels = hello->views[i].x;
		parts->views[i].viewport.y_pixels = hello->views[i].y;
		parts->views[i].viewport.w_pixels = hello->views[i].width;
		parts->views[i].viewport.h_pixels = hello->views[i].height;
		parts->views[i].rot = u_device_rotation_ident;
		parts->distortion.fov[i] = (struct xrt_fov){
		    .angle_left = hello->views[i].fov.left,
		    .angle_right = hello->views[i].fov.right,
		    .angle_up = hello->views[i].fov.up,
		    .angle_down = hello->views[i].fov.down,
		};
	}
	parts->blend_modes[0] = XRT_BLEND_MODE_OPAQUE;
	parts->blend_mode_count = 1;

	// The headset's compositor corrects for its lenses.
	u_distortion_mesh_set_none(&hmd->base);

	return &hmd->base;
}

struct ld_link *
ld_hmd_get_link(struct xrt_device *xdev)
{
	if (xdev == NULL || xdev->destroy != ld_hmd_destroy) {
		return NULL;
	}
	return ld_hmd(xdev)->link;
}
