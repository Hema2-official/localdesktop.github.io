//! The guest's system bus, which the app serves itself (`core::bus`), with Android's battery on it
//! as UPower (`core::upower`) for the desktop's battery widget.
//!
//! Its socket is where every program looks for the system bus, `/run/dbus/system_bus_socket` in
//! the rootfs, which programs under proot reach like any socket of the guest's. The app makes it
//! as soon as there is a rootfs, before any desktop session, since Plasma looks for UPower only
//! when it starts; and leaves the place to a system bus that answers there already. Android's
//! battery broadcast wakes the link only when the charge or the plug changes.

use super::poll_entry;
use crate::android::battery::Battery;
use crate::core::bus::Bus;
use crate::core::config::ARCH_FS_ROOT;
use crate::core::upower::{AndroidBattery, UPower};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use winit::platform::android::activity::AndroidApp;

/// More clients than any desktop has on its system bus.
const MAX_CLIENTS: usize = 64;

pub struct Job {
    battery: Battery,
    /// What Android said last, while the battery is shared.
    last: Option<AndroidBattery>,
    served: Option<Served>,
}

struct Served {
    path: PathBuf,
    listener: UnixListener,
    clients: BTreeMap<u32, UnixStream>,
    next: u32,
    bus: Bus<UPower>,
}

impl Job {
    /// For the calling thread. `None` if Android's battery can't be reached.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        Some(Self {
            battery: Battery::new(android_app)?,
            last: None,
            served: None,
        })
    }

    /// Sharing the battery is on or off, as the config says now. Turning it on again after a fresh
    /// install, when there is a rootfs at last, makes the socket.
    pub fn turn(&mut self, on: bool) {
        match (on, self.served.is_some()) {
            (true, false) => self.serve(),
            (false, true) => self.stop(),
            _ => {}
        }
    }

    fn serve(&mut self) {
        let run = Path::new(ARCH_FS_ROOT).join("run");
        if !run.is_dir() {
            return;
        }
        let path = run.join("dbus/system_bus_socket");
        if UnixStream::connect(&path).is_ok() {
            log::info!("System bus: another one answers at /run/dbus/system_bus_socket");
            return;
        }
        let listener = match listen(&path) {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("System bus: no socket at {}: {error}", path.display());
                return;
            }
        };
        self.battery.watch(true);
        let (maker, model) = self.battery.phone();
        let mut upower = UPower::new(&maker, &model);
        self.last = self.battery.read();
        if let Some(battery) = &self.last {
            upower.update(battery.clone(), now());
        }
        let bus = Bus::new(guid(), std::process::id(), upower);
        self.served = Some(Served {
            path,
            listener,
            clients: BTreeMap::new(),
            next: 1,
            bus,
        });
        log::info!("System bus: UPower tells Android's battery at /run/dbus/system_bus_socket");
    }

    fn stop(&mut self) {
        if let Some(served) = self.served.take() {
            let _ = fs::remove_file(&served.path);
            log::info!("System bus: stopped");
        }
        self.battery.watch(false);
        self.last = None;
    }

    /// Android's battery as it was last read, while it's shared.
    pub fn battery(&self) -> Option<&AndroidBattery> {
        self.last.as_ref()
    }

    /// Android's battery changed its charge or plug: the battery now, while it's shared.
    pub fn battery_changed(&mut self) -> Option<&AndroidBattery> {
        let served = self.served.as_mut()?;
        let battery = self.battery.read()?;
        log::trace!(
            "System bus: the battery is at {} %, state {}",
            battery.percentage(),
            battery.state()
        );
        let signals = served.bus.service().update(battery.clone(), now());
        for signal in &signals {
            served.bus.emit(signal);
        }
        served.flush();
        self.last = Some(battery);
        self.last.as_ref()
    }

    pub fn waits_for(&self, entries: &mut Vec<libc::pollfd>) {
        let Some(served) = &self.served else {
            return;
        };
        entries.push(poll_entry(served.listener.as_raw_fd(), libc::POLLIN));
        for (id, client) in &served.clients {
            let events = if served.bus.has_output(*id) {
                libc::POLLIN | libc::POLLOUT
            } else {
                libc::POLLIN
            };
            entries.push(poll_entry(client.as_raw_fd(), events));
        }
    }

    /// `fd` is ready, if it's one of the bus's.
    pub fn ready(&mut self, fd: RawFd, revents: i16) {
        let Some(served) = &mut self.served else {
            return;
        };
        if fd == served.listener.as_raw_fd() {
            served.accept();
        } else if let Some(id) = served.id_of(fd) {
            if revents & !libc::POLLOUT != 0 {
                served.read(id);
            }
        } else {
            return;
        }
        served.flush();
    }
}

impl Served {
    fn id_of(&self, fd: RawFd) -> Option<u32> {
        self.clients
            .iter()
            .find(|(_, client)| client.as_raw_fd() == fd)
            .map(|(id, _)| *id)
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((client, _)) => {
                    // Dropped, it's closed.
                    if self.clients.len() >= MAX_CLIENTS || client.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.next;
                    self.next += 1;
                    self.bus.accept(id, peer_pid(&client));
                    self.clients.insert(id, client);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) => {
                    log::info!("System bus: no new connection: {error}");
                    return;
                }
            }
        }
    }

    /// What the client sent. A client that left or broke the protocol is disconnected.
    fn read(&mut self, id: u32) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        let mut buffer = [0u8; 16384];
        let open = loop {
            match client.read(&mut buffer) {
                Ok(0) => break false,
                Ok(read) => {
                    if !self.bus.received(id, &buffer[..read]) {
                        break false;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break true,
                Err(_) => break false,
            }
        };
        if !open {
            self.close(id);
        }
    }

    fn close(&mut self, id: u32) {
        self.clients.remove(&id);
        self.bus.closed(id);
    }

    /// Write what the bus has for its clients, as far as their sockets take it.
    fn flush(&mut self) {
        let ids: Vec<u32> = self.clients.keys().copied().collect();
        for id in ids {
            let open = match (self.bus.output(id), self.clients.get_mut(&id)) {
                (Some(output), Some(client)) => write_some(client, output),
                _ => false,
            };
            if !open {
                self.close(id);
            }
        }
    }
}

/// Write as much of `output` as the socket takes, and take that off. False if the client is gone.
fn write_some(client: &mut UnixStream, output: &mut Vec<u8>) -> bool {
    let mut written = 0;
    let open = loop {
        if written == output.len() {
            break true;
        }
        match client.write(&output[written..]) {
            Ok(0) => break false,
            Ok(n) => written += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break true,
            Err(_) => break false,
        }
    };
    output.drain(..written);
    open
}

/// A socket where every user in the guest may connect, replacing what a bus that has gone left.
fn listen(path: &Path) -> io::Result<UnixListener> {
    let directory = path.parent().expect("the socket is in a directory");
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// The process at the other end of a connection.
fn peer_pid(client: &UnixStream) -> u32 {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let got = unsafe {
        libc::getsockopt(
            client.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if got == 0 {
        credentials.pid as u32
    } else {
        0
    }
}

/// The bus's id: 32 hex digits, another one each time.
fn guid() -> String {
    let mut bytes = [0u8; 16];
    let read = File::open("/dev/urandom").and_then(|mut it| it.read_exact(&mut bytes));
    if read.is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        bytes = (nanos ^ std::process::id() as u128).to_le_bytes();
    }
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
