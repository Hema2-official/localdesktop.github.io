//! Immersive mode on VR headsets (Meta Horizon OS). `XrActivity` (Java) is the immersive
//! activity; while it lives, a thread here runs an OpenXR session on the headset's runtime,
//! through the Khronos loader in assets/libs.
//!
//! The session shows the frames Monado in Linux renders into buffers it lends it (`frames`),
//! each with the poses it was rendered for, and a slowly pulsing colour while there are none.
//! Monado gets the head's tracking and the controllers' state from it every display frame
//! (`controllers`). The guest link enters immersive mode when Monado asks for it, and hands it the
//! buffers (`guest::xr`).

mod controllers;
mod frames;
mod hands;
pub mod protocol;

use crate::android::guest;
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

/// Buffers lent to Linux: one the app copies from, one Linux renders into, one spare.
const FRAME_BUFFERS: usize = 3;

/// How long Linux's last frame stays up when no newer one comes (its app quit, say).
const FRAME_TIMEOUT: Duration = Duration::from_secs(1);

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
    // Before the panel's window goes, so the desktop's sound stays (`PolarBearApp::detach`).
    guest::xr::set_immersive(true);
    let stop = Arc::new(AtomicBool::new(false));
    let spawned = thread::Builder::new().name("openxr".into()).spawn({
        let stop = stop.clone();
        move || {
            if let Err(error) = run(&vm, &activity, &stop) {
                log::error!("Immersive mode failed: {error:#}");
            }
            guest::xr::set_immersive(false);
        }
    });
    match spawned {
        Ok(thread) => *SESSION.lock().unwrap() = Some(Session { stop, thread }),
        Err(error) => {
            log::error!("Immersive mode: no thread for the session: {error}");
            guest::xr::set_immersive(false);
        }
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
    enabled.khr_convert_timespec_time = available.khr_convert_timespec_time;
    enabled.fb_display_refresh_rate = available.fb_display_refresh_rate;
    enabled.ext_performance_settings = available.ext_performance_settings;
    enabled.ext_hand_tracking = available.ext_hand_tracking;
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
    let clock = Clock {
        instance: instance.as_raw(),
        convert: instance.exts().khr_convert_timespec_time,
    };

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
    // Linux gets poses in the stage space, whose origin is on the floor, where there is one.
    let space_type = if session
        .enumerate_reference_spaces()?
        .contains(&xr::ReferenceSpaceType::STAGE)
    {
        xr::ReferenceSpaceType::STAGE
    } else {
        log::warn!("Immersive mode: no stage space; Linux gets poses in the local space");
        xr::ReferenceSpaceType::LOCAL
    };
    let space = session.create_reference_space(space_type, xr::Posef::IDENTITY)?;
    let head = session.create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)?;
    let controllers = controllers::Controllers::new(&instance, &session)
        .map_err(|error| log::warn!("Immersive mode: no controllers: {error:#}"))
        .ok();
    let hands = if available.ext_hand_tracking && instance.supports_hand_tracking(system)? {
        hands::Hands::new(&session)
            .map_err(|error| log::warn!("Immersive mode: no hand tracking: {error:#}"))
            .ok()
    } else {
        None
    };
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

    // Lent once the session shows frames, when the views' fields of view are known.
    let mut transport: Option<frames::Frames> = None;
    let mut transport_failed = false;
    // What the newest frame from Linux was rendered for, which the swapchains hold, and when it
    // came.
    let mut shown: Option<Vec<frames::View>> = None;
    let mut shown_since = Instant::now();

    let mut events = xr::EventDataBuffer::new();
    let mut running = false;
    let mut exit_requested = false;
    let started = Instant::now();
    let (mut displayed, mut from_linux) = (0u32, 0u32);
    let mut lateness_ns = 0i64;
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
                            sustain_performance(&instance, &session);
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
            // Linux keeps its buffers while nothing shows.
            drop_frames(&mut transport);
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let frame_state = waiter.wait()?;
        let latch_time_ns = monotonic_ns();
        stream.begin()?;
        if !frame_state.should_render {
            drop_frames(&mut transport);
            stream.end(
                frame_state.predicted_display_time,
                xr::EnvironmentBlendMode::OPAQUE,
                &[],
            )?;
            continue;
        }
        let display_time = frame_state.predicted_display_time;
        let period = frame_state.predicted_display_period;
        let (_, eye_views) = session.locate_views(
            xr::ViewConfigurationType::PRIMARY_STEREO,
            display_time,
            &head,
        )?;

        if transport.is_none() && !transport_failed {
            match lend_buffers(&instance, &session, &mut gl, &eyes, &eye_views, period) {
                Ok(it) => transport = Some(it),
                Err(error) => {
                    log::warn!("Immersive mode: no frames from Linux: {error:#}");
                    transport_failed = true;
                }
            }
        }
        let mut update = frames::Update::Nothing;
        if let Some(transport) = transport.as_mut() {
            let tracking = tracking(
                &clock,
                &head,
                &space,
                display_time,
                period,
                latch_time_ns,
                &eye_views,
            )?;
            transport.send_tracking(&tracking);
            if let Some(controllers) = &controllers {
                match controllers.read(&session, &space, display_time, &clock) {
                    Ok(hands) => {
                        transport.send_controllers(clock.to_monotonic_ns(display_time), &hands)
                    }
                    Err(error) => log::warn!("Immersive mode: the controllers: {error:#}"),
                }
            }
            if let Some(hands) = &hands {
                let time_ns = clock.to_monotonic_ns(display_time);
                match hands.message(&space, display_time, time_ns) {
                    Ok(message) => transport.send_hands(&message),
                    Err(error) => log::warn!("Immersive mode: the hands: {error:#}"),
                }
            }
            update = transport.update();
            if let Some(rate) = transport.take_refresh_rate_request() {
                request_refresh_rate(&instance, &session, rate);
            }
            for haptic in transport.take_haptics() {
                if let Some(controllers) = &controllers {
                    if let Err(error) = controllers.vibrate(&session, &haptic) {
                        log::warn!("Immersive mode: haptics: {error:#}");
                    }
                }
            }
        }
        match (update, transport.as_mut()) {
            (frames::Update::Frame(frame), Some(transport)) => {
                if let Some(fence) = frame.fence {
                    gl.wait(fence)?;
                }
                for (eye, area) in eyes.iter_mut().zip(&transport.views) {
                    let image = eye.swapchain.acquire_image()?;
                    eye.swapchain.wait_image(xr::Duration::INFINITE)?;
                    let texture = eye.images[image as usize];
                    gl.blit(frame.buffer, area.x as i32, eye.width, eye.height, texture)?;
                    eye.swapchain.release_image()?;
                }
                // The swapchains hold the frame now.
                transport.give_back(frame.buffer, gl.fence());
                lateness_ns += clock.to_monotonic_ns(display_time) - frame.display_time_ns;
                from_linux += 1;
                shown = Some(frame.views);
                shown_since = Instant::now();
            }
            (frames::Update::Disconnected, _) => shown = None,
            _ if shown_since.elapsed() > FRAME_TIMEOUT => shown = None,
            _ => {}
        }

        let layer_views: Vec<frames::View> = match &shown {
            Some(views) => views.clone(),
            None => {
                // Nothing from Linux: a pulsing colour, where the head is.
                let pulse = 0.5 + 0.5 * (started.elapsed().as_secs_f32() * 0.8).sin();
                for eye in eyes.iter_mut() {
                    let image = eye.swapchain.acquire_image()?;
                    eye.swapchain.wait_image(xr::Duration::INFINITE)?;
                    gl.clear(
                        eye.images[image as usize],
                        eye.width,
                        eye.height,
                        [0.05, 0.2 + 0.2 * pulse, 0.3, 1.0],
                    )?;
                    eye.swapchain.release_image()?;
                }
                let (_, located) = session.locate_views(
                    xr::ViewConfigurationType::PRIMARY_STEREO,
                    display_time,
                    &space,
                )?;
                located
                    .iter()
                    .map(|view| frames::View {
                        pose: view.pose,
                        fov: view.fov,
                    })
                    .collect()
            }
        };
        let projection_views = eyes
            .iter()
            .zip(&layer_views)
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
            display_time,
            xr::EnvironmentBlendMode::OPAQUE,
            &[&xr::CompositionLayerProjection::new()
                .space(&space)
                .views(&projection_views)],
        )?;

        displayed += 1;
        if counted_since.elapsed() >= Duration::from_secs(10) {
            let seconds = counted_since.elapsed().as_secs_f64();
            log::info!(
                "Immersive mode: {:.1} frames/s, {:.1} from Linux ({:.1} ms behind), display \
                 period {:.2} ms",
                f64::from(displayed) / seconds,
                f64::from(from_linux) / seconds,
                lateness_ns as f64 / f64::from(from_linux.max(1)) / 1e6,
                period.as_nanos() as f64 / 1e6
            );
            (displayed, from_linux, lateness_ns) = (0, 0, 0);
            counted_since = Instant::now();
        }
    }
    Ok(())
}

