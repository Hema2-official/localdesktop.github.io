//! Linux's frames straight to the headset's compositor, with no copy: a surface swapchain
//! (XR_KHR_android_surface_swapchain), whose buffers the app lends Linux (`frames`). The producer
//! end of its Surface is `XrSurface` (Java), an ImageWriter. Each frame is queued with the time it
//! shows from, and the compositor picks the buffer it shows by that time
//! (XR_FB_android_surface_swapchain_create), so a frame shows with the poses it was rendered for:
//! the ones the session's layer has from the same display frame on.

use super::frames::{self, Frames, Ndk};
use crate::android::session::app_class;
use anyhow::{anyhow, bail, Context, Result};
use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::{JNIEnv, JavaVM};
use openxr as xr;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr;
use std::time::Duration;

pub struct Surface {
    pub swapchain: xr::Swapchain<xr::OpenGlEs>,
    /// The `XrSurface`.
    writer: GlobalRef,
    vm: JavaVM,
    ndk: Ndk,
}

impl Surface {
    /// A surface swapchain of `width` x `height`, and `count` of its buffers (acquired
    /// references), which the app may write.
    pub fn new(
        instance: &xr::Instance,
        session: &xr::Session<xr::OpenGlEs>,
        env: &mut JNIEnv,
        activity: &JObject,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<(Self, Vec<*mut c_void>)> {
        let create = instance
            .exts()
            .khr_android_surface_swapchain
            .ok_or_else(|| anyhow!("the runtime has no surface swapchains"))?
            .create_swapchain_android_surface;
        if instance
            .exts()
            .fb_android_surface_swapchain_create
            .is_none()
        {
            bail!("the runtime doesn't pick a surface's buffers by their times");
        }
        let flags = xr::sys::AndroidSurfaceSwapchainCreateInfoFB {
            ty: xr::sys::AndroidSurfaceSwapchainCreateInfoFB::TYPE,
            next: ptr::null(),
            create_flags: xr::sys::AndroidSurfaceSwapchainFlagsFB::SYNCHRONOUS
                | xr::sys::AndroidSurfaceSwapchainFlagsFB::USE_TIMESTAMPS,
        };
        // The Surface's producer decides the rest.
        let info = xr::sys::SwapchainCreateInfo {
            ty: xr::sys::SwapchainCreateInfo::TYPE,
            next: ptr::addr_of!(flags).cast(),
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            usage_flags: xr::SwapchainUsageFlags::SAMPLED,
            format: 0,
            sample_count: 0,
            width,
            height,
            face_count: 0,
            array_size: 0,
            mip_count: 0,
        };
        let mut handle = <xr::sys::Swapchain as xr::sys::Handle>::NULL;
        let mut surface: xr::sys::platform::jobject = ptr::null_mut();
        let result = unsafe { create(session.as_raw(), &info, &mut handle, &mut surface) };
        if result.into_raw() < 0 {
            bail!("xrCreateSwapchainAndroidSurfaceKHR: {result}");
        }
        let swapchain = unsafe { xr::Swapchain::from_raw(session.clone(), handle) };
        let vm = unsafe { JavaVM::from_raw(env.get_java_vm()?.get_java_vm_pointer())? };
        let ndk = Ndk::load().context("looking up AHardwareBuffer")?;
        let writer = env.with_local_frame(8, |env| -> Result<GlobalRef> {
            let class = app_class(env, activity, "app.polarbear.XrSurface");
            let class = checked(env, class)?;
            let surface = unsafe { JObject::from_raw(surface.cast()) };
            let writer = env.new_object(
                class,
                "(Landroid/view/Surface;IIIJ)V",
                &[
                    JValue::Object(&surface),
                    JValue::Int(count as i32),
                    JValue::Int(width as i32),
                    JValue::Int(height as i32),
                    // On top of the compositor's.
                    JValue::Long(frames::USAGE_COMPRESSED as i64),
                ],
            );
            let writer = checked(env, writer).context("making the Surface's writer")?;
            Ok(env.new_global_ref(writer)?)
        })?;
        let this = Self {
            swapchain,
            writer,
            vm,
            ndk,
        };
        let mut buffers = Vec::with_capacity(count);
        while buffers.len() < count {
            match this.dequeue(env) {
                Ok((buffer, _)) => buffers.push(buffer),
                Err(error) => {
                    for buffer in buffers {
                        unsafe { (this.ndk.release)(buffer) };
                    }
                    return Err(error.context("dequeueing the Surface's buffers"));
                }
            }
        }
        Ok((this, buffers))
    }

    /// A buffer the compositor is done with, acquired, and its id.
    fn dequeue(&self, env: &mut JNIEnv) -> Result<(*mut c_void, u64)> {
        env.with_local_frame(4, |env| -> Result<(*mut c_void, u64)> {
            let buffer = env
                .call_method(
                    &self.writer,
                    "dequeue",
                    "()Landroid/hardware/HardwareBuffer;",
                    &[],
                )
                .and_then(|it| it.l());
            let buffer = checked(env, buffer)?;
            let hardware_buffer =
                unsafe { (self.ndk.from_hardware_buffer)(env.get_raw(), buffer.as_raw()) };
            let id = (!hardware_buffer.is_null())
                .then(|| self.ndk.id(hardware_buffer))
                .flatten();
            if let Some(id) = id {
                unsafe { (self.ndk.acquire)(hardware_buffer) };
                let kept =
                    env.call_method(&self.writer, "keep", "(J)V", &[JValue::Long(id as i64)]);
                checked(env, kept)?;
            }
            let closed = env.call_method(&buffer, "close", "()V", &[]);
            let _ = checked(env, closed);
            let id =
                id.ok_or_else(|| anyhow!("no AHardwareBuffer or no id for a HardwareBuffer"))?;
            Ok((hardware_buffer, id))
        })
    }

    /// Give Linux back the buffers the compositor is done with.
    pub fn reclaim(&self, env: &mut JNIEnv, frames: &mut Frames) -> Result<()> {
        let count = env
            .call_method(&self.writer, "takeReleased", "()I", &[])
            .and_then(|it| it.i());
        for _ in 0..checked(env, count)? {
            let (hardware_buffer, id) = self.dequeue(env)?;
            unsafe { (self.ndk.release)(hardware_buffer) };
            match frames.index_of(id) {
                Some(index) => frames.give_back(index, None),
                None => bail!("the compositor gave back a buffer Linux doesn't have"),
            }
        }
        Ok(())
    }

    /// Show the frame in `buffer` from `time_ns` (CLOCK_MONOTONIC) on.
    pub fn queue(
        &self,
        env: &mut JNIEnv,
        frames: &Frames,
        buffer: usize,
        time_ns: i64,
    ) -> Result<()> {
        let id = frames
            .buffers
            .get(buffer)
            .and_then(|it| it.id)
            .ok_or_else(|| anyhow!("no buffer {buffer}"))?;
        let queued = env
            .call_method(
                &self.writer,
                "queue",
                "(JJ)Z",
                &[JValue::Long(id as i64), JValue::Long(time_ns)],
            )
            .and_then(|it| it.z());
        if !checked(env, queued)? {
            bail!("buffer {buffer} isn't the app's to queue");
        }
        Ok(())
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // Before the swapchain goes, with the Surface.
        if let Ok(mut env) = self.vm.get_env() {
            let closed = env.call_method(&self.writer, "close", "()V", &[]);
            let _ = checked(&mut env, closed);
        }
    }
}

/// Whether a sync file signals within `timeout`.
pub fn signalled(fence: &OwnedFd, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd: fence.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: timeout.subsec_nanos() as libc::c_long,
    };
    loop {
        match unsafe { libc::ppoll(&mut poll, 1, &timeout, ptr::null()) } {
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            result => return result == 1,
        }
    }
}

/// A JNI call's result. What Java threw is cleared, logged with its stack trace, and the error.
fn checked<T>(env: &mut JNIEnv, result: jni::errors::Result<T>) -> Result<T> {
    if !env.exception_check().unwrap_or(false) {
        return Ok(result?);
    }
    let thrown = env.exception_occurred();
    let _ = env.exception_clear();
    let trace = thrown.ok().and_then(|thrown| {
        let trace = env
            .call_static_method(
                "android/util/Log",
                "getStackTraceString",
                "(Ljava/lang/Throwable;)Ljava/lang/String;",
                &[JValue::Object(&thrown)],
            )
            .and_then(|it| it.l())
            .ok()?;
        env.get_string(&JString::from(trace)).ok().map(String::from)
    });
    let _ = env.exception_clear();
    let Some(trace) = trace else {
        bail!("Java threw, and its stack trace is lost");
    };
    log::warn!("Immersive mode: Java threw {trace}");
    bail!("{}", trace.lines().next().unwrap_or_default())
}
