// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  The headset's controllers, as its runtime has them in Local Desktop's immersive mode.
 *
 * Touch controllers: the app reads them through the oculus/touch_controller profile, sends their
 * state every display frame, and passes haptic pulses on.
 *
 * @ingroup drv_localdesktop
 */

#include "ld_interface.h"
#include "ld_protocol.h"

#include "util/u_device.h"
#include "util/u_logging.h"
#include "util/u_misc.h"

#include "xrt/xrt_device.h"

#include <stdio.h>


static struct xrt_binding_input_pair simple_inputs[] = {
    {XRT_INPUT_SIMPLE_SELECT_CLICK, XRT_INPUT_TOUCH_TRIGGER_VALUE},
    {XRT_INPUT_SIMPLE_MENU_CLICK, XRT_INPUT_TOUCH_MENU_CLICK},
    {XRT_INPUT_SIMPLE_GRIP_POSE, XRT_INPUT_TOUCH_GRIP_POSE},
    {XRT_INPUT_SIMPLE_AIM_POSE, XRT_INPUT_TOUCH_AIM_POSE},
};

static struct xrt_binding_output_pair simple_outputs[] = {
    {XRT_OUTPUT_NAME_SIMPLE_VIBRATION, XRT_OUTPUT_NAME_TOUCH_HAPTIC},
};

static struct xrt_binding_profile binding_profiles[] = {
    {
        .name = XRT_DEVICE_SIMPLE_CONTROLLER,
        .inputs = simple_inputs,
        .input_count = ARRAY_SIZE(simple_inputs),
        .outputs = simple_outputs,
        .output_count = ARRAY_SIZE(simple_outputs),
    },
};

//! The inputs, in this order on both hands; the first five are each hand's own.
enum ld_input
{
	//! A or X.
	LD_INPUT_LOWER_CLICK,
	LD_INPUT_LOWER_TOUCH,
	//! B or Y.
	LD_INPUT_UPPER_CLICK,
	LD_INPUT_UPPER_TOUCH,
	//! Menu on the left; system on the right, which the headset keeps for itself.
	LD_INPUT_MENU_CLICK,
	LD_INPUT_SQUEEZE_VALUE,
	LD_INPUT_TRIGGER_TOUCH,
	LD_INPUT_TRIGGER_VALUE,
	LD_INPUT_THUMBSTICK_CLICK,
	LD_INPUT_THUMBSTICK_TOUCH,
	LD_INPUT_THUMBSTICK,
	LD_INPUT_THUMBREST_TOUCH,
	LD_INPUT_GRIP_POSE,
	LD_INPUT_AIM_POSE,
	LD_INPUT_COUNT,
};

static const enum xrt_input_name left_inputs[] = {
    XRT_INPUT_TOUCH_X_CLICK, XRT_INPUT_TOUCH_X_TOUCH,    XRT_INPUT_TOUCH_Y_CLICK,
    XRT_INPUT_TOUCH_Y_TOUCH, XRT_INPUT_TOUCH_MENU_CLICK,
};

static const enum xrt_input_name right_inputs[] = {
    XRT_INPUT_TOUCH_A_CLICK, XRT_INPUT_TOUCH_A_TOUCH,      XRT_INPUT_TOUCH_B_CLICK,
    XRT_INPUT_TOUCH_B_TOUCH, XRT_INPUT_TOUCH_SYSTEM_CLICK,
};

static const enum xrt_input_name common_inputs[] = {
    XRT_INPUT_TOUCH_SQUEEZE_VALUE,    XRT_INPUT_TOUCH_TRIGGER_TOUCH,    XRT_INPUT_TOUCH_TRIGGER_VALUE,
    XRT_INPUT_TOUCH_THUMBSTICK_CLICK, XRT_INPUT_TOUCH_THUMBSTICK_TOUCH, XRT_INPUT_TOUCH_THUMBSTICK,
    XRT_INPUT_TOUCH_THUMBREST_TOUCH,  XRT_INPUT_TOUCH_GRIP_POSE,        XRT_INPUT_TOUCH_AIM_POSE,
};