/// Give Linux back the frames it sends while nothing shows them.
fn drop_frames(transport: &mut Option<frames::Frames>) {
    if let Some(transport) = transport.as_mut() {
        if let frames::Update::Frame(frame) = transport.update() {
            transport.give_back(frame.buffer, None);
        }
    }
}

/// Buffers for the views side by side, each at its swapchain's size, for Monado; and the headset
/// as it is, for the next Monado's start.
fn lend_buffers(
    instance: &xr::Instance,
    session: &xr::Session<xr::OpenGlEs>,
    gl: &mut Gl,
    eyes: &[Eye],
    eye_views: &[xr::View],
    period: xr::Duration,
) -> Result<frames::Frames> {
    let mut x = 0;
    let areas: Vec<frames::ViewArea> = eyes
        .iter()
        .zip(eye_views)
        .map(|(eye, view)| {
            let area = frames::ViewArea {
                x,
                width: eye.width,
                height: eye.height,
                fov: view.fov,
            };
            x += eye.width;
            area
        })
        .collect();
    let (refresh_rate, refresh_rates) = refresh_rates(instance, session, period);
    let degrees = |angle: f32| angle.to_degrees().round() as i32;
    for (index, area) in areas.iter().enumerate() {
        log::info!(
            "Immersive mode: view {index} {}x{}, field of view {} {} {} {} degrees",
            area.width,
            area.height,
            degrees(area.fov.angle_left),
            degrees(area.fov.angle_right),
            degrees(area.fov.angle_up),
            degrees(area.fov.angle_down)
        );
    }
    guest::xr::remember(&protocol::Headset {
        views: areas
            .iter()
            .map(|area| (area.width, area.height, area.fov))
            .collect(),
        refresh_rate,
        refresh_rates,
    });
    let transport = frames::Frames::new(areas, refresh_rate, FRAME_BUFFERS)?;
    for buffer in &transport.buffers {
        gl.import(buffer.hardware_buffer)?;
    }
    Ok(transport)
}

