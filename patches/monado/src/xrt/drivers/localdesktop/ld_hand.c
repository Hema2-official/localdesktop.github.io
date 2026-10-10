// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  The hands, as the headset's runtime tracks them in Local Desktop's immersive mode.
 *
 * The app reads their joints with XR_EXT_hand_tracking every display frame, at the frame's
 * display time, in the stage space.
 *
 * @ingroup drv_localdesktop
 */

#include "ld_interface.h"
#include "ld_protocol.h"

#include "math/m_space.h"
#include "util/u_device.h"
#include "util/u_logging.h"
#include "util/u_misc.h"

#include "xrt/xrt_device.h"

#include <stdio.h>


struct ld_hand_device
{
	struct xrt_device base;

	struct ld_link *link;
	//! 0 left, 1 right.
	uint32_t hand;
};

static inline struct ld_hand_device *
ld_hand_device(struct xrt_device *xdev)
{
	return (struct ld_hand_device *)xdev;
}

static void
ld_hand_destroy(struct xrt_device *xdev)
{
	struct ld_hand_device *device = ld_hand_device(xdev);

	ld_link_reference(&device->link, NULL);
	u_device_free(&device->base);
}

static xrt_result_t
ld_hand_get_tracked_pose(struct xrt_device *xdev,
                         enum xrt_input_name name,
                         int64_t at_timestamp_ns,
                         struct xrt_space_relation *out_relation)
{
	U_LOG_XDEV_UNSUPPORTED_INPUT(xdev, U_LOGGING_WARN, name);
	return XRT_ERROR_INPUT_UNSUPPORTED;
}

static xrt_result_t
ld_hand_get_hand_tracking(struct xrt_device *xdev,
                          enum xrt_input_name name,
                          int64_t desired_timestamp_ns,
                          struct xrt_hand_joint_set *out_value,
                          int64_t *out_timestamp_ns)
{
	struct ld_hand_device *device = ld_hand_device(xdev);

	if (name != device->base.inputs[0].name) {
		U_LOG_XDEV_UNSUPPORTED_INPUT(&device->base, U_LOGGING_WARN, name);
		return XRT_ERROR_INPUT_UNSUPPORTED;
	}

	struct ld_hand hand;
	int64_t time_ns = 0;
	*out_value = (struct xrt_hand_joint_set){0};
	*out_timestamp_ns = desired_timestamp_ns;
	if (!ld_link_get_hand(device->link, device->hand, &hand, &time_ns) || (hand.flags & LD_HAND_ACTIVE) == 0) {
		return XRT_SUCCESS;
	}

	// The joints are in the stage space already, as the hand's pose is.
	m_space_relation_ident(&out_value->hand_pose);
	for (uint32_t i = 0; i < LD_HAND_JOINTS && i < XRT_HAND_JOINT_COUNT; i++) {
		const struct ld_joint *joint = &hand.joints[i];
		struct xrt_hand_joint_value *value = &out_value->values.hand_joint_set_default[i];
		value->relation = (struct xrt_space_relation){
		    .relation_flags = (enum xrt_space_relation_flags)joint->flags,
		    .pose =
		        {
		            .orientation = {joint->pose.orientation[0], joint->pose.orientation[1],
		                            joint->pose.orientation[2], joint->pose.orientation[3]},
		            .position = {joint->pose.position[0], joint->pose.position[1], joint->pose.position[2]},
		        },
		};
		value->radius = joint->radius;
	}
	out_value->is_active = true;
	*out_timestamp_ns = time_ns;
	return XRT_SUCCESS;
}

struct xrt_device *
ld_hand_create(struct ld_link *link, uint32_t hand, struct xrt_device *head)
{
	struct ld_hand_device *device = U_DEVICE_ALLOCATE(struct ld_hand_device, U_DEVICE_ALLOC_NO_FLAGS, 1, 0);
	ld_link_reference(&device->link, link);
	device->hand = hand;

	u_device_populate_function_pointers(&device->base, ld_hand_get_tracked_pose, ld_hand_destroy);
	device->base.get_hand_tracking = ld_hand_get_hand_tracking;

	bool left = hand == 0;
	device->base.name = XRT_DEVICE_HAND_TRACKER;
	device->base.device_type = XRT_DEVICE_TYPE_HAND_TRACKER;
	snprintf(device->base.str, XRT_DEVICE_NAME_LEN, "Local Desktop %s hand", left ? "left" : "right");
	snprintf(device->base.serial, XRT_DEVICE_NAME_LEN, "Local Desktop %s hand", left ? "left" : "right");
	device->base.supported.hand_tracking = true;
	device->base.inputs[0].name = left ? XRT_INPUT_HT_UNOBSTRUCTED_LEFT : XRT_INPUT_HT_UNOBSTRUCTED_RIGHT;

	// Its joints come in the same space as the head's pose.
	device->base.tracking_origin = head->tracking_origin;

	return &device->base;
}
