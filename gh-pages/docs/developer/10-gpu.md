---
title: The GPU
---

Android doesn't give apps the kernel's DRM devices (`/dev/dri`), which every Linux graphics stack expects. On Qualcomm phones it does give them Qualcomm's own GPU driver interface, KGSL (`/dev/kgsl-3d0`), and Mesa has drivers for it: Turnip for Vulkan, Freedreno for OpenGL. Arch Linux ARM builds Mesa without them, so Local Desktop installs a build that has them (see [Configurations](../user/4-configurations.md#gpu)).

## What draws on the GPU

| | |
|---|---|
| Vulkan programs | Turnip, directly. |
| OpenGL programs | Zink (OpenGL on Vulkan) on Turnip, in every desktop session unless `[graphics] zink = false`; `gpu PROGRAM` does the same for a single program. |
| Plasma's panels and menus | Qt Quick with its Vulkan renderer, where `vulkaninfo` finds Turnip. |
| Firefox | Its software renderer: its displays end up on Mesa's software EGL device, which has no window configurations, so hardware WebRender fails to start. |

Freedreno's own OpenGL works only without a window system (no DRM device to show frames on), so OpenGL goes through Zink, which shows its frames through Vulkan's window system support ("kopper").

Mesa's EGL on Wayland asks the compositor for the GPU's DRM device. Without one it tries again in software mode, and drops Zink on the way even when `MESA_LOADER_DRIVER_OVERRIDE=zink` asked for it, so programs would draw with llvmpipe on the CPU. In software mode with Zink asked for, though, it takes Zink with kopper. Software mode is `LIBGL_ALWAYS_SOFTWARE`, which Zink reads too, and then it looks for a CPU Vulkan device instead of the GPU. So the session preloads `libegl-zink.so` (`src/guest/egl-zink.c`), which tells Mesa's EGL alone that the variable is set, only when Zink was asked for. A fix in Mesa (keep Zink in EGL's software retry) would make it unnecessary.

## How frames reach the screen

**In the desktop session**, programs are clients of the desktop's compositor (KWin in the Plasma preset, labwc in Xfce), which is itself a client of Local Desktop's compositor in the app:

1. A Vulkan program (or an OpenGL one through Zink) renders on the GPU, then Mesa copies each frame into shared memory (`MESA_VK_WSI_DEBUG=sw`): the desktop's compositor takes nothing else.
2. KWin composites the desktop on the CPU (its QPainter renderer) into shared memory.
3. The app uploads the changed parts into a GL texture and draws it into the Android window.

**Directly on the app's compositor** (`localdesktop direct PROGRAM`), a program hands its GPU buffers over as dma-bufs (`zwp_linux_dmabuf_v1`, linear ARGB/XRGB/ABGR/XBGR8888), and the app shows them without copies:

- Android's EGL can't import dma-bufs, only Android's own buffers (AHardwareBuffers). The app takes the handle of a buffer gralloc makes of the same size and format, passes it through a socket the way Android hands buffers between processes, with the client's dma-buf in place of the pixels, and gets an AHardwareBuffer of the client's memory (`src/android/backend/wayland/dmabuf.rs`). Nothing else in the handle changes, since its layout is the vendor's.
- That needs gralloc's rows as far apart as Turnip's. Turnip aligns linear rows to 64 pixels; Qualcomm's gralloc does too, except where that comes to a multiple of 512 pixels, which it pads by 256 more. The phone's and DeX's full-screen widths (1080, 2340, 1920) match. For other widths the dma-buf becomes a gralloc buffer of bytes, and the GPU copies it row by row into a buffer of the app's at each commit (about 0.6 ms for 1000x600).
- The kernel can't attach fences to dma-bufs (that came with Linux 6.0) and KGSL has no implicit sync, so nothing tells the compositor when a client's rendering is done. Both use the same GPU queue at the same priority, which keeps them in order in practice.

The newest window on the app's compositor is drawn on top and gets the input, so a program started there covers the desktop until it closes; the desktop's compositor gets no frame callbacks meanwhile and stops drawing.

## The desktop on the GPU

KWin's nested Wayland backend composites with OpenGL only when its host offers `zwp_linux_dmabuf_v1` with a main device that opens as a DRM device: that device is KWin's "render device", whose GBM allocator makes the buffers KWin renders into (`WaylandBackend::supportedCompositors`, `WaylandEglLayer` in KWin 6.7). Without it KWin falls back to QPainter. There's no DRM device on Android, and none can be faked from userspace.

What KWin would do instead is known to work here: OpenGL through Mesa's EGL without a window system (`EGL_PLATFORM_SURFACELESS_MESA`, Zink on Turnip, with the same preload) imports dma-bufs that Vulkan allocated linear (`EGL_EXT_image_dma_buf_import` and `_modifiers` are there), renders into them, and the app's compositor shows them without copies, 120 frames a second at the phone's 1080x2340. So KWin on the GPU needs a patch to KWin:

- a render device without a DRM device: an EGL display on the surfaceless platform, and buffers allocated through KWin's Vulkan device instead of GBM (a `GraphicsBufferAllocator`);
- OpenGL compositing allowed with such a device when the host takes dma-bufs.

and a KWin package built with it for every KWin release, which setup installs and keeps, as it does the Adreno drivers. With that, KWin would also offer its own clients dma-bufs, and Vulkan programs in the desktop would stop copying their frames through the CPU. wlroots (labwc) has the same requirement (a DRM device for its renderer and allocator).

## Looking at it

- `localdesktop direct vkcube` shows Vulkan through dma-bufs; logcat says `Graphics: clients may hand over dma-bufs` when the app's compositor offers them, and `Graphics: copying clients' WxH buffers…` for widths gralloc pads differently.
- `vulkaninfo --summary` lists `Turnip Adreno (TM) …` when the driver works.
- An OpenGL program's renderer (mpv `--msg-level=vo/gpu=v` prints `GL_RENDERER`) says `zink Vulkan 1.3(Turnip …)` on the GPU, `llvmpipe` on the CPU.