/// When this display frame shows, the eyes relative to the head, and the head in `space` over
/// the next few display periods, as the runtime predicts it.
fn tracking(
    clock: &Clock,
    head: &xr::Space,
    space: &xr::Space,
    display_time: xr::Time,
    period: xr::Duration,
    latch_time_ns: i64,
    eye_views: &[xr::View],
) -> Result<frames::Tracking> {
    let mut samples = Vec::with_capacity(protocol::MAX_HEAD_SAMPLES);
    for index in 0..protocol::MAX_HEAD_SAMPLES as i64 {
        let time = xr::Time::from_nanos(display_time.as_nanos() + index * period.as_nanos());
        let (location, velocity) = head.relate(space, time)?;
        samples.push(protocol::PoseSample::new(
            clock.to_monotonic_ns(time),
            location,
            velocity,
        ));
    }
    Ok(frames::Tracking {
        display_time_ns: clock.to_monotonic_ns(display_time),
        display_period_ns: period.as_nanos(),
        latch_time_ns,
        view_poses: eye_views
            .iter()
            .map(|view| frames::View {
                pose: view.pose,
                fov: view.fov,
            })
            .collect(),
        head: samples,
    })
}

/// Ask the runtime for clocks that sustain Linux's apps: they render in other processes, and
/// through proot.
fn sustain_performance(instance: &xr::Instance, session: &xr::Session<xr::OpenGlEs>) {
    let Some(performance) = instance.exts().ext_performance_settings else {
        return;
    };
    for domain in [
        xr::sys::PerfSettingsDomainEXT::CPU,
        xr::sys::PerfSettingsDomainEXT::GPU,
    ] {
        let result = unsafe {
            (performance.perf_settings_set_performance_level)(
                session.as_raw(),
                domain,
                xr::sys::PerfSettingsLevelEXT::SUSTAINED_HIGH,
            )
        };
        if result != xr::sys::Result::SUCCESS {
            log::warn!("Immersive mode: performance level for {domain:?}: {result:?}");
        }
    }
}

