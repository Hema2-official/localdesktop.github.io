//! The frames Linux renders for immersive mode. The app allocates the buffers (AHardwareBuffers,
//! so Android's GL can read them) and lends them to Monado in the guest as dma-bufs with a linear
//! layout, the views side by side, together with a channel of their own (`protocol`): the guest
//! link hands both over on its control connection. Every display frame the app sends the head's
//! tracking and the controllers' state on the channel; Monado sends each frame back with the poses
//! it rendered it for, which the app submits it with, so the headset's compositor reprojects it,
//! and haptic pulses for the controllers.
//!
//! Linux may render into any buffer the app doesn't hold: the app holds a buffer from its frame
//! message until its release. The channel closing on Monado's side (it quit) gives every buffer
//! back, and the app offers a new channel for the next Monado.

use super::protocol::{self, Controller, Haptic, Message, PoseSample};
use crate::android::guest;
use anyhow::{bail, Context, Result};
use openxr as xr;
use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

// AHardwareBuffer (NDK, API level 26), looked up at run time: the app's minimum level is lower.
#[repr(C)]
#[derive(Default)]
struct BufferDesc {
    width: u32,
    height: u32,
    layers: u32,
    format: u32,
    usage: u64,
    stride: u32,
    rfu0: u32,
    rfu1: u64,
}

const FORMAT_R8G8B8A8_UNORM: u32 = 1;
/// CPU access keeps the layout linear (no UBWC), which Linux can describe to its driver.
const USAGE_CPU_READ_RARELY: u64 = 2;
const USAGE_GPU_SAMPLED_IMAGE: u64 = 1 << 8;
const USAGE_GPU_FRAMEBUFFER: u64 = 1 << 9;

/// Tells the offers of one set of buffers from those of the next.
static OFFERS: AtomicU64 = AtomicU64::new(0);

struct Ndk {
    allocate: unsafe extern "C" fn(*const BufferDesc, *mut *mut c_void) -> i32,
    describe: unsafe extern "C" fn(*const c_void, *mut BufferDesc),
    release: unsafe extern "C" fn(*mut c_void),
    send_handle: unsafe extern "C" fn(*const c_void, i32) -> i32,
    _library: libloading::Library,
}

impl Ndk {
    fn load() -> Result<Self> {
        unsafe {
            let library = libloading::Library::new("libandroid.so")?;
            Ok(Self {
                allocate: *library.get(b"AHardwareBuffer_allocate\0")?,
                describe: *library.get(b"AHardwareBuffer_describe\0")?,
                release: *library.get(b"AHardwareBuffer_release\0")?,
                send_handle: *library.get(b"AHardwareBuffer_sendHandleToUnixSocket\0")?,
                _library: library,
            })
        }
    }
}

/// A buffer the app lends to Linux.
pub struct Buffer {
    /// The `AHardwareBuffer`, for importing into GL.
    pub hardware_buffer: *mut c_void,
    dma_buf: OwnedFd,
    /// Linux handed it over in a frame message and hasn't had it back.
    held: bool,
}

/// Where a view goes in the buffers, which is also the size to render it at, and its field of
/// view.
#[derive(Clone, Copy)]
pub struct ViewArea {
    pub x: u32,
    pub width: u32,
    pub height: u32,
    pub fov: xr::Fovf,
}

#[derive(Clone, Copy)]
pub struct View {
    pub pose: xr::Posef,
    pub fov: xr::Fovf,
}

/// What the app sends Linux every display frame. Times are CLOCK_MONOTONIC nanoseconds.
pub struct Tracking {
    pub display_time_ns: i64,
    pub display_period_ns: i64,
    /// When the app takes the newest frame for this display frame.
    pub latch_time_ns: i64,
    /// Relative to the head.
    pub view_poses: Vec<View>,
    /// In increasing time order.
    pub head: Vec<PoseSample>,
}

/// A frame from Linux.
pub struct Frame {
    pub buffer: usize,
    /// To wait for before reading it.
    pub fence: Option<OwnedFd>,
    pub display_time_ns: i64,
    /// Whether to show it over the surroundings, blended by its (premultiplied) alpha.
    pub alpha_blend: bool,
    /// What it was rendered for: each view's pose in the stage space, and field of view.
    pub views: Vec<View>,
}

/// What came from Linux since the last look.
pub enum Update {
    Nothing,
    /// The newest frame.
    Frame(Frame),
    /// Monado went away, and with it every frame.
    Disconnected,
}

