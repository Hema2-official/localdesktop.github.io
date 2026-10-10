//! The frames Linux renders for immersive mode. The app allocates the buffers (AHardwareBuffers,
//! so Android's GL can read them) and lends them to Monado in the rootfs over a Unix socket, as
//! dma-bufs with a linear layout; the views share a buffer, side by side. Every display frame,
//! the app sends the head's tracking; Monado sends each frame back with the poses it rendered it
//! for, which the app submits it with, so the headset's compositor reprojects it.
//!
//! The messages, little-endian and of a fixed size per type, are those of Monado's Local Desktop
//! driver (patches/monado/src/xrt/drivers/localdesktop/ld_protocol.h); descriptors travel as
//! SCM_RIGHTS:
//!
//! - hello (app → Linux, on connecting): the buffers, the views and the refresh rates; a dma-buf
//!   per buffer.
//! - tracking (app → Linux, every display frame): its time, the views' poses and the head's pose
//!   predicted for the next few periods.
//! - frame (Linux → app): a buffer, its display time and the views it was rendered for; a sync
//!   file that signals when it's rendered, or none if it already is.
//! - release (app → Linux): a buffer; a sync file that signals when the app is done reading it,
//!   or none.
//!
//! Linux may render into any buffer the app doesn't hold: the app holds a buffer from its frame
//! message until its release.

use crate::core::config::ARCH_FS_ROOT;
use anyhow::{anyhow, bail, Context, Result};
use openxr as xr;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::mem::{size_of, size_of_val};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::ptr;

const HELLO: u32 = 0x5258_4c44; // "LDXR"
const VERSION: u32 = 2;
const FRAME: u32 = 1;
const RELEASE: u32 = 2;
const TRACKING: u32 = 3;
const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
/// Room for every buffer's descriptor in one message.
const MAX_BUFFERS: usize = 8;
pub const MAX_VIEWS: usize = 2;
const MAX_REFRESH_RATES: usize = 8;
pub const MAX_HEAD_SAMPLES: usize = 4;
const HELLO_SIZE: usize = 136;
const TRACKING_SIZE: usize = 376;
const FRAME_SIZE: usize = 112;

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

/// The head's pose the runtime predicted for a time, in the stage space.
pub struct HeadSample {
    pub time_ns: i64,
    pub pose: xr::Posef,
    pub linear_velocity: xr::Vector3f,
    /// In the stage space.
    pub angular_velocity: xr::Vector3f,
    /// Monado's `xrt_space_relation_flags`.
    pub flags: u32,
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
    pub head: Vec<HeadSample>,
}

/// A frame from Linux.
pub struct Frame {
    pub buffer: usize,
    /// To wait for before reading it.
    pub fence: Option<OwnedFd>,
    pub display_time_ns: i64,
    /// What it was rendered for: each view's pose in the stage space, and field of view.
    pub views: Vec<View>,
}

/// What came from Linux since the last look.
pub enum Update {
    Nothing,
    /// The newest frame.
    Frame(Frame),
    /// The program went away, and with it every frame.
    Disconnected,
}

pub struct Frames {
    ndk: Ndk,
    pub width: u32,
    pub height: u32,
    stride: u32,
    pub views: Vec<ViewArea>,
    refresh_rate: f32,
    refresh_rates: Vec<f32>,
    pub buffers: Vec<Buffer>,
    path: String,
    listener: OwnedFd,
    client: Option<OwnedFd>,
}

