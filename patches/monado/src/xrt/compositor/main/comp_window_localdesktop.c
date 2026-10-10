// Copyright 2026, Local Desktop contributors.
// SPDX-License-Identifier: BSL-1.0
/*!
 * @file
 * @brief  Target that renders into the buffers Local Desktop's immersive mode lends.
 *
 * The app shows each frame in an OpenXR session on the headset's own runtime, with the poses it
 * was rendered for, so the headset's compositor reprojects it and corrects for the lenses. It
 * lends new buffers each time it enters immersive mode, and enters it when the compositor's
 * session begins. See @ref drv_localdesktop.
 *
 * @ingroup comp_main
 */

#include "localdesktop/ld_interface.h"
#include "localdesktop/ld_protocol.h"

#include "main/comp_window.h"

#include "os/os_time.h"
#include "util/u_misc.h"
#include "util/u_pacing.h"
#include "util/u_time.h"
#include "vk/vk_mini_helpers.h"

#include <stdlib.h>
#include <strings.h>
#include <unistd.h>


/*!
 * The app takes the newest frame shortly after its display frame starts; frames arrive this much
 * earlier than that.
 */
#define LATCH_MARGIN_NS (2 * (int64_t)U_TIME_1MS_IN_NS)

//! Display periods closer than this to the pacer's are the same.
#define PERIOD_TOLERANCE_NS (100 * (int64_t)U_TIME_1US_IN_NS)

struct ld_target
{
	//! Base "class", so that we are a target the compositor can use.
	struct comp_target base;

	//! Owned by the HMD.
	struct ld_link *link;

	//! Follows the app's display frames.
	struct u_pacing_compositor *upc;
	//! The display period @ref upc was made for.
	int64_t upc_period_ns;

	PFN_vkGetMemoryFdPropertiesKHR vkGetMemoryFdPropertiesKHR;

	//! The app's buffers, as images; @ref comp_target::images points here.
	struct comp_target_image images[LD_MAX_BUFFERS];
	VkDeviceMemory memories[LD_MAX_BUFFERS];
	//! Which of the app's buffers they are.
	uint64_t generation;

	//! The image between acquire and present, -1 for none.
	int64_t acquired;

	//! Frames go to the app with a sync file from the renderer's semaphore; else after a queue idle.
	bool export_fences;

	bool has_init_vulkan;
};

static inline struct ld_target *
ld_target(struct comp_target *ct)
{
	return (struct ld_target *)ct;
}

static inline struct vk_bundle *
get_vk(struct ld_target *ldt)
{
	return &ldt->base.c->base.vk;
}


/*
 *
 * Images.
 *
 */

static void
destroy_images(struct ld_target *ldt)
{
	struct vk_bundle *vk = get_vk(ldt);

	for (uint32_t i = 0; i < ldt->base.image_count; i++) {
		D(ImageView, ldt->images[i].view);
		D(Image, ldt->images[i].handle);
		DF(Memory, ldt->memories[i]);
	}
	ldt->base.image_count = 0;
	ldt->base.images = NULL;
}

/*!
 * One of the app's buffers as an image: linear, with the app's row pitch. Takes @p fd, the
 * buffer's dma-buf.
 */
