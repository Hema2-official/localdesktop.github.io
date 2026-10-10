// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  Builder for the headset Local Desktop runs on.
 * @ingroup xrt_iface
 */

#include "xrt/xrt_config_drivers.h"
#include "xrt/xrt_prober.h"
#include "xrt/xrt_system.h"
#include "xrt/xrt_tracking.h"

#include "util/u_logging.h"
#include "util/u_misc.h"

#include "target_builder_helpers.h"
#include "target_builder_interface.h"

#include "localdesktop/ld_interface.h"

#include <stdlib.h>

#ifndef XRT_BUILD_DRIVER_LOCALDESKTOP
#error "Must only be built with XRT_BUILD_DRIVER_LOCALDESKTOP set"
#endif


static const char *driver_list[] = {
    "localdesktop",
};

static xrt_result_t
localdesktop_estimate_system(struct xrt_builder *xb,
                             cJSON *config,
                             struct xrt_prober *xp,
                             struct xrt_builder_estimate *estimate)
{
	// The app listens while it's in immersive mode.
	if (ld_link_available()) {
		estimate->certain.head = true;
		estimate->certain.left = true;
		estimate->certain.right = true;
	}
	return XRT_SUCCESS;
}

static xrt_result_t
localdesktop_open_system_impl(struct xrt_builder *xb,
                              cJSON *config,
                              struct xrt_prober *xp,
                              struct xrt_tracking_origin *origin,
                              struct xrt_system_devices *xsysd,
                              struct xrt_frame_context *xfctx,
                              struct t_builder_roles_helper *tbrh)
{
	struct ld_link *link = ld_link_create();
	if (link == NULL) {
		U_LOG_E("Local Desktop isn't in immersive mode");
		return XRT_ERROR_DEVICE_CREATION_FAILED;
	}

	struct xrt_device *head = ld_hmd_create(link);
	struct xrt_device *left = ld_controller_create(link, 0, head);
	struct xrt_device *right = ld_controller_create(link, 1, head);
	struct xrt_device *left_hand = ld_hand_create(link, 0, head);
	struct xrt_device *right_hand = ld_hand_create(link, 1, head);
	ld_link_reference(&link, NULL);

	xsysd->static_xdevs[xsysd->static_xdev_count++] = head;
	xsysd->static_xdevs[xsysd->static_xdev_count++] = left;
	xsysd->static_xdevs[xsysd->static_xdev_count++] = right;
	xsysd->static_xdevs[xsysd->static_xdev_count++] = left_hand;
	xsysd->static_xdevs[xsysd->static_xdev_count++] = right_hand;
	tbrh->head = head;
	tbrh->left = left;
	tbrh->right = right;
	tbrh->hand_tracking.unobstructed.left = left_hand;
	tbrh->hand_tracking.unobstructed.right = right_hand;

	/*
	 * The headset's compositor does the lens correction, so apps render at the size it
	 * recommends, not at the 140 % Monado defaults to for sampling its own distortion. The
	 * compositor reads this after the devices are built.
	 */
	setenv("XRT_COMPOSITOR_SCALE_PERCENTAGE", "100", 0);

	return XRT_SUCCESS;
}

static void
localdesktop_destroy(struct xrt_builder *xb)
{
	free(xb);
}

struct xrt_builder *
t_builder_localdesktop_create(void)
{
	struct t_builder *ub = U_TYPED_CALLOC(struct t_builder);

	// xrt_builder fields.
	ub->base.estimate_system = localdesktop_estimate_system;
	ub->base.open_system = t_builder_open_system_static_roles;
	ub->base.destroy = localdesktop_destroy;
	ub->base.identifier = "localdesktop";
	ub->base.name = "Local Desktop headset";
	ub->base.driver_identifiers = driver_list;
	ub->base.driver_identifier_count = ARRAY_SIZE(driver_list);

	// t_builder fields.
	ub->open_system_static_roles = localdesktop_open_system_impl;

	return &ub->base;
}
