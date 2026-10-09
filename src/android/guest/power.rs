//! Plasma's battery widget without PowerDevil (`core::power_management`). Plasma's system tray
//! shows the widget only while PowerDevil's service is on the session's bus, so the link takes
//! PowerDevil's names there and answers what the widget asks. Its switch for blocking sleep then
//! keeps Android's screen on (`screen::Job::hold`), and so does any program that blocks sleep or
//! the screen saver (KWin passes those on to PowerDevil).
//!
//! Where PowerDevil has the names already (a rootfs that starts it), the link leaves the widget to
//! it. One that starts later finds them taken: `[battery] share = false` makes way for it.

use super::bus::{self, Bus};
use crate::core::bus::{Service, Signal};
use crate::core::dbus::{self, Message, Writer};
use crate::core::power_management::PowerManagement;
use crate::core::upower::AndroidBattery;
use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

/// Connections that leave the bus, whose inhibitions end with them.
const GONE: &str = "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',\
                    member='NameOwnerChanged',arg2=''";
/// `RequestName`: fail rather than wait in line for a name somebody has.
const DO_NOT_QUEUE: u32 = 4;
const PRIMARY_OWNER: u32 = 1;

/// The bus comes up with the session, which may be after its compositor: how often and how long
/// to look for it.
const TRIES: u32 = 30;
const PAUSE: Duration = Duration::from_secs(2);

pub struct Job {
    connection: Option<Bus>,
    power: PowerManagement,
    battery: Option<AndroidBattery>,
    /// Whether the screen was last told to stay on for the desktop (`held_changed`).
    held: bool,
    retry: Option<Instant>,
    tries: u32,
}

impl Job {
    pub fn new() -> Self {
        Self {
            connection: None,
            power: PowerManagement::new(),
            battery: None,
            held: false,
            retry: None,
            tries: 0,
        }
    }

    /// Turned on or off in the config. Off lets go of the names.
    pub fn turn(&mut self, on: bool) {
        if !on {
            self.disconnected();
        }
    }

    /// The session's compositor is there, so its bus is or will be soon; `battery` as it is.
    pub fn connected(&mut self, battery: Option<&AndroidBattery>) {
        self.battery = battery.cloned();
        self.tries = 0;
        self.serve();
    }

    pub fn disconnected(&mut self) {
        self.connection = None;
        self.retry = None;
        self.power = PowerManagement::new();
    }

    fn serve(&mut self) {
        self.retry = None;
        match take_names(&self.power) {
            Ok(Some(connection)) => {
                log::info!("Power management: stands in for PowerDevil on the session's bus");
                self.power = PowerManagement::new();
                if let Some(battery) = &self.battery {
                    self.power.battery(battery);
                }
                self.connection = Some(connection);
                // Calls that came while the names were being taken.
                self.pump();
            }
            Ok(None) => {
                log::info!("Power management: PowerDevil runs, the battery widget is its own")
            }
            Err(error) => {
                self.tries += 1;
                if self.tries < TRIES {
                    self.retry = Some(Instant::now() + PAUSE);
                } else {
                    log::info!("Power management: no session bus: {error}");
                }
            }
        }
    }

    /// Android's battery changed.
    pub fn battery(&mut self, battery: &AndroidBattery) {
        self.battery = Some(battery.clone());
        let signals = self.power.battery(battery);
        self.emit(&signals);
    }

    /// Whether something on the desktop blocks sleep, if that changed since the last look.
    pub fn held_changed(&mut self) -> Option<bool> {
        let held = self.connection.is_some() && self.power.inhibited();
        if held == self.held {
            return None;
        }
        self.held = held;
        if held {
            let holders: Vec<&str> = self.power.holders().into_iter().map(|it| it.0).collect();
            log::info!(
                "Power management: {} blocks sleep, so the screen stays on",
                holders.join(", ")
            );
        } else {
            log::info!("Power management: nothing blocks sleep");
        }
        Some(held)
    }