static VkResult
import_buffer(struct ld_target *ldt,
              const struct ld_immersive *description,
              uint32_t index,
              int fd,
              VkFormat format,
              VkImageUsageFlags usage)
{
	struct vk_bundle *vk = get_vk(ldt);
	VkImage image = VK_NULL_HANDLE;
	VkDeviceMemory memory = VK_NULL_HANDLE;
	VkImageView view = VK_NULL_HANDLE;
	VkResult ret;

	VkSubresourceLayout plane = {.offset = 0, .rowPitch = description->stride};
	VkImageDrmFormatModifierExplicitCreateInfoEXT modifier = {
	    .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
	    .drmFormatModifier = 0, // DRM_FORMAT_MOD_LINEAR
	    .drmFormatModifierPlaneCount = 1,
	    .pPlaneLayouts = &plane,
	};
	VkExternalMemoryImageCreateInfo external = {
	    .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
	    .pNext = &modifier,
	    .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
	};
	VkImageCreateInfo image_info = {
	    .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
	    .pNext = &external,
	    .imageType = VK_IMAGE_TYPE_2D,
	    .format = format,
	    .extent = {description->width, description->height, 1},
	    .mipLevels = 1,
	    .arrayLayers = 1,
	    .samples = VK_SAMPLE_COUNT_1_BIT,
	    .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
	    .usage = usage,
	    .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
	    .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
	};
	ret = vk->vkCreateImage(vk->device, &image_info, NULL, &image);
	if (ret != VK_SUCCESS) {
		COMP_ERROR(ldt->base.c, "vkCreateImage for buffer %u: %s", index, vk_result_string(ret));
		close(fd);
		return ret;
	}

	VkMemoryRequirements requirements;
	vk->vkGetImageMemoryRequirements(vk->device, image, &requirements);

	VkMemoryFdPropertiesKHR fd_properties = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
	ret = ldt->vkGetMemoryFdPropertiesKHR(vk->device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd,
	                                      &fd_properties);
	uint32_t types = requirements.memoryTypeBits & fd_properties.memoryTypeBits;
	if (ret != VK_SUCCESS || types == 0) {
		COMP_ERROR(ldt->base.c, "No memory type takes buffer %u: %s", index, vk_result_string(ret));
		close(fd);
		D(Image, image);
		return ret != VK_SUCCESS ? ret : VK_ERROR_INVALID_EXTERNAL_HANDLE;
	}

	VkImportMemoryFdInfoKHR import = {
	    .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
	    .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
	    .fd = fd,
	};
	VkMemoryDedicatedAllocateInfo dedicated = {
	    .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
	    .pNext = &import,
	    .image = image,
	};
	VkMemoryAllocateInfo allocate = {
	    .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
	    .pNext = &dedicated,
	    .allocationSize = requirements.size,
	    .memoryTypeIndex = (uint32_t)(ffs((int)types) - 1),
	};
	// Takes the descriptor when it succeeds.
	ret = vk->vkAllocateMemory(vk->device, &allocate, NULL, &memory);
	if (ret != VK_SUCCESS) {
		COMP_ERROR(ldt->base.c, "Importing buffer %u: %s", index, vk_result_string(ret));
		close(fd);
		D(Image, image);
		return ret;
	}

	ret = vk->vkBindImageMemory(vk->device, image, memory, 0);
	if (ret == VK_SUCCESS) {
		VkImageSubresourceRange range = {
		    .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
		    .levelCount = 1,
		    .layerCount = 1,
		};
		ret = vk_create_view(vk, image, VK_IMAGE_VIEW_TYPE_2D, format, range, &view);
	}
	if (ret != VK_SUCCESS) {
		COMP_ERROR(ldt->base.c, "Binding buffer %u: %s", index, vk_result_string(ret));
		D(Image, image);
		DF(Memory, memory);
		return ret;
	}

	ldt->images[index].handle = image;
	ldt->images[index].view = view;
	ldt->memories[index] = memory;
	return VK_SUCCESS;
}


/*
 *
 * Pacing.
 *
 */

/*!
 * Frames have to be in the app's hands when it takes the newest for a display frame: that's when
 * the compositor presents, and the display frame's time is when it shows.
 */
static void
follow_app_timing(struct ld_target *ldt)
{
	int64_t display_time_ns = 0;
	int64_t period_ns = 0;
	int64_t latch_time_ns = 0;
	if (!ld_link_get_timing(ldt->link, &display_time_ns, &period_ns, &latch_time_ns) || period_ns <= 0) {
		return;
	}

	int64_t difference_ns = period_ns - ldt->upc_period_ns;
	if (difference_ns > PERIOD_TOLERANCE_NS || difference_ns < -PERIOD_TOLERANCE_NS) {
		COMP_INFO(ldt->base.c, "Display period %.2f ms", time_ns_to_ms_f(period_ns));
		u_pc_destroy(&ldt->upc);
		u_pc_fake_create(period_ns, os_monotonic_get_ns(), &ldt->upc);
		ldt->upc_period_ns = period_ns;
	}

	int64_t present_time_ns = latch_time_ns - LATCH_MARGIN_NS;
	u_pc_update_vblank_from_display_control(ldt->upc, present_time_ns);
	u_pc_update_present_offset(ldt->upc, -1, display_time_ns - present_time_ns);
}


/*
 *
 * Target members.
 *
 */

static bool
target_init_pre_vulkan(struct comp_target *ct)
{
	return true; // The link is up already.
}