struct ld_controller_device
{
	struct xrt_device base;

	struct ld_link *link;
	//! 0 left, 1 right.
	uint32_t hand;
};

static inline struct ld_controller_device *
ld_controller_device(struct xrt_device *xdev)
{
	return (struct ld_controller_device *)xdev;
}

static void
ld_controller_destroy(struct xrt_device *xdev)
{
	struct ld_controller_device *controller = ld_controller_device(xdev);

	ld_link_reference(&controller->link, NULL);
	u_device_free(&controller->base);
}

static xrt_result_t
ld_controller_get_tracked_pose(struct xrt_device *xdev,
                               enum xrt_input_name name,
                               int64_t at_timestamp_ns,
                               struct xrt_space_relation *out_relation)
{
	struct ld_controller_device *controller = ld_controller_device(xdev);

	if (name != XRT_INPUT_TOUCH_GRIP_POSE && name != XRT_INPUT_TOUCH_AIM_POSE) {
		U_LOG_XDEV_UNSUPPORTED_INPUT(&controller->base, U_LOGGING_WARN, name);
		return XRT_ERROR_INPUT_UNSUPPORTED;
	}

	struct ld_controller state;
	int64_t time_ns = 0;
	if (!ld_link_get_controller(controller->link, controller->hand, &state, &time_ns) ||
	    (state.flags & LD_CONTROLLER_ACTIVE) == 0) {
		// Nowhere: not in a hand, or not on.
		*out_relation = (struct xrt_space_relation){.pose = {.orientation = {0.0f, 0.0f, 0.0f, 1.0f}}};
		return XRT_SUCCESS;
	}

	const struct ld_pose_sample *sample = name == XRT_INPUT_TOUCH_GRIP_POSE ? &state.grip : &state.aim;
	ld_pose_sample_predict(sample, at_timestamp_ns, out_relation);
	return XRT_SUCCESS;
}

static void
set_bool(struct xrt_input *input, int64_t time_ns, uint32_t buttons, uint32_t button)
{
	input->timestamp = time_ns;
	input->value.boolean = (buttons & button) != 0;
}

static void
set_float(struct xrt_input *input, int64_t time_ns, float value)
{
	input->timestamp = time_ns;
	input->value.vec1.x = value;
}

static xrt_result_t
ld_controller_update_inputs(struct xrt_device *xdev)
{
	struct ld_controller_device *controller = ld_controller_device(xdev);

	struct ld_controller state;
	int64_t time_ns = 0;
	if (!ld_link_get_controller(controller->link, controller->hand, &state, &time_ns)) {
		return XRT_SUCCESS;
	}
	// A controller the runtime doesn't have is let go.
	if ((state.flags & LD_CONTROLLER_ACTIVE) == 0) {
		state = (struct ld_controller){0};
	}

	struct xrt_input *inputs = controller->base.inputs;
	uint32_t buttons = state.buttons;
	set_bool(&inputs[LD_INPUT_LOWER_CLICK], time_ns, buttons, LD_BUTTON_LOWER_CLICK);
	set_bool(&inputs[LD_INPUT_LOWER_TOUCH], time_ns, buttons, LD_BUTTON_LOWER_TOUCH);
	set_bool(&inputs[LD_INPUT_UPPER_CLICK], time_ns, buttons, LD_BUTTON_UPPER_CLICK);
	set_bool(&inputs[LD_INPUT_UPPER_TOUCH], time_ns, buttons, LD_BUTTON_UPPER_TOUCH);
	set_bool(&inputs[LD_INPUT_MENU_CLICK], time_ns, buttons, LD_BUTTON_MENU_CLICK);
	set_bool(&inputs[LD_INPUT_TRIGGER_TOUCH], time_ns, buttons, LD_BUTTON_TRIGGER_TOUCH);
	set_bool(&inputs[LD_INPUT_THUMBSTICK_CLICK], time_ns, buttons, LD_BUTTON_THUMBSTICK_CLICK);
	set_bool(&inputs[LD_INPUT_THUMBSTICK_TOUCH], time_ns, buttons, LD_BUTTON_THUMBSTICK_TOUCH);
	set_bool(&inputs[LD_INPUT_THUMBREST_TOUCH], time_ns, buttons, LD_BUTTON_THUMBREST_TOUCH);
	set_float(&inputs[LD_INPUT_SQUEEZE_VALUE], time_ns, state.squeeze);
	set_float(&inputs[LD_INPUT_TRIGGER_VALUE], time_ns, state.trigger);
	inputs[LD_INPUT_THUMBSTICK].timestamp = time_ns;
	inputs[LD_INPUT_THUMBSTICK].value.vec2.x = state.thumbstick[0];
	inputs[LD_INPUT_THUMBSTICK].value.vec2.y = state.thumbstick[1];

	return XRT_SUCCESS;
}

