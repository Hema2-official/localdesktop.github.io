//! The messages between the app and Monado's Local Desktop driver in the guest. The driver's
//! patches/monado/src/xrt/drivers/localdesktop/ld_protocol.h is the reference: little-endian
//! structs of a fixed size per type, without padding, on SOCK_SEQPACKET sockets, with descriptors
//! as SCM_RIGHTS.
//!
//! Monado connects to the app's socket in the rootfs, the control connection: the app describes
//! the headset (hello), Monado says when Linux apps run OpenXR sessions (session), and each time
//! immersive mode starts, the app hands over a channel and the buffers frames go into
//! (immersive). On the channel go tracking, frames and releases, until it closes with immersive
//! mode.

use openxr as xr;
use std::ffi::c_void;
use std::io;
use std::mem::{size_of, size_of_val};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

/// The control socket, in the rootfs.
pub const SOCKET: &str = "tmp/localdesktop-xr.sock";

pub const MAGIC: u32 = 0x5258_4c44; // "LDXR"
pub const VERSION: u32 = 3;
pub const FRAME: u32 = 1;
pub const RELEASE: u32 = 2;
pub const TRACKING: u32 = 3;
pub const IMMERSIVE: u32 = 4;
pub const SESSION: u32 = 5;
pub const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;

pub const MAX_BUFFERS: usize = 8;
pub const MAX_VIEWS: usize = 2;
pub const MAX_REFRESH_RATES: usize = 8;
pub const MAX_HEAD_SAMPLES: usize = 4;

pub const HELLO_SIZE: usize = 100;
pub const IMMERSIVE_SIZE: usize = 100;
pub const SESSION_SIZE: usize = 8;
pub const TRACKING_SIZE: usize = 376;
pub const FRAME_SIZE: usize = 112;

/// What the app tells Monado of the headset when it connects: each view's size to render at and
/// field of view, and the refresh rates.
#[derive(Clone, PartialEq)]
pub struct Headset {
    pub views: Vec<(u32, u32, xr::Fovf)>,
    pub refresh_rate: f32,
    pub refresh_rates: Vec<f32>,
}

impl Headset {
    /// Before immersive mode has run once: a Quest 3's sizes and rates, and a field of view that
    /// immersive mode corrects when it lends buffers.
    pub fn guess() -> Self {
        let quarter = std::f32::consts::FRAC_PI_4;
        let fov = xr::Fovf {
            angle_left: -quarter,
            angle_right: quarter,
            angle_up: quarter,
            angle_down: -quarter,
        };
        Self {
            views: vec![(1680, 1760, fov); 2],
            refresh_rate: 90.0,
            refresh_rates: vec![72.0, 80.0, 90.0, 120.0],
        }
    }

    pub fn hello(&self) -> Vec<u8> {
        let mut message = Message::default();
        message.u32(MAGIC);
        message.u32(VERSION);
        message.u32(self.views.len().min(MAX_VIEWS) as u32);
        message.u32(self.refresh_rates.len().min(MAX_REFRESH_RATES) as u32);
        for index in 0..MAX_VIEWS {
            match self.views.get(index) {
                Some((width, height, fov)) => {
                    message.u32(*width);
                    message.u32(*height);
                    message.fov(fov);
                }
                None => message.zeros(24),
            }
        }
        message.f32(self.refresh_rate);
        for index in 0..MAX_REFRESH_RATES {
            message.f32(self.refresh_rates.get(index).copied().unwrap_or(0.0));
        }
        debug_assert_eq!(message.0.len(), HELLO_SIZE);
        message.0
    }

    /// From a hello, as `hello` made it.
    pub fn from_hello(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != HELLO_SIZE || u32_at(bytes, 0) != MAGIC || u32_at(bytes, 4) != VERSION {
            return None;
        }
        let view_count = (u32_at(bytes, 8) as usize).min(MAX_VIEWS);
        let rate_count = (u32_at(bytes, 12) as usize).min(MAX_REFRESH_RATES);
        let views = (0..view_count)
            .map(|index| {
                let offset = 16 + index * 24;
                (
                    u32_at(bytes, offset),
                    u32_at(bytes, offset + 4),
                    fov_at(bytes, offset + 8),
                )
            })
            .collect();
        let refresh_rates = (0..rate_count)
            .map(|index| f32_at(bytes, 68 + index * 4))
            .collect();
        Some(Self {
            views,
            refresh_rate: f32_at(bytes, 64),
            refresh_rates,
        })
    }
}

/// A message being put together.
#[derive(Default)]
pub struct Message(pub Vec<u8>);

impl Message {
    pub fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn f32(&mut self, value: f32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn zeros(&mut self, count: usize) {
        self.0.resize(self.0.len() + count, 0);
    }

    pub fn vector(&mut self, vector: &xr::Vector3f) {
        for value in [vector.x, vector.y, vector.z] {
            self.f32(value);
        }
    }

    pub fn pose(&mut self, pose: &xr::Posef) {
        self.vector(&pose.position);
        let orientation = pose.orientation;
        for value in [orientation.x, orientation.y, orientation.z, orientation.w] {
            self.f32(value);
        }
    }

    pub fn fov(&mut self, fov: &xr::Fovf) {
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

pub fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

pub fn i64_at(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

pub fn f32_at(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

pub fn pose_at(bytes: &[u8], offset: usize) -> xr::Posef {
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

pub fn fov_at(bytes: &[u8], offset: usize) -> xr::Fovf {
    let float = |index: usize| f32_at(bytes, offset + index * 4);
    xr::Fovf {
        angle_left: float(0),
        angle_right: float(1),
        angle_up: float(2),
        angle_down: float(3),
    }
}

/// One message, with `fds` as SCM_RIGHTS; never waits for room.
pub fn send(socket: &OwnedFd, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
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
    let flags = libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT;
    if unsafe { libc::sendmsg(socket.as_raw_fd(), &message, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One message into `data`, and the descriptors that came with it; never waits for one.
pub fn receive(socket: &OwnedFd, data: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
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
    let flags = libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT;
    let received = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, flags) };
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

/// A pair of connected SOCK_SEQPACKET sockets: ours, which never blocks, and the other end.
pub fn channel() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut pair = [0; 2];
    let kind = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC;
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, pair.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    let flags = unsafe { libc::fcntl(ours.as_raw_fd(), libc::F_GETFL) };
    unsafe { libc::fcntl(ours.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    Ok((ours, theirs))
}