static bool
target_init_post_vulkan(struct comp_target *ct, uint32_t preferred_width, uint32_t preferred_height)
{
	struct ld_target *ldt = ld_target(ct);
	struct vk_bundle *vk = get_vk(ldt);

	if (!vk->has_EXT_external_memory_dma_buf || !vk->has_EXT_image_drm_format_modifier) {
		COMP_ERROR(ct->c,
		           "The Vulkan driver can't import the app's buffers: it needs "
		           "VK_EXT_external_memory_dma_buf and VK_EXT_image_drm_format_modifier");
		return false;
	}
	ldt->vkGetMemoryFdPropertiesKHR =
	    (PFN_vkGetMemoryFdPropertiesKHR)vk->vkGetDeviceProcAddr(vk->device, "vkGetMemoryFdPropertiesKHR");
	if (ldt->vkGetMemoryFdPropertiesKHR == NULL) {
		COMP_ERROR(ct->c, "No vkGetMemoryFdPropertiesKHR");
		return false;
	}

	// The renderer signals it; each present turns it into the sync file that goes with the frame.
	if (vk->has_KHR_external_semaphore_fd) {
		VkExportSemaphoreCreateInfo export_info = {
		    .sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
		    .handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
		};
		VkSemaphoreCreateInfo semaphore_info = {
		    .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
		    .pNext = &export_info,
		};
		VkResult ret =
		    vk->vkCreateSemaphore(vk->device, &semaphore_info, NULL, &ct->semaphores.render_complete);
		ldt->export_fences = ret == VK_SUCCESS;
	}
	if (!ldt->export_fences) {
		COMP_WARN(ct->c, "No sync files for frames; each waits for the GPU to go idle instead");
		ct->semaphores.render_complete = VK_NULL_HANDLE;
	}
	ct->semaphores.render_complete_is_timeline = false;

	ldt->has_init_vulkan = true;
	return true;
}

static bool
target_check_ready(struct comp_target *ct)
{
	struct ld_target *ldt = ld_target(ct);

	// Ready once immersive mode has lent buffers; until it lends newer ones, frames go nowhere.
	return ld_link_has_buffers(ldt->link);
}

static bool
target_is_shared_presentable_image(struct comp_target *ct)
{
	return false;
}

/*!
 * The views' places in the buffers and their fields of view, as immersive mode has them: the
 * app's guess of the headset may have been off.
 */
static void
update_views(struct ld_target *ldt, const struct ld_immersive *description)
{
	struct xrt_hmd_parts *parts = ldt->base.c->xdev->hmd;

	for (uint32_t i = 0; i < description->view_count && i < parts->view_count; i++) {
		parts->views[i].viewport.x_pixels = description->views[i].x;
		parts->views[i].viewport.y_pixels = description->views[i].y;
		parts->views[i].viewport.w_pixels = description->views[i].width;
		parts->views[i].viewport.h_pixels = description->views[i].height;
		parts->distortion.fov[i] = (struct xrt_fov){
		    .angle_left = description->views[i].fov.left,
		    .angle_right = description->views[i].fov.right,
		    .angle_up = description->views[i].fov.up,
		    .angle_down = description->views[i].fov.down,
		};
	}
	parts->screens[0].w_pixels = (int)description->width;
	parts->screens[0].h_pixels = (int)description->height;
}

static void
target_create_images(struct comp_target *ct,
                     const struct comp_target_create_images_info *create_info,
                     struct vk_bundle_queue *present_queue)
{
	struct ld_target *ldt = ld_target(ct);

	assert(ldt->has_init_vulkan);
	(void)present_queue;

	destroy_images(ldt);

	struct ld_buffers buffers;
	if (!ld_link_get_buffers(ldt->link, &buffers)) {
		COMP_ERROR(ct->c, "Immersive mode has lent no buffers yet");
		return;
	}
	const struct ld_immersive *description = &buffers.description;

	/*
	 * The buffers hold R, G, B, A bytes, sRGB-encoded: by the format on the graphics path, by the
	 * shaders on the compute path (which can't store to sRGB formats).
	 */
	VkFormat format = VK_FORMAT_UNDEFINED;
	for (uint32_t i = 0; i < create_info->format_count && format == VK_FORMAT_UNDEFINED; i++) {
		if (create_info->formats[i] == VK_FORMAT_R8G8B8A8_SRGB ||
		    create_info->formats[i] == VK_FORMAT_R8G8B8A8_UNORM) {
			format = create_info->formats[i];
		}
	}
	if (format == VK_FORMAT_UNDEFINED) {
		COMP_ERROR(ct->c, "The compositor takes neither R8G8B8A8_SRGB nor R8G8B8A8_UNORM");
	}