pub struct Frames {
    ndk: Ndk,
    pub width: u32,
    pub height: u32,
    stride: u32,
    pub views: Vec<ViewArea>,
    refresh_rate: f32,
    pub buffers: Vec<Buffer>,
    /// Our end of the channel.
    channel: OwnedFd,
    /// The offer of the other end, until Monado has it.
    offer: u64,
    /// Monado has sent on the channel.
    heard: bool,
    /// What Monado asked of the controllers' haptics since the last look.
    haptics: Vec<Haptic>,
    /// The refresh rate Monado asked for last, until the app takes it.
    refresh_rate_request: Option<f32>,
    /// Whether the headset shows immersive mode, as Monado was told last.
    state: u32,
}

impl Frames {
    /// `count` buffers for `views` side by side, offered to Monado.
    pub fn new(views: Vec<ViewArea>, refresh_rate: f32, count: usize) -> Result<Self> {
        if count == 0 || count > protocol::MAX_BUFFERS {
            bail!("{count} buffers");
        }
        if views.is_empty() || views.len() > protocol::MAX_VIEWS {
            bail!("{} views", views.len());
        }
        let width = views
            .iter()
            .map(|view| view.x + view.width)
            .max()
            .unwrap_or(0);
        let height = views.iter().map(|view| view.height).max().unwrap_or(0);
        let ndk = Ndk::load().context("looking up AHardwareBuffer")?;
        let desc = BufferDesc {
            width,
            height,
            layers: 1,
            format: FORMAT_R8G8B8A8_UNORM,
            usage: USAGE_GPU_SAMPLED_IMAGE | USAGE_GPU_FRAMEBUFFER | USAGE_CPU_READ_RARELY,
            ..Default::default()
        };
        let mut buffers: Vec<Buffer> = Vec::with_capacity(count);
        let mut stride = 0;
        let release_all = |buffers: &[Buffer]| {
            for buffer in buffers {
                unsafe { (ndk.release)(buffer.hardware_buffer) };
            }
        };
        for _ in 0..count {
            let mut hardware_buffer = ptr::null_mut();
            let status = unsafe { (ndk.allocate)(&desc, &mut hardware_buffer) };
            if status != 0 {
                release_all(&buffers);
                bail!("allocating a {width}x{height} buffer: error {status}");
            }
            let mut actual = BufferDesc::default();
            unsafe { (ndk.describe)(hardware_buffer, &mut actual) };
            stride = actual.stride * 4;
            let dma_buf = match dma_buf_of(&ndk, hardware_buffer) {
                Ok(it) => it,
                Err(error) => {
                    unsafe { (ndk.release)(hardware_buffer) };
                    release_all(&buffers);
                    return Err(error.context("taking a buffer's dma-buf"));
                }
            };
            buffers.push(Buffer {
                hardware_buffer,
                dma_buf,
                held: false,
            });
        }
        let channel = match protocol::channel() {
            Ok((ours, theirs)) => {
                let mut frames = Self {
                    ndk,
                    width,
                    height,
                    stride,
                    views,
                    refresh_rate,
                    buffers,
                    channel: ours,
                    offer: 0,
                    heard: false,
                    haptics: Vec::new(),
                    refresh_rate_request: None,
                    state: protocol::STATE_VISIBLE | protocol::STATE_FOCUSED,
                };
                frames.offer(theirs)?;
                return Ok(frames);
            }
            Err(error) => error,
        };
        release_all(&buffers);
        Err(anyhow::Error::from(channel).context("making the frames' channel"))
    }

    /// Hand Monado (through the guest link) the other end of the channel, and the buffers.
    fn offer(&mut self, theirs: OwnedFd) -> Result<()> {
        let mut message = Message::default();
        for value in [
            protocol::IMMERSIVE,
            self.buffers.len() as u32,
            self.width,
            self.height,
            self.stride,
            protocol::DRM_FORMAT_ABGR8888,
            self.views.len() as u32,
            0,
        ] {
            message.u32(value);
        }
        for index in 0..protocol::MAX_VIEWS {
            match self.views.get(index) {
                Some(view) => {
                    for value in [view.x, 0, view.width, view.height] {
                        message.u32(value);
                    }
                    message.fov(&view.fov);
                }
                None => message.zeros(32),
            }
        }
        message.f32(self.refresh_rate);
        debug_assert_eq!(message.0.len(), protocol::IMMERSIVE_SIZE);
        let mut fds = vec![theirs];
        for buffer in &self.buffers {
            fds.push(buffer.dma_buf.try_clone().context("sharing a buffer")?);
        }
        self.offer = OFFERS.fetch_add(1, Ordering::Relaxed) + 1;
        guest::xr::offer(guest::xr::Offer {
            id: self.offer,
            message: message.0,
            fds,
        });
        Ok(())
    }