/// Switch the display to the refresh rate Monado asked for.
fn request_refresh_rate(instance: &xr::Instance, session: &xr::Session<xr::OpenGlEs>, rate: f32) {
    let Some(fb) = instance.exts().fb_display_refresh_rate else {
        return;
    };
    let result = unsafe { (fb.request_display_refresh_rate)(session.as_raw(), rate) };
    if result == xr::sys::Result::SUCCESS {
        log::info!("Immersive mode: {rate:.0} Hz");
    } else {
        log::warn!("Immersive mode: no {rate:.0} Hz: {result:?}");
    }
}

/// The display's refresh rate and those it can switch to, where the runtime says.
fn refresh_rates(
    instance: &xr::Instance,
    session: &xr::Session<xr::OpenGlEs>,
    period: xr::Duration,
) -> (f32, Vec<f32>) {
    let mut current = 1e9 / period.as_nanos() as f32;
    let Some(fb) = instance.exts().fb_display_refresh_rate else {
        return (current, vec![current]);
    };
    let mut rates = Vec::new();
    unsafe {
        let mut rate = 0.0;
        if (fb.get_display_refresh_rate)(session.as_raw(), &mut rate) == xr::sys::Result::SUCCESS
            && rate > 0.0
        {
            current = rate;
        }
        let mut count = 0;
        if (fb.enumerate_display_refresh_rates)(session.as_raw(), 0, &mut count, ptr::null_mut())
            == xr::sys::Result::SUCCESS
        {
            rates.resize(count as usize, 0.0);
            if (fb.enumerate_display_refresh_rates)(
                session.as_raw(),
                count,
                &mut count,
                rates.as_mut_ptr(),
            ) != xr::sys::Result::SUCCESS
            {
                count = 0;
            }
            rates.truncate(count as usize);
        }
    }
    if rates.is_empty() {
        rates.push(current);
    }
    (current, rates)
}

/// The runtime's times as CLOCK_MONOTONIC, which is Linux's clock too.
struct Clock {
    instance: xr::sys::Instance,
    convert: Option<xr::raw::ConvertTimespecTimeKHR>,
}

impl Clock {
    fn to_monotonic_ns(&self, time: xr::Time) -> i64 {
        if let Some(convert) = &self.convert {
            let mut timespec = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let result = unsafe {
                (convert.convert_time_to_timespec_time)(self.instance, time, &mut timespec)
            };
            if result == xr::sys::Result::SUCCESS {
                return timespec.tv_sec * 1_000_000_000 + timespec.tv_nsec;
            }
        }
        // Without the extension, take the runtime to count CLOCK_MONOTONIC nanoseconds.
        time.as_nanos()
    }
}

fn monotonic_ns() -> i64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}

const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: egl::Int = 0x30D2;
const EGL_SYNC_NATIVE_FENCE_ANDROID: u32 = 0x3144;
const EGL_SYNC_NATIVE_FENCE_FD_ANDROID: egl::Int = 0x3145;
const GL_FRAMEBUFFER_SRGB_EXT: u32 = 0x8DB9;

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
    /// GL_EXT_sRGB_write_control: writes to sRGB images can skip the encoding.
    srgb_write_control: bool,
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
        let srgb_write_control = gl
            .supported_extensions()
            .contains("GL_EXT_sRGB_write_control");
        if !srgb_write_control {
            log::warn!(
                "Immersive mode: GL lacks GL_EXT_sRGB_write_control; Linux's frames show too light"
            );
        }
        Ok(Self {
            egl,
            display,
            config,
            context,
            surface,
            gl,
            framebuffer,
            extensions,
            srgb_write_control,
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
    /// swapchain images, as they are: Linux's frames are sRGB-encoded already.
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
            if self.srgb_write_control {
                self.gl.disable(GL_FRAMEBUFFER_SRGB_EXT);
            }
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
            if self.srgb_write_control {
                self.gl.enable(GL_FRAMEBUFFER_SRGB_EXT);
            }
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