	// Each import takes its dma-buf; what isn't imported is closed.
	uint32_t imported = 0;
	for (uint32_t i = 0; i < LD_MAX_BUFFERS; i++) {
		int fd = buffers.dma_bufs[i];
		bool importing = format != VK_FORMAT_UNDEFINED && i < description->buffer_count && imported == i;
		if (importing &&
		    import_buffer(ldt, description, i, fd, format, create_info->image_usage) == VK_SUCCESS) {
			imported++;
		} else if (!importing && fd >= 0) {
			close(fd);
		}
	}
	if (imported < description->buffer_count) {
		ldt->base.image_count = imported;
		destroy_images(ldt);
		return;
	}

	update_views(ldt, description);
	ldt->generation = buffers.generation;
	ldt->base.image_count = description->buffer_count;
	ldt->base.images = ldt->images;
	ldt->base.width = description->width;
	ldt->base.height = description->height;
	ldt->base.format = format;
	ldt->base.final_layout = VK_IMAGE_LAYOUT_GENERAL;
	ldt->base.present_load_op = VK_ATTACHMENT_LOAD_OP_CLEAR;
	ldt->base.surface_transform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR;

	COMP_INFO(ct->c, "Rendering into the app's %u buffers of %ux%u (%s)", description->buffer_count,
	          description->width, description->height, format == VK_FORMAT_R8G8B8A8_SRGB ? "sRGB" : "UNORM");
}

static bool
target_has_images(struct comp_target *ct)
{
	return ct->images != NULL;
}

static VkResult
target_acquire(struct comp_target *ct, uint32_t *out_index)
{
	struct ld_target *ldt = ld_target(ct);

	assert(ldt->acquired < 0);

	uint32_t index = 0;
	if (ld_link_acquire(ldt->link, ldt->generation, &index) == LD_ACQUIRE_CHANGED) {
		// Immersive mode lent newer buffers.
		return VK_ERROR_OUT_OF_DATE_KHR;
	}

	ldt->acquired = index;
	*out_index = index;
	return VK_SUCCESS;
}

static VkResult
target_present(struct comp_target *ct,
               struct vk_bundle_queue *present_queue,
               uint32_t index,
               uint64_t timeline_semaphore_value,
               int64_t desired_present_time_ns,
               int64_t present_slop_ns)
{
	struct ld_target *ldt = ld_target(ct);
	struct vk_bundle *vk = get_vk(ldt);
	struct comp_compositor *c = ct->c;

	assert(index == ldt->acquired);
	ldt->acquired = -1;

	// Signals once the renderer is done; the export also resets the semaphore for the next frame.
	int fence = -1;
	if (ldt->export_fences) {
		VkSemaphoreGetFdInfoKHR get_fd = {
		    .sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
		    .semaphore = ct->semaphores.render_complete,
		    .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
		};
		VkResult ret = vk->vkGetSemaphoreFdKHR(vk->device, &get_fd, &fence);
		if (ret != VK_SUCCESS) {
			COMP_ERROR(c, "vkGetSemaphoreFdKHR: %s; waiting for idle queues from now on",
			           vk_result_string(ret));
			fence = -1;
			ldt->export_fences = false;
			// Unsignals it.
			vk_queue_lock(present_queue);
			vk->vkQueueWaitIdle(present_queue->queue);
			vk_queue_unlock(present_queue);
			D(Semaphore, ct->semaphores.render_complete);
		}
	}
	if (!ldt->export_fences) {
		vk_queue_lock(present_queue);
		vk->vkQueueWaitIdle(present_queue->queue);
		vk_queue_unlock(present_queue);
	}

	// Turnip flushes its caches at the end of every command buffer, so the app reads the frame
	// once the sync file signals; the image needs no queue family transfer.
	ld_link_present(ldt->link, ldt->generation, index, fence, c->frame.rendering.predicted_display_time_ns,
	                (uint32_t)c->xdev->hmd->view_count, c->base.frame_params.poses, c->base.frame_params.fovs);

	if (fence >= 0) {
		close(fence);
	}
	return VK_SUCCESS;
}

static VkResult
target_wait_for_present(struct comp_target *ct, time_duration_ns timeout_ns)
{
	return VK_ERROR_EXTENSION_NOT_PRESENT;
}

