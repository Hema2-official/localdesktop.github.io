//! Immersive mode on VR headsets (Meta Horizon OS). `XrActivity` (Java) is the immersive
//! activity; while it lives, a thread here runs an OpenXR session on the headset's runtime,
//! through the Khronos loader in assets/libs.
//!
//! The session shows the frames a program in Linux renders into buffers it lends it (`frames`),
//! and a slowly pulsing colour while there are none.

mod frames;

use anyhow::{anyhow, bail, Context, Result};
use glow::HasContext;
use jni::objects::{GlobalRef, JClass, JObject};
use jni::{JNIEnv, JavaVM};
use khronos_egl as egl;
use openxr as xr;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The session's thread, while `XrActivity` lives.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

struct Session {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// How long `XrActivity.onDestroy` waits for the session to end. The loop sees the request
/// within a frame; the UI thread mustn't hang on a runtime that doesn't answer.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

const GL_RGBA8: u32 = 0x8058;
const GL_SRGB8_ALPHA8: u32 = 0x8C43;

/// Buffers lent to Linux: one on show, one it renders into, one spare.
const FRAME_BUFFERS: usize = 3;

#[no_mangle]
pub extern "system" fn Java_app_polarbear_XrActivity_nativeStart(
    env: JNIEnv,
    _class: JClass,
    activity: JObject,
) {
    // The process may have started with this activity, before any NativeActivity set up logging.
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    stop_session();
    let (vm, activity) = match env
        .get_java_vm()
        .and_then(|vm| Ok((vm, env.new_global_ref(activity)?)))
    {
        Ok(it) => it,
        Err(error) => {
            log::error!("Immersive mode: no JNI handles for the session: {error}");
            return;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let spawned = thread::Builder::new().name("openxr".into()).spawn({
        let stop = stop.clone();
        move || {
            if let Err(error) = run(&vm, &activity, &stop) {
                log::error!("Immersive mode failed: {error:#}");
            }
        }
    });
    match spawned {
        Ok(thread) => *SESSION.lock().unwrap() = Some(Session { stop, thread }),
        Err(error) => log::error!("Immersive mode: no thread for the session: {error}"),
    }
}

#[no_mangle]
pub extern "system" fn Java_app_polarbear_XrActivity_nativeStop(_env: JNIEnv, _class: JClass) {
    stop_session();
}

fn stop_session() {
    let Some(session) = SESSION.lock().unwrap().take() else {
        return;
    };
    session.stop.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + STOP_TIMEOUT;
    while !session.thread.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if session.thread.is_finished() {
        let _ = session.thread.join();
    } else {
        log::warn!("Immersive mode: the session didn't end in time; leaving it to finish");
    }
}

/// One swapchain per eye, at the runtime's recommended size.
struct Eye {
    swapchain: xr::Swapchain<xr::OpenGlEs>,
    images: Vec<u32>,
    width: u32,
    height: u32,
}

fn run(vm: &JavaVM, activity: &GlobalRef, stop: &AtomicBool) -> Result<()> {
    // The loader and the runtime call into Java from this thread.
    let _env = vm.attach_current_thread()?;
    let platform = unsafe {
        xr::AndroidPlatformInfo::new(
            vm.get_java_vm_pointer().cast(),
            activity.as_obj().as_raw().cast(),
        )
    };
    let entry = unsafe { xr::Entry::load(&platform) }
        .map_err(|error| anyhow!("loading the OpenXR loader: {error}"))?;
    let available = entry.enumerate_extensions()?;
    if !available.khr_opengl_es_enable {
        bail!("the OpenXR runtime has no OpenGL ES support");
    }
    let mut enabled = xr::ExtensionSet::default();
    enabled.khr_android_create_instance = true;
    enabled.khr_opengl_es_enable = true;
    let instance = entry.create_instance(
        &xr::ApplicationInfo {
            application_name: "Local Desktop",
            application_version: 1,
            engine_name: "Local Desktop",
            engine_version: 1,
            api_version: xr::Version::new(1, 0, 0),
        },
        &enabled,
        &[],
        &platform,
    )?;
    let properties = instance.properties()?;
    let system = instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)?;
    let views = instance
        .enumerate_view_configuration_views(system, xr::ViewConfigurationType::PRIMARY_STEREO)?;
    log::info!(
        "Immersive mode: runtime {} {}, {} views of {}x{}",
        properties.runtime_name,
        properties.runtime_version,
        views.len(),
        views[0].recommended_image_rect_width,
        views[0].recommended_image_rect_height
    );
    // Required before a session, even when nothing is checked against it.
    let _ = instance.graphics_requirements::<xr::OpenGlEs>(system)?;

    // Before the session, so it's dropped after it.
    let mut gl = Gl::new().context("setting up OpenGL ES")?;
    let (session, mut waiter, mut stream) = unsafe {
        instance.create_session::<xr::OpenGlEs>(
            system,
            &xr::opengles::SessionCreateInfo::Android {
                display: gl.display.as_ptr(),
                config: gl.config.as_ptr(),
                context: gl.context.as_ptr(),
            },
        )
    }?;
    let space =
        session.create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)?;
    let formats = session.enumerate_swapchain_formats()?;
    let format = [GL_SRGB8_ALPHA8, GL_RGBA8]
        .into_iter()
        .find(|format| formats.contains(format))
        .or(formats.first().copied())
        .ok_or_else(|| anyhow!("the runtime offers no swapchain formats"))?;
    let mut eyes = views
        .iter()
        .map(|view| {
            let (width, height) = (
                view.recommended_image_rect_width,
                view.recommended_image_rect_height,
            );
            let swapchain = session.create_swapchain(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                    | xr::SwapchainUsageFlags::SAMPLED,
                format,
                sample_count: 1,
                width,
                height,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            })?;
            let images = swapchain.enumerate_images()?;
            Ok(Eye {
                swapchain,
                images,
                width,
                height,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Both eyes side by side in each buffer lent to Linux.
    let mut transport = match frames::Frames::new(eyes[0].width * 2, eyes[0].height, FRAME_BUFFERS)
        .and_then(|transport| {
            for buffer in &transport.buffers {
                gl.import(buffer.hardware_buffer)?;
            }
            Ok(transport)
        }) {
        Ok(transport) => Some(transport),
        Err(error) => {
            log::warn!("Immersive mode: no frames from Linux: {error:#}");
            None
        }
    };
    // The buffer on show, which the app holds until a newer frame replaces it.
    let mut showing: Option<usize> = None;

    let mut events = xr::EventDataBuffer::new();
    let mut running = false;
    let mut exit_requested = false;
    let started = Instant::now();
    let (mut shown, mut from_linux) = (0u32, 0u32);
    let mut counted_since = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) && !exit_requested {
            if !running {
                break;
            }
            session.request_exit()?;
            exit_requested = true;
        }
        while let Some(event) = instance.poll_event(&mut events)? {
            match event {
                xr::Event::SessionStateChanged(change) => {
                    log::info!("Immersive mode: session {:?}", change.state());
                    match change.state() {
                        xr::SessionState::READY => {
                            session.begin(xr::ViewConfigurationType::PRIMARY_STEREO)?;
                            running = true;
                        }
                        xr::SessionState::STOPPING => {
                            session.end()?;
                            running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                xr::Event::InstanceLossPending(_) => return Ok(()),
                _ => {}
            }
        }
        if !running {
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let frame = waiter.wait()?;
        stream.begin()?;
        if !frame.should_render {
            stream.end(
                frame.predicted_display_time,
                xr::EnvironmentBlendMode::OPAQUE,
                &[],
            )?;
            continue;
        }
        let (_, located) = session.locate_views(
            xr::ViewConfigurationType::PRIMARY_STEREO,
            frame.predicted_display_time,
            &space,
        )?;
        let mut replaced = None;
        if let Some(transport) = transport.as_mut() {
            match transport.update() {
                frames::Update::Frame { buffer, fence } => {
                    if let Some(fence) = fence {
                        gl.wait(fence)?;
                    }
                    replaced = showing.replace(buffer);
                    from_linux += 1;
                }
                frames::Update::Disconnected => showing = None,
                frames::Update::Nothing => {}
            }
        }
        let pulse = 0.5 + 0.5 * (started.elapsed().as_secs_f32() * 0.8).sin();
        for (index, eye) in eyes.iter_mut().enumerate() {
            let image = eye.swapchain.acquire_image()?;
            eye.swapchain.wait_image(xr::Duration::INFINITE)?;
            let texture = eye.images[image as usize];
            match showing {
                Some(buffer) => gl.blit(
                    buffer,
                    index as i32 * eye.width as i32,
                    eye.width,
                    eye.height,
                    texture,
                )?,
                None => gl.clear(
                    texture,
                    eye.width,
                    eye.height,
                    [0.05, 0.2 + 0.2 * pulse, 0.3, 1.0],
                )?,
            }
            eye.swapchain.release_image()?;
        }
        if let (Some(transport), Some(buffer)) = (transport.as_mut(), replaced) {
            transport.give_back(buffer, gl.fence());
        }
        let projection_views = eyes
            .iter()
            .zip(&located)
            .map(|(eye, view)| {
                xr::CompositionLayerProjectionView::new()
                    .pose(view.pose)
                    .fov(view.fov)
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&eye.swapchain)
                            .image_array_index(0)
                            .image_rect(xr::Rect2Di {
                                offset: xr::Offset2Di { x: 0, y: 0 },
                                extent: xr::Extent2Di {
                                    width: eye.width as i32,
                                    height: eye.height as i32,
                                },
                            }),
                    )
            })
            .collect::<Vec<_>>();
        stream.end(
            frame.predicted_display_time,
            xr::EnvironmentBlendMode::OPAQUE,
            &[&xr::CompositionLayerProjection::new()
                .space(&space)
                .views(&projection_views)],
        )?;

        shown += 1;
        if counted_since.elapsed() >= Duration::from_secs(10) {
            let seconds = counted_since.elapsed().as_secs_f64();
            log::info!(
                "Immersive mode: {:.1} frames/s, {:.1} from Linux, display period {} ms",
                f64::from(shown) / seconds,
                f64::from(from_linux) / seconds,
                frame.predicted_display_period.as_nanos() / 1_000_000
            );
            (shown, from_linux) = (0, 0);
            counted_since = Instant::now();
        }
    }
    Ok(())
}

const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: egl::Int = 0x30D2;
const EGL_SYNC_NATIVE_FENCE_ANDROID: u32 = 0x3144;
const EGL_SYNC_NATIVE_FENCE_FD_ANDROID: egl::Int = 0x3145;

/// The EGL and GL extensions that show Linux's frames: AHardwareBuffers as textures, and sync
/// files as EGL fences.
struct Extensions {
    get_native_client_buffer: unsafe extern "system" fn(*const c_void) -> *mut c_void,
    create_image: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *mut c_void,
        *const i32,
    ) -> *mut c_void,
    destroy_image: unsafe extern "system" fn(*mut c_void, *mut c_void) -> u32,
    image_target_texture: unsafe extern "system" fn(u32, *mut c_void),
    create_sync: unsafe extern "system" fn(*mut c_void, u32, *const i32) -> *mut c_void,
    wait_sync: unsafe extern "system" fn(*mut c_void, *mut c_void, i32) -> i32,
    destroy_sync: unsafe extern "system" fn(*mut c_void, *mut c_void) -> u32,
    dup_native_fence: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
}

impl Extensions {
    /// The functions, if the display has every extension behind them.
    fn load(egl: &egl::DynamicInstance<egl::EGL1_4>, display: egl::Display) -> Option<Self> {
        let extensions = egl.query_string(Some(display), egl::EXTENSIONS).ok()?;
        let extensions = extensions.to_string_lossy();
        let needed = [
            "EGL_KHR_image_base",
            "EGL_ANDROID_image_native_buffer",
            "EGL_ANDROID_get_native_client_buffer",
            "EGL_ANDROID_native_fence_sync",
            "EGL_KHR_wait_sync",
        ];
        if let Some(missing) = needed
            .iter()
            .find(|name| !extensions.split(' ').any(|it| it == **name))
        {
            log::warn!("Immersive mode: EGL lacks {missing}");
            return None;
        }
        unsafe fn get<T>(egl: &egl::DynamicInstance<egl::EGL1_4>, name: &str) -> Option<T> {
            egl.get_proc_address(name)
                .map(|function| std::mem::transmute_copy(&function))
        }
        unsafe {
            Some(Self {
                get_native_client_buffer: get(egl, "eglGetNativeClientBufferANDROID")?,
                create_image: get(egl, "eglCreateImageKHR")?,
                destroy_image: get(egl, "eglDestroyImageKHR")?,
                image_target_texture: get(egl, "glEGLImageTargetTexture2DOES")?,
                create_sync: get(egl, "eglCreateSyncKHR")?,
                wait_sync: get(egl, "eglWaitSyncKHR")?,
                destroy_sync: get(egl, "eglDestroySyncKHR")?,
                dup_native_fence: get(egl, "eglDupNativeFenceFDANDROID")?,
            })
        }
    }
}

/// A buffer lent to Linux, as GL reads it.
struct Imported {
    image: *mut c_void,
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
}

/// An OpenGL ES 3 context of the session's own, current on its thread, with a pbuffer surface
/// (the runtime presents; nothing draws to a window).
struct Gl {
    egl: egl::DynamicInstance<egl::EGL1_4>,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    surface: egl::Surface,
    gl: glow::Context,
    framebuffer: glow::Framebuffer,
    extensions: Option<Extensions>,
    /// The buffers lent to Linux, by index.
    imported: Vec<Imported>,
}

impl Gl {
    fn new() -> Result<Self> {
        const OPENGL_ES3_BIT: egl::Int = 0x40;
        let library = unsafe { libloading::Library::new("libEGL.so") }?;
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required_from(library) }
            .map_err(|error| anyhow!("loading EGL: {error}"))?;
        let display = unsafe { egl.get_display(egl::DEFAULT_DISPLAY) }
            .ok_or_else(|| anyhow!("no EGL display"))?;
        // The compositor shares the display: initializing again is harmless, terminating isn't.
        egl.initialize(display)?;
        let config = egl
            .choose_first_config(
                display,
                &[
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::RENDERABLE_TYPE,
                    OPENGL_ES3_BIT,
                    egl::SURFACE_TYPE,
                    egl::PBUFFER_BIT,
                    egl::NONE,
                ],
            )?
            .ok_or_else(|| anyhow!("no EGL config for OpenGL ES 3"))?;
        let context = egl.create_context(
            display,
            config,
            None,
            &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
        )?;
        let surface = egl.create_pbuffer_surface(
            display,
            config,
            &[egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE],
        )?;
        egl.make_current(display, Some(surface), Some(surface), Some(context))?;
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map_or(std::ptr::null(), |function| function as *const _)
            })
        };
        let framebuffer = unsafe { gl.create_framebuffer() }.map_err(|error| anyhow!(error))?;
        let extensions = Extensions::load(&egl, display);
        Ok(Self {
            egl,
            display,
            config,
            context,
            surface,
            gl,
            framebuffer,
            extensions,
            imported: Vec::new(),
        })
    }

    fn extensions(&self) -> Result<&Extensions> {
        self.extensions
            .as_ref()
            .ok_or_else(|| anyhow!("EGL can't import AHardwareBuffers or sync files"))
    }

    /// Make an AHardwareBuffer readable as the next of `imported`.
    fn import(&mut self, hardware_buffer: *mut c_void) -> Result<()> {
        let extensions = self.extensions()?;
        let display = self.display.as_ptr();
        unsafe {
            let client_buffer = (extensions.get_native_client_buffer)(hardware_buffer);
            if client_buffer.is_null() {
                bail!("eglGetNativeClientBufferANDROID failed");
            }
            let attributes = [EGL_IMAGE_PRESERVED_KHR, 1, egl::NONE];
            let image = (extensions.create_image)(
                display,
                ptr::null_mut(),
                EGL_NATIVE_BUFFER_ANDROID,
                client_buffer,
                attributes.as_ptr(),
            );
            if image.is_null() {
                bail!("eglCreateImageKHR failed");
            }
            let texture = self.gl.create_texture().map_err(|error| anyhow!(error))?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            (extensions.image_target_texture)(glow::TEXTURE_2D, image);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            let framebuffer = self
                .gl
                .create_framebuffer()
                .map_err(|error| anyhow!(error))?;
            self.imported.push(Imported {
                image,
                texture,
                framebuffer,
            });
            self.gl
                .bind_framebuffer(glow::READ_FRAMEBUFFER, Some(framebuffer));
            self.gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            let status = self.gl.check_framebuffer_status(glow::READ_FRAMEBUFFER);
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                bail!("a buffer from Linux can't be read: framebuffer status 0x{status:x}");
            }
        }
        Ok(())
    }

    /// Make GL wait (on the GPU) for a sync file to signal.
    fn wait(&self, fence: OwnedFd) -> Result<()> {
        let extensions = self.extensions()?;
        let display = self.display.as_ptr();
        let attributes = [
            EGL_SYNC_NATIVE_FENCE_FD_ANDROID,
            fence.as_raw_fd(),
            egl::NONE,
        ];
        unsafe {
            let sync = (extensions.create_sync)(
                display,
                EGL_SYNC_NATIVE_FENCE_ANDROID,
                attributes.as_ptr(),
            );
            if sync.is_null() {
                bail!("eglCreateSyncKHR failed for a frame's sync file");
            }
            // EGL owns the descriptor now.
            let _ = fence.into_raw_fd();
            (extensions.wait_sync)(display, sync, 0);
            (extensions.destroy_sync)(display, sync);
        }
        Ok(())
    }

    /// A sync file that signals once GL has done what it was asked so far.
    fn fence(&self) -> Option<OwnedFd> {
        let extensions = self.extensions.as_ref()?;
        let display = self.display.as_ptr();
        let attributes = [egl::NONE];
        unsafe {
            let sync = (extensions.create_sync)(
                display,
                EGL_SYNC_NATIVE_FENCE_ANDROID,
                attributes.as_ptr(),
            );
            if sync.is_null() {
                return None;
            }
            self.gl.flush();
            let fd = (extensions.dup_native_fence)(display, sync);
            (extensions.destroy_sync)(display, sync);
            (fd >= 0).then(|| OwnedFd::from_raw_fd(fd))
        }
    }

    /// Copy `width` x `height` from `source_x` in an imported buffer into one of the runtime's
    /// swapchain images.
    fn blit(
        &self,
        buffer: usize,
        source_x: i32,
        width: u32,
        height: u32,
        texture: u32,
    ) -> Result<()> {
        let source = self
            .imported
            .get(buffer)
            .ok_or_else(|| anyhow!("no buffer {buffer}"))?;
        let texture = NonZeroU32::new(texture).ok_or_else(|| anyhow!("swapchain image 0"))?;
        let (width, height) = (width as i32, height as i32);
        unsafe {
            self.gl
                .bind_framebuffer(glow::READ_FRAMEBUFFER, Some(source.framebuffer));
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.framebuffer));
            self.gl.framebuffer_texture_2d(
                glow::DRAW_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(glow::NativeTexture(texture)),
                0,
            );
            // Linux renders with Vulkan, whose rows go top to bottom, and GL's go bottom to top.
            self.gl.blit_framebuffer(
                source_x,
                0,
                source_x + width,
                height,
                0,
                height,
                width,
                0,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
        }
        Ok(())
    }

    /// Fill one of the runtime's swapchain images with a colour.
    fn clear(&self, texture: u32, width: u32, height: u32, color: [f32; 4]) -> Result<()> {
        let texture = NonZeroU32::new(texture).ok_or_else(|| anyhow!("swapchain image 0"))?;
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(glow::NativeTexture(texture)),
                0,
            );
            self.gl.viewport(0, 0, width as i32, height as i32);
            self.gl.clear_color(color[0], color[1], color[2], color[3]);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        Ok(())
    }
}

impl Drop for Gl {
    fn drop(&mut self) {
        for imported in self.imported.drain(..) {
            unsafe {
                self.gl.delete_framebuffer(imported.framebuffer);
                self.gl.delete_texture(imported.texture);
                if let Some(extensions) = self.extensions.as_ref() {
                    (extensions.destroy_image)(self.display.as_ptr(), imported.image);
                }
            }
        }
        unsafe { self.gl.delete_framebuffer(self.framebuffer) };
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
    }
}
