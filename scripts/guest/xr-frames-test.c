/* Render a test pattern into the frame buffers immersive mode lends to Linux
 * (src/android/xr/frames.rs), with Vulkan: the transport alone, without Monado.
 *
 * Build in the rootfs:  gcc -O2 -o xr-frames-test xr-frames-test.c -lvulkan -lm
 * Run while the app is in immersive mode:  xr-frames-test [SECONDS]
 *
 * The buffers are dma-bufs with a linear layout, imported with VK_EXT_image_drm_format_modifier.
 * Each frame goes back with a sync file that signals when it's rendered; the app returns buffers
 * with a sync file of its own, which this program waits on before rendering into them again.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <math.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#define SOCKET_PATH "/tmp/localdesktop-xr.sock"
#define MAX_BUFFERS 8
#define HELLO 0x52584c44u /* "LDXR" */
#define FRAME 1u
#define RELEASE 2u
#define DRM_FORMAT_ABGR8888 0x34324241u
#define DRM_FORMAT_MOD_LINEAR 0ull

struct hello {
    uint32_t magic, version, width, height, stride, format, count;
};

struct frame {
    uint32_t type, buffer;
    uint64_t number;
};

struct release {
    uint32_t type, buffer;
};

#define CHECK(call)                                                       \
    do {                                                                  \
        VkResult result_ = (call);                                        \
        if (result_ != VK_SUCCESS) {                                      \
            fprintf(stderr, "%s failed: %d\n", #call, (int)result_);       \
            exit(1);                                                      \
        }                                                                 \
    } while (0)

/* One message and the descriptors that came with it (at most max). */
static ssize_t receive(int sock, void *data, size_t size, int *fds, int *count, int max, int flags)
{
    char control[CMSG_SPACE(sizeof(int) * MAX_BUFFERS)];
    struct iovec iov = {data, size};
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control,
                         .msg_controllen = sizeof(control)};
    ssize_t received = recvmsg(sock, &msg, flags | MSG_CMSG_CLOEXEC);
    *count = 0;
    if (received < 0)
        return received;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
        if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS)
            continue;
        int n = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
        for (int i = 0; i < n; i++) {
            int fd;
            memcpy(&fd, CMSG_DATA(c) + i * sizeof(int), sizeof(int));
            if (*count < max)
                fds[(*count)++] = fd;
            else
                close(fd);
        }
    }
    return received;
}

/* One message, with fd (if not -1) as SCM_RIGHTS. */
static int send_with_fd(int sock, const void *data, size_t size, int fd)
{
    char control[CMSG_SPACE(sizeof(int))];
    struct iovec iov = {(void *)data, size};
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1};
    if (fd >= 0) {
        memset(control, 0, sizeof(control));
        msg.msg_control = control;
        msg.msg_controllen = sizeof(control);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &fd, sizeof(int));
    }
    return sendmsg(sock, &msg, MSG_NOSIGNAL) < 0 ? -1 : 0;
}

static double now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