static void
target_flush(struct comp_target *ct)
{
	// No-op
}

static void
target_calc_frame_pacing(struct comp_target *ct,
                         int64_t *out_frame_id,
                         int64_t *out_wake_up_time_ns,
                         int64_t *out_desired_present_time_ns,
                         int64_t *out_present_slop_ns,
                         int64_t *out_predicted_display_time_ns)
{
	struct ld_target *ldt = ld_target(ct);

	follow_app_timing(ldt);

	int64_t frame_id = -1;
	int64_t wake_up_time_ns = 0;
	int64_t desired_present_time_ns = 0;
	int64_t present_slop_ns = 0;
	int64_t predicted_display_time_ns = 0;
	int64_t predicted_display_period_ns = 0;
	int64_t min_display_period_ns = 0;
	int64_t now_ns = os_monotonic_get_ns();

	u_pc_predict(ldt->upc,                     //
	             now_ns,                       //
	             &frame_id,                    //
	             &wake_up_time_ns,             //
	             &desired_present_time_ns,     //
	             &present_slop_ns,             //
	             &predicted_display_time_ns,   //
	             &predicted_display_period_ns, //
	             &min_display_period_ns);      //

	*out_frame_id = frame_id;
	*out_wake_up_time_ns = wake_up_time_ns;
	*out_desired_present_time_ns = desired_present_time_ns;
	*out_predicted_display_time_ns = predicted_display_time_ns;
	*out_present_slop_ns = present_slop_ns;
}

static void
target_mark_timing_point(struct comp_target *ct, enum comp_target_timing_point point, int64_t frame_id, int64_t when_ns)
{
	struct ld_target *ldt = ld_target(ct);

	switch (point) {
	case COMP_TARGET_TIMING_POINT_WAKE_UP:
		u_pc_mark_point(ldt->upc, U_TIMING_POINT_WAKE_UP, frame_id, when_ns);
		break;
	case COMP_TARGET_TIMING_POINT_BEGIN:
		u_pc_mark_point(ldt->upc, U_TIMING_POINT_BEGIN, frame_id, when_ns);
		break;
	case COMP_TARGET_TIMING_POINT_SUBMIT_BEGIN:
		u_pc_mark_point(ldt->upc, U_TIMING_POINT_SUBMIT_BEGIN, frame_id, when_ns);
		break;
	case COMP_TARGET_TIMING_POINT_SUBMIT_END:
		u_pc_mark_point(ldt->upc, U_TIMING_POINT_SUBMIT_END, frame_id, when_ns);
		break;
	default: assert(false);
	}
}

static VkResult
target_update_timings(struct comp_target *ct)
{
	return VK_SUCCESS; // The timings come with the tracking.
}

static void
target_info_gpu(struct comp_target *ct, int64_t frame_id, int64_t gpu_start_ns, int64_t gpu_end_ns, int64_t when_ns)
{
	struct ld_target *ldt = ld_target(ct);

	u_pc_info_gpu(ldt->upc, frame_id, gpu_start_ns, gpu_end_ns, when_ns);
}

static void
target_set_title(struct comp_target *ct, const char *title)
{
	// No-op
}

static void
target_set_session_running(struct comp_target *ct, bool running)
{
	struct ld_target *ldt = ld_target(ct);

	// The app enters immersive mode while apps run sessions.
	ld_link_set_session_running(ldt->link, running);
}

static xrt_result_t
target_get_refresh_rates(struct comp_target *ct, uint32_t *out_count, float *out_rates)
{
	struct ld_target *ldt = ld_target(ct);
	const struct ld_hello *hello = ld_link_hello(ldt->link);

	*out_count = 0;
	for (uint32_t i = 0; i < hello->refresh_rate_count && i < XRT_MAX_SUPPORTED_REFRESH_RATES; i++) {
		out_rates[(*out_count)++] = hello->refresh_rates[i];
	}
	return XRT_SUCCESS;
}

static xrt_result_t
target_get_current_refresh_rate(struct comp_target *ct, float *out_display_refresh_rate_hz)
{
	struct ld_target *ldt = ld_target(ct);

	int64_t display_time_ns = 0;
	int64_t period_ns = 0;
	int64_t latch_time_ns = 0;
	if (ld_link_get_timing(ldt->link, &display_time_ns, &period_ns, &latch_time_ns) && period_ns > 0) {
		*out_display_refresh_rate_hz = (float)(U_TIME_1S_IN_NS / (double)period_ns);
	} else {
		*out_display_refresh_rate_hz = ld_link_hello(ldt->link)->refresh_rate;
	}
	return XRT_SUCCESS;
}

