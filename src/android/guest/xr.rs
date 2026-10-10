//! Immersive mode on demand, on headsets. Monado, the OpenXR runtime in the guest, connects to the
//! app's socket in the rootfs, learns the headset from it, and says when Linux apps run OpenXR
//! sessions: the app then enters immersive mode (`crate::android::xr`), and goes back to its
//! panel after they end. Each time immersive mode starts, its session offers Monado a channel and
//! the buffers frames go into, which this hands over. The messages are in
//! `crate::android::xr::protocol`.

use super::{notify, poll_entry, Event};
use crate::android::utils::java::AppClass;
use crate::android::xr::protocol::{self, Headset};
use crate::core::config::ARCH_FS_ROOT;
use std::fs;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use winit::platform::android::activity::AndroidApp;

/// Monado begins its session for a frame when it starts: a session must last this long for
/// immersive mode.
const ENTER_AFTER: Duration = Duration::from_millis(300);

/// Immersive mode stays this long after the last session ends, for an app that starts another.
const LEAVE_AFTER: Duration = Duration::from_secs(2);

/// What immersive mode's session hands Monado: the immersive message and its descriptors, the
/// channel first.
pub struct Offer {
    pub id: u64,
    pub message: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

static OFFER: Mutex<Option<Offer>> = Mutex::new(None);
static IMMERSIVE: AtomicBool = AtomicBool::new(false);

/// Hand Monado a channel and buffers, now or when it connects.
pub fn offer(offer: Offer) {
    *OFFER.lock().unwrap() = Some(offer);
    notify(Event::Immersive);
}

/// Take back an offer Monado hasn't had yet.
pub fn withdraw(id: u64) {
    let mut offer = OFFER.lock().unwrap();
    if offer.as_ref().is_some_and(|it| it.id == id) {
        *offer = None;
    }
}

/// Immersive mode started or ended.
pub fn set_immersive(on: bool) {
    IMMERSIVE.store(on, Ordering::Release);
}

/// Whether immersive mode is on: its activity has started, and not ended.
pub fn immersive() -> bool {
    IMMERSIVE.load(Ordering::Acquire)
}

/// The headset as immersive mode found it, for the next Monado.
pub fn remember(headset: &Headset) {
    let path = headset_file();
    let known = fs::read(&path).ok().and_then(|it| Headset::from_hello(&it));
    if known.as_ref() == Some(headset) {
        return;
    }
    if let Err(error) = fs::write(&path, headset.hello()) {
        log::warn!("Immersive mode: couldn't remember the headset: {error}");
    }
}

fn headset_file() -> PathBuf {
    Path::new(ARCH_FS_ROOT).with_file_name("xr-headset")
}

/// Whether the device is a headset: one with an OpenXR runtime of the system's.
fn headset() -> bool {
    [
        "/odm/etc/openxr",
        "/vendor/etc/openxr",
        "/system/etc/openxr",
        "/product/etc/openxr",
    ]
    .iter()
    .any(|path| Path::new(path).is_dir())
}

pub struct Job {
    activity: AppClass,
    listener: Option<OwnedFd>,
    monado: Option<OwnedFd>,
    /// What Monado said last: whether apps run sessions.
    running: bool,
    /// Whether immersive mode should be on, and from when.
    wanted: Option<(bool, Instant)>,
}

impl Job {
    /// For the calling thread, on headsets.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        if !headset() {
            return None;
        }
        Some(Self {
            activity: AppClass::new(android_app, "app.polarbear.XrActivity")?,
            listener: None,
            monado: None,
            running: false,
            wanted: None,
        })
    }

    /// Make the socket, once there is a rootfs.
    pub fn listen(&mut self) {
        if self.listener.is_some() {
            return;
        }
        let path = Path::new(ARCH_FS_ROOT).join(protocol::SOCKET);
        if !path.parent().is_some_and(Path::is_dir) {
            return;
        }
        match listen(&path) {
            Ok(listener) => self.listener = Some(listener),
            Err(error) => log::error!("Immersive mode: can't listen for Monado: {error}"),
        }
    }

    pub fn waits_for(&self, entries: &mut Vec<libc::pollfd>) {
        for fd in [&self.listener, &self.monado].into_iter().flatten() {
            entries.push(poll_entry(fd.as_raw_fd(), libc::POLLIN));
        }
    }

    pub fn ready(&mut self, fd: RawFd) {
        if self
            .listener
            .as_ref()
            .is_some_and(|it| it.as_raw_fd() == fd)
        {
            self.accept();
        } else if self.monado.as_ref().is_some_and(|it| it.as_raw_fd() == fd) {
            self.hear();
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.wanted.map(|(_, at)| at)
    }

    pub fn expire(&mut self, now: Instant) {
        let Some((on, at)) = self.wanted else {
            return;
        };
        if at > now {
            return;
        }
        self.wanted = None;
        if on == IMMERSIVE.load(Ordering::Acquire) {
            return;
        }
        let (method, what) = if on {
            log::info!("Immersive mode: Linux apps run OpenXR sessions");
            ("enter", "enter immersive mode")
        } else {
            log::info!("Immersive mode: the Linux apps' OpenXR sessions ended");
            ("leave", "leave immersive mode")
        };
        self.activity.call(what, |env, class, activity| {
            env.call_static_method(
                class,
                method,
                "(Landroid/app/Activity;)V",
                &[activity.into()],
            )?;
            Ok(())
        });
    }

    /// Hand Monado what immersive mode's session offers, if both are there.
    pub fn offered(&mut self) {
        let Some(monado) = &self.monado else {
            return;
        };
        let Some(offer) = OFFER.lock().unwrap().take() else {
            return;
        };
        let fds: Vec<RawFd> = offer.fds.iter().map(|it| it.as_raw_fd()).collect();
        if let Err(error) = protocol::send(monado, &offer.message, &fds) {
            log::warn!("Immersive mode: couldn't hand Monado the buffers: {error}");
        }
    }

    /// Monado connected, maybe after an earlier one: tell it the headset.
    fn accept(&mut self) {
        let Some(listener) = &self.listener else {
            return;
        };
        let flags = libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                ptr::null_mut(),
                ptr::null_mut(),
                flags,
            )
        };
        if fd < 0 {
            return;
        }
        let monado = unsafe { OwnedFd::from_raw_fd(fd) };
        let headset = fs::read(headset_file())
            .ok()
            .and_then(|it| Headset::from_hello(&it))
            .unwrap_or_else(Headset::guess);
        if let Err(error) = protocol::send(&monado, &headset.hello(), &[]) {
            log::warn!("Immersive mode: couldn't describe the headset to Monado: {error}");
            return;
        }
        log::info!("Immersive mode: Monado connected");
        self.monado = Some(monado);
        self.running = false;
        self.offered();
    }

    fn hear(&mut self) {
        loop {
            let Some(monado) = &self.monado else {
                return;
            };
            let mut message = [0u8; protocol::SESSION_SIZE + 1];
            match protocol::receive(monado, &mut message) {
                Ok((protocol::SESSION_SIZE, _))
                    if protocol::u32_at(&message, 0) == protocol::SESSION =>
                {
                    self.session(protocol::u32_at(&message, 4) != 0);
                }
                Ok((0, _)) => return self.left(),
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return self.left(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) => {
                    log::warn!("Immersive mode: the connection to Monado: {error}");
                    return self.left();
                }
                Ok((length, _)) => {
                    log::warn!("Immersive mode: a {length}-byte message from Monado")
                }
            }
        }
    }

    /// Apps began running OpenXR sessions, or the last one ended.
    fn session(&mut self, running: bool) {
        self.running = running;
        let delay = if running { ENTER_AFTER } else { LEAVE_AFTER };
        self.wanted = Some((running, Instant::now() + delay));
    }

    /// Monado quit, and with it its apps' sessions.
    fn left(&mut self) {
        log::info!("Immersive mode: Monado left");
        self.monado = None;
        if self.running {
            self.session(false);
        }
    }
}

/// A socket where every user in the guest may connect, replacing what an earlier one left.
fn listen(path: &Path) -> io::Result<OwnedFd> {
    let kind = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    let socket = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    if socket < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(socket) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let name = path.as_os_str().as_encoded_bytes();
    if name.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    for (slot, byte) in address.sun_path.iter_mut().zip(name) {
        *slot = *byte as libc::c_char;
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
