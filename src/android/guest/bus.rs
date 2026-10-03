//! The desktop session's D-Bus, from outside of it.
//!
//! Its address is in the environment of the session's programs, which are the app's own
//! processes, and the bus takes the app's connections for the session user's: proot says so when
//! the bus asks for their credentials (fake_id0's `SO_PEERCRED`). The wire format is in
//! `core::dbus`.

use crate::core::config::ARCH_FS_ROOT;
use crate::core::dbus::{self, AuthReply, Message};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::os::android::net::SocketAddrExt;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

pub const BUS: &str = "org.freedesktop.DBus";
pub const BUS_PATH: &str = "/org/freedesktop/DBus";

/// How long the bus and the programs on it have to answer a method call.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

fn environment_value(pid: u32, name: &str) -> Option<String> {
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{name}=");
    environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .map(|value| String::from_utf8_lossy(value).into_owned())
}

/// Each process's children, from one pass over /proc (only the app's own processes are there).
fn process_tree() -> HashMap<u32, Vec<u32>> {
    let mut tree: HashMap<u32, Vec<u32>> = HashMap::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|it| it.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // After the command's closing parenthesis: the state, then the parent.
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok());
        if let Some(parent) = parent {
            tree.entry(parent).or_default().push(pid);
        }
    }
    tree
}

/// The session bus of the desktop, whose proot is `pid`: the address in the environment of the
/// first program under it that has one (the session's launcher sets it up).
pub fn address_of(pid: u32) -> Option<String> {
    let tree = process_tree();
    let mut queue = VecDeque::from([pid]);
    while let Some(next) = queue.pop_front() {
        if let Some(address) = environment_value(next, "DBUS_SESSION_BUS_ADDRESS") {
            return Some(address);
        }
        queue.extend(tree.get(&next).into_iter().flatten());
    }
    None
}

/// The current desktop session's bus, if there is a session and it has one.
pub fn session_address() -> Option<String> {
    let pid = crate::android::proot::launch::desktop_pid()?;
    address_of(pid)
}

/// `%xx` escapes in an address's values.
fn unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3).and_then(|it| u8::from_str_radix(it, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Connect to the first of the address's Unix sockets that answers: a path inside the rootfs,
/// or an abstract name, which the app shares with the guest.
fn connect_address(address: &str) -> io::Result<UnixStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no Unix socket in the bus address");
    for transport in address.split(';') {
        let Some(keys) = transport.strip_prefix("unix:") else {
            continue;
        };
        for pair in keys.split(',') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let value = unescape(value);
            let connected = match key {
                "path" => UnixStream::connect(
                    Path::new(ARCH_FS_ROOT).join(value.trim_start_matches('/')),
                ),
                "abstract" => SocketAddr::from_abstract_name(value.as_bytes())
                    .and_then(|socket| UnixStream::connect_addr(&socket)),
                _ => continue,
            };
            match connected {
                Ok(stream) => return Ok(stream),
                Err(error) => last = error,
            }
        }
    }
    Err(last)
}

fn malformed(error: dbus::Malformed) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.0)
}

/// A connection to the session bus.
pub struct Bus {
    stream: UnixStream,
    serial: u32,
    /// What was read and doesn't make a whole message yet.
    buffer: Vec<u8>,
}

impl Bus {
    /// Connect and say hello, waiting for the bus to answer.
    pub fn connect(address: &str) -> io::Result<Self> {
        let stream = connect_address(address)?;
        stream.set_read_timeout(Some(CALL_TIMEOUT))?;
        let mut bus = Self {
            stream,
            serial: 0,
            buffer: Vec::new(),
        };
        bus.authenticate()?;
        bus.call(BUS, BUS_PATH, BUS, "Hello", "", &[])?;
        Ok(bus)
    }

    fn read_line(&mut self) -> io::Result<String> {
        let mut line = Vec::new();
        let mut byte = [0u8];
        while !line.ends_with(b"\n") {
            if self.stream.read(&mut byte)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            line.push(byte[0]);
            if line.len() > 512 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "authentication line too long"));
            }
        }
        Ok(String::from_utf8_lossy(&line).into_owned())
    }

    fn authenticate(&mut self) -> io::Result<()> {
        self.stream.write_all(dbus::AUTH)?;
        let mut reply = dbus::auth_reply(&self.read_line()?);
        if reply == AuthReply::Data {
            self.stream.write_all(dbus::AUTH_DATA)?;
            reply = dbus::auth_reply(&self.read_line()?);
        }
        match reply {
            AuthReply::Ok => self.stream.write_all(dbus::AUTH_BEGIN),
            AuthReply::Data => Err(io::Error::other("the bus wants more than EXTERNAL")),
            AuthReply::Rejected(why) => Err(io::Error::other(why)),
        }
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Send a method call; its serial, for the reply.
    pub fn send(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
        flags: u8,
    ) -> io::Result<u32> {
        self.serial += 1;
        let message = dbus::method_call(
            self.serial,
            destination,
            path,
            interface,
            member,
            signature,
            body,
            flags,
        );
        self.stream.write_all(&message)?;
        Ok(self.serial)
    }

    /// Call a method and wait for its reply. An error reply is an error.
    pub fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> io::Result<Message> {
        let serial = self.send(destination, path, interface, member, signature, body, 0)?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let message = self.read_message(deadline)?;
            if message.header.reply_serial != Some(serial) {
                // Signals meant for every connection (NameAcquired after Hello), say.
                continue;
            }
            if message.header.kind == dbus::ERROR {
                let name = message.header.error_name.clone().unwrap_or_default();
                let text = message.arguments().string().unwrap_or_default();
                return Err(io::Error::other(format!("{name}: {text}")));
            }
            return Ok(message);
        }
    }

    /// The next whole message, waiting for it until `deadline`.
    fn read_message(&mut self, deadline: Instant) -> io::Result<Message> {
        loop {
            if let Some(message) = self.take_message()? {
                return Ok(message);
            }
            if Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn take_message(&mut self) -> io::Result<Option<Message>> {
        let Some(length) = dbus::message_len(&self.buffer).map_err(malformed)? else {
            return Ok(None);
        };
        if self.buffer.len() < length {
            return Ok(None);
        }
        let message = dbus::parse(&self.buffer[..length]).map_err(malformed)?;
        self.buffer.drain(..length);
        Ok(Some(message))
    }

    /// From now on, reading only takes what is there (for a `poll()` loop).
    pub fn set_nonblocking(&self) -> io::Result<()> {
        self.stream.set_nonblocking(true)
    }

    /// The whole messages that have arrived. An error when the bus has gone.
    pub fn read_ready(&mut self) -> io::Result<Vec<Message>> {
        let mut chunk = [0u8; 16384];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        let mut messages = Vec::new();
        while let Some(message) = self.take_message()? {
            messages.push(message);
        }
        Ok(messages)
    }
}