    pub fn waits_for(&self, entries: &mut Vec<libc::pollfd>) {
        if let Some(connection) = &self.connection {
            let events = if connection.wants_write() {
                libc::POLLIN | libc::POLLOUT
            } else {
                libc::POLLIN
            };
            entries.push(super::poll_entry(connection.fd(), events));
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.retry
    }

    pub fn expire(&mut self, now: Instant) {
        if self.retry.is_some_and(|at| at <= now) {
            self.serve();
        }
    }

    /// `fd` is ready, if it's the connection's.
    pub fn ready(&mut self, fd: RawFd) {
        if self.connection.as_ref().is_some_and(|it| it.fd() == fd) {
            self.pump();
        }
    }

    /// Send what waits, then answer what came.
    fn pump(&mut self) {
        let Some(connection) = &mut self.connection else {
            return;
        };
        let messages = match connection.flush().and_then(|()| connection.read_ready()) {
            Ok(messages) => messages,
            Err(error) => {
                log::info!("Power management: the session's bus went away ({error})");
                self.disconnected();
                return;
            }
        };
        for message in &messages {
            self.heard(message);
        }
        let signals = self.power.signals();
        self.emit(&signals);
    }

    fn heard(&mut self, message: &Message) {
        let header = &message.header;
        match header.kind {
            dbus::METHOD_CALL => {
                let reply = self.power.call(message);
                if header.flags & dbus::NO_REPLY_EXPECTED != 0 {
                    return;
                }
                let Some(connection) = &mut self.connection else {
                    return;
                };
                let serial = connection.next_serial();
                let bytes = match reply {
                    Ok((signature, body)) => {
                        dbus::method_return(serial, header, connection.name(), &signature, &body)
                    }
                    Err((error, text)) => {
                        dbus::error(serial, header, connection.name(), error, &text)
                    }
                };
                self.send(&bytes);
            }
            dbus::SIGNAL if header.member.as_deref() == Some("NameOwnerChanged") => {
                if let Ok(name) = message.arguments().string() {
                    self.power.gone(&name);
                }
            }
            _ => {}
        }
    }

    fn emit(&mut self, signals: &[Signal]) {
        for signal in signals {
            let Some(connection) = &mut self.connection else {
                return;
            };
            let serial = connection.next_serial();
            let bytes = dbus::signal(
                serial,
                connection.name(),
                None,
                &signal.path,
                &signal.interface,
                &signal.member,
                &signal.signature,
                &signal.body,
            );
            self.send(&bytes);
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let Some(connection) = &mut self.connection else {
            return;
        };
        if let Err(error) = connection.queue(bytes) {
            log::info!("Power management: the session's bus went away ({error})");
            self.disconnected();
        }
    }
}

/// A connection that has PowerDevil's names, `None` if PowerDevil has them.
fn take_names(power: &PowerManagement) -> io::Result<Option<Bus>> {
    let address = bus::session_address()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the session has no bus yet"))?;
    let mut connection = Bus::connect(&address)?;
    let mut rule = Writer::new();
    rule.string(GONE);
    connection.call(
        bus::BUS,
        bus::BUS_PATH,
        bus::BUS,
        "AddMatch",
        "s",
        &rule.into_bytes(),
    )?;
    for name in power.names() {
        let mut body = Writer::new();
        body.string(name).u32(DO_NOT_QUEUE);
        let body = body.into_bytes();
        let reply = connection.call(
            bus::BUS,
            bus::BUS_PATH,
            bus::BUS,
            "RequestName",
            "su",
            &body,
        )?;
        let answer = reply
            .arguments()
            .u32()
            .map_err(|it| io::Error::new(io::ErrorKind::InvalidData, it.0))?;
        if answer != PRIMARY_OWNER {
            return Ok(None);
        }
    }
    connection.set_nonblocking()?;
    Ok(Some(connection))
}