    /// What Monado sent since the last look. Frames older than the newest go back at once: the
    /// app never shows them.
    pub fn update(&mut self) -> Update {
        let mut newest: Option<Frame> = None;
        loop {
            let mut message = [0u8; protocol::FRAME_SIZE + 1];
            match protocol::receive(&self.channel, &mut message) {
                Ok((0, _)) => return self.disconnected(None),
                // Monado exiting with messages it hasn't read resets the channel.
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {
                    return self.disconnected(None)
                }
                Ok((protocol::FRAME_SIZE, mut fds))
                    if protocol::u32_at(&message, 0) == protocol::FRAME =>
                {
                    if !self.heard {
                        log::info!("Immersive mode: frames from Linux");
                        self.heard = true;
                    }
                    let buffer = protocol::u32_at(&message, 4) as usize;
                    if buffer >= self.buffers.len() || self.buffers[buffer].held {
                        log::warn!(
                            "Immersive mode: Linux sent buffer {buffer}, which it doesn't have"
                        );
                        continue;
                    }
                    self.buffers[buffer].held = true;
                    if let Some(skipped) = newest.take() {
                        self.give_back(skipped.buffer, None);
                    }
                    let views = (0..self.views.len())
                        .map(|index| View {
                            pose: protocol::pose_at(&message, 32 + index * 44),
                            fov: protocol::fov_at(&message, 32 + index * 44 + 28),
                        })
                        .collect();
                    let flags = protocol::u32_at(&message, 24);
                    newest = Some(Frame {
                        buffer,
                        fence: fds.pop(),
                        display_time_ns: protocol::i64_at(&message, 16),
                        alpha_blend: flags & protocol::FRAME_ALPHA_BLEND != 0,
                        views,
                    });
                }
                Ok((protocol::HAPTIC_SIZE, _)) => {
                    match Haptic::from_message(&message[..protocol::HAPTIC_SIZE]) {
                        Some(haptic) => self.haptics.push(haptic),
                        None => log::warn!("Immersive mode: an unknown message from Linux"),
                    }
                }
                Ok((protocol::REFRESH_RATE_SIZE, _))
                    if protocol::u32_at(&message, 0) == protocol::REFRESH_RATE =>
                {
                    self.refresh_rate_request = Some(protocol::f32_at(&message, 4));
                }
                Ok((length, _)) => log::warn!("Immersive mode: a {length}-byte message from Linux"),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return self.disconnected(Some(error)),
            }
        }
        match newest {
            Some(frame) => Update::Frame(frame),
            None => Update::Nothing,
        }
    }

    /// Monado closed the channel, or it broke: every buffer is the app's again, and the next
    /// Monado gets a new channel.
    fn disconnected(&mut self, error: Option<io::Error>) -> Update {
        match error {
            None if self.heard => log::info!("Immersive mode: Monado is gone"),
            None => {}
            Some(error) => log::warn!("Immersive mode: the frames' channel: {error}"),
        }
        for buffer in &mut self.buffers {
            buffer.held = false;
        }
        self.heard = false;
        match protocol::channel() {
            Ok((ours, theirs)) => {
                self.channel = ours;
                if let Err(error) = self.offer(theirs) {
                    log::warn!("Immersive mode: no buffers for the next Monado: {error:#}");
                }
                self.send_state(self.state);
            }
            Err(error) => log::warn!("Immersive mode: no channel for the next Monado: {error}"),
        }
        Update::Disconnected
    }

    /// Return a buffer the app had from Linux; `fence` signals when the app is done reading it.
    pub fn give_back(&mut self, buffer: usize, fence: Option<OwnedFd>) {
        let Some(slot) = self.buffers.get_mut(buffer) else {
            return;
        };
        if !slot.held {
            return;
        }
        slot.held = false;
        let mut release = Message::default();
        release.u32(protocol::RELEASE);
        release.u32(buffer as u32);
        let fds: Vec<RawFd> = fence.iter().map(|it| it.as_raw_fd()).collect();
        if let Err(error) = protocol::send(&self.channel, &release.0, &fds) {
            log::warn!("Immersive mode: couldn't give buffer {buffer} back: {error}");
        }
    }