impl Frames {
    /// `count` buffers for `views` side by side, and the socket that lends them.
    pub fn new(
        views: Vec<ViewArea>,
        refresh_rate: f32,
        refresh_rates: Vec<f32>,
        count: usize,
    ) -> Result<Self> {
        if count == 0 || count > MAX_BUFFERS {
            bail!("{count} buffers");
        }
        if views.is_empty() || views.len() > MAX_VIEWS {
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
        for _ in 0..count {
            let mut hardware_buffer = ptr::null_mut();
            let status = unsafe { (ndk.allocate)(&desc, &mut hardware_buffer) };
            if status != 0 {
                for buffer in &buffers {
                    unsafe { (ndk.release)(buffer.hardware_buffer) };
                }
                bail!("allocating a {width}x{height} buffer: error {status}");
            }
            let mut actual = BufferDesc::default();
            unsafe { (ndk.describe)(hardware_buffer, &mut actual) };
            stride = actual.stride * 4;
            let dma_buf = match dma_buf_of(&ndk, hardware_buffer) {
                Ok(it) => it,
                Err(error) => {
                    unsafe { (ndk.release)(hardware_buffer) };
                    for buffer in &buffers {
                        unsafe { (ndk.release)(buffer.hardware_buffer) };
                    }
                    return Err(error.context("taking a buffer's dma-buf"));
                }
            };
            buffers.push(Buffer {
                hardware_buffer,
                dma_buf,
                held: false,
            });
        }
        let path = format!("{ARCH_FS_ROOT}/tmp/localdesktop-xr.sock");
        let listener = match listen(&path) {
            Ok(it) => it,
            Err(error) => {
                for buffer in &buffers {
                    unsafe { (ndk.release)(buffer.hardware_buffer) };
                }
                return Err(anyhow!(error).context(format!("listening on {path}")));
            }
        };
        let mut refresh_rates = refresh_rates;
        refresh_rates.truncate(MAX_REFRESH_RATES);
        Ok(Self {
            ndk,
            width,
            height,
            stride,
            views,
            refresh_rate,
            refresh_rates,
            buffers,
            path,
            listener,
            client: None,
        })
    }

    fn hello(&self) -> Vec<u8> {
        let mut message = Message::default();
        for value in [
            HELLO,
            VERSION,
            self.width,
            self.height,
            self.stride,
            DRM_FORMAT_ABGR8888,
            self.buffers.len() as u32,
            self.views.len() as u32,
        ] {
            message.u32(value);
        }
        for index in 0..MAX_VIEWS {
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
        message.u32(self.refresh_rates.len() as u32);
        for index in 0..MAX_REFRESH_RATES {
            message.f32(self.refresh_rates.get(index).copied().unwrap_or(0.0));
        }
        debug_assert_eq!(message.0.len(), HELLO_SIZE);
        message.0
    }

    /// Take a program that connects (lending it every buffer) and what it sent since the last
    /// look. Frames older than the newest go back at once: the app never shows them.
    pub fn update(&mut self) -> Update {
        if self.client.is_none() {
            let client = unsafe {
                libc::accept4(
                    self.listener.as_raw_fd(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            if client < 0 {
                return Update::Nothing;
            }
            let client = unsafe { OwnedFd::from_raw_fd(client) };
            let dma_bufs: Vec<RawFd> = self
                .buffers
                .iter()
                .map(|it| it.dma_buf.as_raw_fd())
                .collect();
            if let Err(error) = send(&client, &self.hello(), &dma_bufs) {
                log::warn!("Immersive mode: couldn't lend the buffers: {error}");
                return Update::Nothing;
            }
            log::info!("Immersive mode: a program in Linux takes frames");
            self.client = Some(client);
        }

        let mut newest: Option<Frame> = None;
        loop {
            let mut message = [0u8; FRAME_SIZE + 1];
            let received = match self.client.as_ref() {
                Some(client) => receive(client, &mut message),
                None => break,
            };
            match received {
                Ok((0, _)) => return self.disconnected(None),
                // A program that exits with messages it hasn't read resets the connection.
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {
                    return self.disconnected(None)
                }
                Ok((FRAME_SIZE, mut fds)) if u32_at(&message, 0) == FRAME => {
                    let buffer = u32_at(&message, 4) as usize;
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
                            pose: pose_at(&message, 24 + index * 44),
                            fov: fov_at(&message, 24 + index * 44 + 28),
                        })
                        .collect();
                    newest = Some(Frame {
                        buffer,
                        fence: fds.pop(),
                        display_time_ns: i64_at(&message, 16),
                        views,
                    });
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

    /// The program went away, or its socket broke: every buffer is the app's to lend again.
    fn disconnected(&mut self, error: Option<io::Error>) -> Update {
        match error {
            None => log::info!("Immersive mode: the program in Linux is gone"),
            Some(error) => log::warn!("Immersive mode: the frames' socket: {error}"),
        }
        self.client = None;
        for buffer in &mut self.buffers {
            buffer.held = false;
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
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let mut release = Message::default();
        release.u32(RELEASE);
        release.u32(buffer as u32);
        let fds: Vec<RawFd> = fence.iter().map(|it| it.as_raw_fd()).collect();
        if let Err(error) = send(client, &release.0, &fds) {
            log::warn!("Immersive mode: couldn't give buffer {buffer} back: {error}");
        }
    }

    /// Tell the program in Linux, if there is one, when this display frame shows and where the
    /// head will be.
    pub fn send_tracking(&self, tracking: &Tracking) {
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let samples = tracking.head.len().min(MAX_HEAD_SAMPLES);
        let mut message = Message::default();
        message.u32(TRACKING);
        message.u32(samples as u32);
        message.i64(tracking.display_time_ns);
        message.i64(tracking.display_period_ns);
        message.i64(tracking.latch_time_ns);
        for index in 0..MAX_VIEWS {
            match tracking.view_poses.get(index) {
                Some(view) => {
                    message.pose(&view.pose);
                    message.fov(&view.fov);
                }
                None => message.zeros(44),
            }
        }
        for index in 0..MAX_HEAD_SAMPLES {
            match tracking.head.get(index).filter(|_| index < samples) {
                Some(sample) => {
                    message.i64(sample.time_ns);
                    message.pose(&sample.pose);
                    message.vector(&sample.linear_velocity);
                    message.vector(&sample.angular_velocity);
                    message.u32(sample.flags);
                }
                None => message.zeros(64),
            }
        }
        debug_assert_eq!(message.0.len(), TRACKING_SIZE);
        if let Err(error) = send(client, &message.0, &[]) {
            if error.kind() != io::ErrorKind::WouldBlock {
                log::warn!("Immersive mode: couldn't send the tracking: {error}");
            }
        }
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        for buffer in &self.buffers {
            unsafe { (self.ndk.release)(buffer.hardware_buffer) };
        }
    }
}

/// A message being put together.
#[derive(Default)]
struct Message(Vec<u8>);

impl Message {
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn f32(&mut self, value: f32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn zeros(&mut self, count: usize) {
        self.0.resize(self.0.len() + count, 0);
    }

    fn vector(&mut self, vector: &xr::Vector3f) {
        for value in [vector.x, vector.y, vector.z] {
            self.f32(value);
        }
    }

    fn pose(&mut self, pose: &xr::Posef) {
        self.vector(&pose.position);
        let orientation = pose.orientation;
        for value in [orientation.x, orientation.y, orientation.z, orientation.w] {
            self.f32(value);
        }
    }

    fn fov(&mut self, fov: &xr::Fovf) {
        for value in [
            fov.angle_left,
            fov.angle_right,
            fov.angle_up,
            fov.angle_down,
        ] {
            self.f32(value);
        }
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn i64_at(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn f32_at(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn pose_at(bytes: &[u8], offset: usize) -> xr::Posef {
    let float = |index: usize| f32_at(bytes, offset + index * 4);
    xr::Posef {
        position: xr::Vector3f {
            x: float(0),
            y: float(1),
            z: float(2),
        },
        orientation: xr::Quaternionf {
            x: float(3),
            y: float(4),
            z: float(5),
            w: float(6),
        },
    }
}

fn fov_at(bytes: &[u8], offset: usize) -> xr::Fovf {
    let float = |index: usize| f32_at(bytes, offset + index * 4);
    xr::Fovf {
        angle_left: float(0),
        angle_right: float(1),
        angle_up: float(2),
        angle_down: float(3),
    }
}

/// The dma-buf behind an AHardwareBuffer. Android's public way to share one sends its native
/// handle over a socket; its first descriptor is the buffer's memory, the rest (metadata) aren't
/// needed.
fn dma_buf_of(ndk: &Ndk, hardware_buffer: *mut c_void) -> Result<OwnedFd> {
    let mut pair = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let (sender, receiver) =
        unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    let status = unsafe { (ndk.send_handle)(hardware_buffer, sender.as_raw_fd()) };
    if status != 0 {
        bail!("AHardwareBuffer_sendHandleToUnixSocket: error {status}");
    }
    let mut handle = [0u8; 4096];
    let (_, mut fds) = receive(&receiver, &mut handle)?;
    if fds.is_empty() {
        bail!("the handle came without descriptors");
    }
    Ok(fds.remove(0))
}

/// A socket where every user in the guest may connect, replacing what an earlier one left.
fn listen(path: &str) -> io::Result<OwnedFd> {
    let socket = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if socket < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(socket) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if path.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    for (slot, byte) in address.sun_path.iter_mut().zip(path.bytes()) {
        *slot = byte as libc::c_char;
    }
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if bound != 0 || unsafe { libc::listen(socket.as_raw_fd(), 1) } != 0 {
        return Err(io::Error::last_os_error());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    Ok(socket)
}

/// One message, with `fds` as SCM_RIGHTS.
fn send(socket: &OwnedFd, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(size_of_val(fds) as u32) } as usize];
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut c_void,
        iov_len: data.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if !fds.is_empty() {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of_val(fds) as u32) as _;
            ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(header).cast(), fds.len());
        }
    }
    if unsafe { libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One message into `data`, and the descriptors that came with it.
fn receive(socket: &OwnedFd, data: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut control = [0u8; 256];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;
    let received =
        unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let count =
                    ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / size_of::<RawFd>();
                let first: *const RawFd = libc::CMSG_DATA(header).cast();
                for index in 0..count {
                    fds.push(OwnedFd::from_raw_fd(ptr::read_unaligned(first.add(index))));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    Ok((received as usize, fds))
}