static xrt_result_t
ld_controller_set_output(struct xrt_device *xdev, enum xrt_output_name name, const struct xrt_output_value *value)
{
	struct ld_controller_device *controller = ld_controller_device(xdev);

	if (name != XRT_OUTPUT_NAME_TOUCH_HAPTIC || value->type != XRT_OUTPUT_VALUE_TYPE_VIBRATION) {
		U_LOG_XDEV_UNSUPPORTED_OUTPUT(&controller->base, U_LOGGING_WARN, name);
		return XRT_ERROR_OUTPUT_UNSUPPORTED;
	}

	ld_link_send_haptic(controller->link, controller->hand, value->vibration.duration_ns,
	                    value->vibration.frequency, value->vibration.amplitude);
	return XRT_SUCCESS;
}

struct xrt_device *
ld_controller_create(struct ld_link *link, uint32_t hand, struct xrt_device *head)
{
	struct ld_controller_device *controller =
	    U_DEVICE_ALLOCATE(struct ld_controller_device, U_DEVICE_ALLOC_NO_FLAGS, LD_INPUT_COUNT, 1);
	ld_link_reference(&controller->link, link);
	controller->hand = hand;

	u_device_populate_function_pointers(&controller->base, ld_controller_get_tracked_pose,
	                                    ld_controller_destroy);
	controller->base.update_inputs = ld_controller_update_inputs;
	controller->base.set_output = ld_controller_set_output;

	bool left = hand == 0;
	controller->base.name = XRT_DEVICE_TOUCH_CONTROLLER;
	controller->base.device_type =
	    left ? XRT_DEVICE_TYPE_LEFT_HAND_CONTROLLER : XRT_DEVICE_TYPE_RIGHT_HAND_CONTROLLER;
	snprintf(controller->base.str, XRT_DEVICE_NAME_LEN, "Local Desktop %s controller", left ? "left" : "right");
	snprintf(controller->base.serial, XRT_DEVICE_NAME_LEN, "Local Desktop %s controller",
	         left ? "left" : "right");
	controller->base.supported.orientation_tracking = true;
	controller->base.supported.position_tracking = true;

	// Its poses come in the same space as the head's.
	controller->base.tracking_origin = head->tracking_origin;

	const enum xrt_input_name *own = left ? left_inputs : right_inputs;
	for (uint32_t i = 0; i < ARRAY_SIZE(left_inputs); i++) {
		controller->base.inputs[i].name = own[i];
	}
	for (uint32_t i = 0; i < ARRAY_SIZE(common_inputs); i++) {
		controller->base.inputs[LD_INPUT_SQUEEZE_VALUE + i].name = common_inputs[i];
	}
	controller->base.outputs[0].name = XRT_OUTPUT_NAME_TOUCH_HAPTIC;

	controller->base.binding_profiles = binding_profiles;
	controller->base.binding_profile_count = ARRAY_SIZE(binding_profiles);

	return &controller->base;
}