    /// Tell Monado, if it's there, when this display frame shows and where the head will be.
    pub fn send_tracking(&self, tracking: &Tracking) {
        let samples = tracking.head.len().min(protocol::MAX_HEAD_SAMPLES);
        let mut message = Message::default();
        message.u32(protocol::TRACKING);
        message.u32(samples as u32);
        message.i64(tracking.display_time_ns);
        message.i64(tracking.display_period_ns);
        message.i64(tracking.latch_time_ns);
        for index in 0..protocol::MAX_VIEWS {
            match tracking.view_poses.get(index) {
                Some(view) => {
                    message.pose(&view.pose);
                    message.fov(&view.fov);
                }
                None => message.zeros(44),
            }
        }
        for index in 0..protocol::MAX_HEAD_SAMPLES {
            match tracking.head.get(index).filter(|_| index < samples) {
                Some(sample) => message.sample(sample),
                None => message.zeros(64),
            }
        }
        debug_assert_eq!(message.0.len(), protocol::TRACKING_SIZE);
        self.send(&message.0, "the tracking");
    }

    /// Tell Monado the controllers' state, read at `time_ns`.
    pub fn send_controllers(&self, time_ns: i64, hands: &[Controller; 2]) {
        let mut message = Message::default();
        message.u32(protocol::CONTROLLERS);
        message.u32(0);
        message.i64(time_ns);
        for hand in hands {
            message.u32(hand.flags);
            message.u32(hand.buttons);
            message.f32(hand.trigger);
            message.f32(hand.squeeze);
            message.f32(hand.thumbstick.x);
            message.f32(hand.thumbstick.y);
            message.sample(&hand.grip);
            message.sample(&hand.aim);
        }
        debug_assert_eq!(message.0.len(), protocol::CONTROLLERS_SIZE);
        self.send(&message.0, "the controllers' state");
    }

    /// Tell Monado whether the headset shows immersive mode (`protocol::STATE_*`). Sent again on
    /// a new channel.
    pub fn send_state(&mut self, flags: u32) {
        self.state = flags;
        let mut message = Message::default();
        message.u32(protocol::STATE);
        message.u32(flags);
        self.send(&message.0, "the session's state");
    }

    /// Tell Monado where the hands' joints are (a hands message).
    pub fn send_hands(&self, message: &[u8]) {
        self.send(message, "the hands");
    }

    /// What Monado asked of the controllers' haptics since the last look.
    pub fn take_haptics(&mut self) -> Vec<Haptic> {
        std::mem::take(&mut self.haptics)
    }

    /// The refresh rate Monado asked for since the last look, if it did.
    pub fn take_refresh_rate_request(&mut self) -> Option<f32> {
        self.refresh_rate_request.take()
    }

    fn send(&self, message: &[u8], what: &str) {
        // Nobody reads the channel until Monado has it.
        if let Err(error) = protocol::send(&self.channel, message, &[]) {
            if error.kind() != io::ErrorKind::WouldBlock {
                log::warn!("Immersive mode: couldn't send {what}: {error}");
            }
        }
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        guest::xr::withdraw(self.offer);
        for buffer in &self.buffers {
            unsafe { (self.ndk.release)(buffer.hardware_buffer) };
        }
    }
}

/// The dma-buf behind an AHardwareBuffer. Android's public way to share one sends its native
/// handle over a socket; its first descriptor is the buffer's memory, the rest (metadata) aren't
/// needed.
fn dma_buf_of(ndk: &Ndk, hardware_buffer: *mut c_void) -> Result<OwnedFd> {
    let (receiver, sender) = protocol::channel()?;
    let status = unsafe { (ndk.send_handle)(hardware_buffer, sender.as_raw_fd()) };
    if status != 0 {
        bail!("AHardwareBuffer_sendHandleToUnixSocket: error {status}");
    }
    let mut handle = [0u8; 4096];
    let (_, mut fds) = protocol::receive(&receiver, &mut handle)?;
    if fds.is_empty() {
        bail!("the handle came without descriptors");
    }
    Ok(fds.remove(0))
}