int main(int argc, char **argv)
{
    double run_for = argc > 1 ? atof(argv[1]) : 30;

    int sock = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    strcpy(address.sun_path, SOCKET_PATH);
    if (sock < 0 || connect(sock, (struct sockaddr *)&address, sizeof(address)) < 0) {
        perror("connect " SOCKET_PATH);
        return 1;
    }

    struct hello hello;
    int dmabufs[MAX_BUFFERS], count;
    if (receive(sock, &hello, sizeof(hello), dmabufs, &count, MAX_BUFFERS, 0) != sizeof(hello) ||
        hello.magic != HELLO || hello.version != 1 || hello.format != DRM_FORMAT_ABGR8888 ||
        hello.count == 0 || (int)hello.count != count) {
        fprintf(stderr, "unexpected hello (%d descriptors)\n", count);
        return 1;
    }
    printf("%u buffers of %ux%u, %u bytes per row\n", hello.count, hello.width, hello.height,
           hello.stride);

    /* Vulkan 1.3, for dynamic rendering. */
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "xr-frames-test",
                             .apiVersion = VK_API_VERSION_1_3};
    VkInstanceCreateInfo instance_info = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                          .pApplicationInfo = &app};
    VkInstance instance;
    CHECK(vkCreateInstance(&instance_info, NULL, &instance));
    uint32_t gpus = 1;
    VkPhysicalDevice gpu;
    VkResult enumerated = vkEnumeratePhysicalDevices(instance, &gpus, &gpu);
    if ((enumerated != VK_SUCCESS && enumerated != VK_INCOMPLETE) || gpus == 0) {
        fprintf(stderr, "no Vulkan device\n");
        return 1;
    }
    VkPhysicalDeviceProperties properties;
    vkGetPhysicalDeviceProperties(gpu, &properties);
    printf("rendering on %s\n", properties.deviceName);

    uint32_t families = 8;
    VkQueueFamilyProperties family_properties[8];
    vkGetPhysicalDeviceQueueFamilyProperties(gpu, &families, family_properties);
    uint32_t family = 0;
    while (family < families && !(family_properties[family].queueFlags & VK_QUEUE_GRAPHICS_BIT))
        family++;

    const char *extensions[] = {
        VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,      VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
        VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME, VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME,
        VK_EXT_QUEUE_FAMILY_FOREIGN_EXTENSION_NAME,
    };
    float priority = 1;
    VkDeviceQueueCreateInfo queue_info = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                          .queueFamilyIndex = family,
                                          .queueCount = 1,
                                          .pQueuePriorities = &priority};
    VkPhysicalDeviceVulkan13Features features13 = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE};
    VkDeviceCreateInfo device_info = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                                      .pNext = &features13,
                                      .queueCreateInfoCount = 1,
                                      .pQueueCreateInfos = &queue_info,
                                      .enabledExtensionCount = sizeof(extensions) / sizeof(*extensions),
                                      .ppEnabledExtensionNames = extensions};
    VkDevice device;
    CHECK(vkCreateDevice(gpu, &device_info, NULL, &device));
    VkQueue queue;
    vkGetDeviceQueue(device, family, 0, &queue);
    PFN_vkGetMemoryFdPropertiesKHR get_memory_fd_properties =
        (PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(device, "vkGetMemoryFdPropertiesKHR");
    PFN_vkGetSemaphoreFdKHR get_semaphore_fd =
        (PFN_vkGetSemaphoreFdKHR)vkGetDeviceProcAddr(device, "vkGetSemaphoreFdKHR");

    /* The app's buffers as images. */
    VkImage images[MAX_BUFFERS];
    VkImageView views[MAX_BUFFERS];
    for (uint32_t i = 0; i < hello.count; i++) {
        VkSubresourceLayout plane = {.offset = 0, .rowPitch = hello.stride};
        VkImageDrmFormatModifierExplicitCreateInfoEXT modifier = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
            .drmFormatModifier = DRM_FORMAT_MOD_LINEAR,
            .drmFormatModifierPlaneCount = 1,
            .pPlaneLayouts = &plane};
        VkExternalMemoryImageCreateInfo external = {
            .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            .pNext = &modifier,
            .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
        VkImageCreateInfo image_info = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            .pNext = &external,
            .imageType = VK_IMAGE_TYPE_2D,
            .format = VK_FORMAT_R8G8B8A8_UNORM,
            .extent = {hello.width, hello.height, 1},
            .mipLevels = 1,
            .arrayLayers = 1,
            .samples = VK_SAMPLE_COUNT_1_BIT,
            .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
            .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
            .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED};
        CHECK(vkCreateImage(device, &image_info, NULL, &images[i]));

        VkMemoryRequirements requirements;
        vkGetImageMemoryRequirements(device, images[i], &requirements);
        VkMemoryFdPropertiesKHR fd_properties = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
        CHECK(get_memory_fd_properties(device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                       dmabufs[i], &fd_properties));
        uint32_t types = requirements.memoryTypeBits & fd_properties.memoryTypeBits;
        if (!types) {
            fprintf(stderr, "no memory type takes buffer %u\n", i);
            return 1;
        }
        VkImportMemoryFdInfoKHR import = {.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                                          .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                          .fd = dmabufs[i]};
        VkMemoryDedicatedAllocateInfo dedicated = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .pNext = &import,
            .image = images[i]};
        VkMemoryAllocateInfo allocate = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                         .pNext = &dedicated,
                                         .allocationSize = requirements.size,
                                         .memoryTypeIndex = __builtin_ctz(types)};
        VkDeviceMemory memory;
        CHECK(vkAllocateMemory(device, &allocate, NULL, &memory)); /* takes the descriptor */
        CHECK(vkBindImageMemory(device, images[i], memory, 0));

        VkImageViewCreateInfo view_info = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
            .image = images[i],
            .viewType = VK_IMAGE_VIEW_TYPE_2D,
            .format = VK_FORMAT_R8G8B8A8_UNORM,
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
        CHECK(vkCreateImageView(device, &view_info, NULL, &views[i]));
    }

    VkCommandPoolCreateInfo pool_info = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                         .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                         .queueFamilyIndex = family};
    VkCommandPool pool;
    CHECK(vkCreateCommandPool(device, &pool_info, NULL, &pool));
    VkCommandBuffer commands[MAX_BUFFERS];
    VkCommandBufferAllocateInfo command_info = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                                .commandPool = pool,
                                                .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                                .commandBufferCount = hello.count};
    CHECK(vkAllocateCommandBuffers(device, &command_info, commands));
    VkFence done[MAX_BUFFERS];
    VkSemaphore rendered[MAX_BUFFERS];
    for (uint32_t i = 0; i < hello.count; i++) {
        VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
                                        .flags = VK_FENCE_CREATE_SIGNALED_BIT};
        CHECK(vkCreateFence(device, &fence_info, NULL, &done[i]));
        VkExportSemaphoreCreateInfo export_info = {
            .sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
            .handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT};
        VkSemaphoreCreateInfo semaphore_info = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
                                                .pNext = &export_info};
        CHECK(vkCreateSemaphore(device, &semaphore_info, NULL, &rendered[i]));
    }

    int lent[MAX_BUFFERS] = {0};          /* the app has it */
    int returned[MAX_BUFFERS];           /* its fence when it gave it back */
    for (int i = 0; i < MAX_BUFFERS; i++)
        returned[i] = -1;

    uint32_t eye_width = hello.width / 2, size = hello.height / 4;
    double start = now(), next = start, period = 1.0 / 72;
    uint64_t number = 0, skipped = 0;
    while (now() - start < run_for) {
        /* Buffers the app gives back. */
        for (;;) {
            struct release release;
            int fd, n;
            ssize_t got = receive(sock, &release, sizeof(release), &fd, &n, 1, MSG_DONTWAIT);
            if (got == 0) {
                printf("the app closed the connection\n");
                return 0;
            }
            if (got < 0)
                break;
            if (got == sizeof(release) && release.type == RELEASE && release.buffer < hello.count) {
                lent[release.buffer] = 0;
                if (returned[release.buffer] >= 0)
                    close(returned[release.buffer]);
                returned[release.buffer] = n ? fd : -1;
            } else if (n) {
                close(fd);
            }
        }

        uint32_t i = 0;
        while (i < hello.count && lent[i])
            i++;
        if (i == hello.count) {
            skipped++;
        } else {
            if (returned[i] >= 0) {
                struct pollfd wait = {returned[i], POLLIN, 0};
                poll(&wait, 1, 1000);
                close(returned[i]);
                returned[i] = -1;
            }
            CHECK(vkWaitForFences(device, 1, &done[i], VK_TRUE, UINT64_MAX));
            CHECK(vkResetFences(device, 1, &done[i]));

            double t = now() - start;
            VkCommandBuffer cmd = commands[i];
            VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                              .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
            CHECK(vkBeginCommandBuffer(cmd, &begin));
            VkImageMemoryBarrier to_render = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                .image = images[i],
                .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
            vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                                 VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1,
                                 &to_render);
            /* A background drifting through hues, and a square sliding back and forth in each eye
             * at the same place, so the pair fuses into one at infinity. */
            VkRenderingAttachmentInfo attachment = {
                .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
                .imageView = views[i],
                .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
                .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                .clearValue.color.float32 = {0.25f + 0.2f * sinf(t * 0.5f),
                                             0.25f + 0.2f * sinf(t * 0.5f + 2.1f),
                                             0.25f + 0.2f * sinf(t * 0.5f + 4.2f), 1}};
            VkRenderingInfo rendering = {.sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
                                         .renderArea = {{0, 0}, {hello.width, hello.height}},
                                         .layerCount = 1,
                                         .colorAttachmentCount = 1,
                                         .pColorAttachments = &attachment};
            vkCmdBeginRendering(cmd, &rendering);
            int32_t x = (int32_t)((0.5 + 0.5 * sin(t)) * (eye_width - size));
            int32_t y = (int32_t)(hello.height - size) / 2;
            VkClearAttachment white = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
                                       .clearValue.color.float32 = {0.95f, 0.95f, 0.95f, 1}};
            VkClearRect squares[2] = {{{{x, y}, {size, size}}, 0, 1},
                                      {{{(int32_t)eye_width + x, y}, {size, size}}, 0, 1}};
            vkCmdClearAttachments(cmd, 1, &white, 2, squares);
            vkCmdEndRendering(cmd);
            VkImageMemoryBarrier to_app = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                .newLayout = VK_IMAGE_LAYOUT_GENERAL,
                .srcQueueFamilyIndex = family,
                .dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
                .image = images[i],
                .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
            vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                                 VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &to_app);
            CHECK(vkEndCommandBuffer(cmd));

            VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                                   .commandBufferCount = 1,
                                   .pCommandBuffers = &cmd,
                                   .signalSemaphoreCount = 1,
                                   .pSignalSemaphores = &rendered[i]};
            CHECK(vkQueueSubmit(queue, 1, &submit, done[i]));
            VkSemaphoreGetFdInfoKHR get_fd = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
                                              .semaphore = rendered[i],
                                              .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT};
            int fence = -1;
            CHECK(get_semaphore_fd(device, &get_fd, &fence));
            struct frame frame = {FRAME, i, ++number};
            if (send_with_fd(sock, &frame, sizeof(frame), fence) < 0) {
                perror("send");
                return 1;
            }
            close(fence);
            lent[i] = 1;
        }

        next += period;
        double wait = next - now();
        if (wait > 0) {
            struct timespec ts = {(time_t)wait, (long)((wait - (time_t)wait) * 1e9)};
            nanosleep(&ts, NULL);
        } else {
            next = now();
        }
    }
    printf("%llu frames in %.1f s, %llu skipped for want of a buffer\n", (unsigned long long)number,
           now() - start, (unsigned long long)skipped);
    vkDeviceWaitIdle(device);
    return 0;
}