static VkResult
target_queue_supports_present(struct comp_target *ct, struct vk_bundle_queue *queue, VkBool32 *out_supported)
{
	// Frames go out with sync files, any queue will do.
	(void)queue;
	*out_supported = VK_TRUE;
	return VK_SUCCESS;
}

static void
target_destroy(struct comp_target *ct)
{
	struct ld_target *ldt = ld_target(ct);
	struct vk_bundle *vk = get_vk(ldt);

	if (ldt->has_init_vulkan) {
		destroy_images(ldt);
		D(Semaphore, ct->semaphores.render_complete);
		ldt->has_init_vulkan = false;
	}

	u_pc_destroy(&ldt->upc);

	free(ldt);
}

static struct comp_target *
target_create(struct comp_compositor *c, struct ld_link *link)
{
	struct ld_target *ldt = U_TYPED_CALLOC(struct ld_target);

	ldt->base.name = "localdesktop";
	ldt->base.init_pre_vulkan = target_init_pre_vulkan;
	ldt->base.init_post_vulkan = target_init_post_vulkan;
	ldt->base.check_ready = target_check_ready;
	ldt->base.is_shared_presentable_image = target_is_shared_presentable_image;
	ldt->base.create_images = target_create_images;
	ldt->base.has_images = target_has_images;
	ldt->base.acquire = target_acquire;
	ldt->base.present = target_present;
	ldt->base.wait_for_present = target_wait_for_present;
	ldt->base.flush = target_flush;
	ldt->base.calc_frame_pacing = target_calc_frame_pacing;
	ldt->base.mark_timing_point = target_mark_timing_point;
	ldt->base.update_timings = target_update_timings;
	ldt->base.info_gpu = target_info_gpu;
	ldt->base.set_title = target_set_title;
	ldt->base.set_session_running = target_set_session_running;
	ldt->base.get_refresh_rates = target_get_refresh_rates;
	ldt->base.get_current_refresh_rate = target_get_current_refresh_rate;
	ldt->base.queue_supports_present = target_queue_supports_present;
	ldt->base.destroy = target_destroy;
	ldt->base.c = c;
	ldt->base.wait_for_present_supported = false;

	ldt->link = link;
	ldt->acquired = -1;

	// Until the app's first display frame says otherwise.
	ldt->upc_period_ns = (int64_t)c->settings.nominal_frame_interval_ns;
	u_pc_fake_create(ldt->upc_period_ns, os_monotonic_get_ns(), &ldt->upc);

	return &ldt->base;
}


/*
 *
 * Factory
 *
 */

static bool
factory_detect(const struct comp_target_factory *ctf, struct comp_compositor *c)
{
	return ld_hmd_get_link(c->xdev) != NULL;
}

static bool
factory_create_target(const struct comp_target_factory *ctf, struct comp_compositor *c, struct comp_target **out_ct)
{
	struct ld_link *link = ld_hmd_get_link(c->xdev);
	if (link == NULL) {
		COMP_ERROR(c, "The Local Desktop target only works with the Local Desktop headset");
		return false;
	}

	*out_ct = target_create(c, link);
	return true;
}

static const char *optional_device_extensions[] = {
    VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
    VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
    // What else VK_EXT_image_drm_format_modifier needs on the compositor's Vulkan 1.0 instance,
    // besides image_format_list and maintenance1, which the compositor asks for itself.
    VK_KHR_BIND_MEMORY_2_EXTENSION_NAME,
    VK_KHR_SAMPLER_YCBCR_CONVERSION_EXTENSION_NAME,
};

const struct comp_target_factory comp_target_factory_localdesktop = {
    .name = "Local Desktop",
    .identifier = "localdesktop",
    .requires_vulkan_for_create = false,
    .is_deferred = false,
    .required_instance_version = 0,
    .required_instance_extensions = NULL,
    .required_instance_extension_count = 0,
    .optional_device_extensions = optional_device_extensions,
    .optional_device_extension_count = ARRAY_SIZE(optional_device_extensions),
    .detect = factory_detect,
    .create_target = factory_create_target,
};
