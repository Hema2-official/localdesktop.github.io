//! The desktop's notifications on Android, while the app isn't in front.
//!
//! Desktop programs send their notifications over the session's bus to its notification server
//! (plasmashell, xfce4-notifyd), which shows them on the desktop. The link watches them go by
//! with a connection that is a monitor (`BecomeMonitor`, which the bus allows the session's
//! user), and shows them on Android too while nobody is looking at the desktop: a build that
//! finished while the phone was in a pocket. Once the app is back in front the desktop has them
//! in its own history, so they come off Android.

use super::bus::{self, Bus};
use crate::android::notifications::AndroidNotifications;
use crate::core::dbus::{self, Message, Writer};
use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};
use winit::platform::android::activity::AndroidApp;

/// What the monitor gets: the notifications programs send.
const RULE: &str = "type='method_call',interface='org.freedesktop.Notifications',member='Notify'";

/// The bus comes up with the session, which may be after its compositor: how often and how long
/// to look for it.
const WATCH_TRIES: u32 = 30;
const WATCH_PAUSE: Duration = Duration::from_secs(2);

pub struct Job {
    android: AndroidNotifications,
    monitor: Option<Bus>,
    retry: Option<Instant>,
    tries: u32,
    /// Some went to Android since the app was last in front.
    shown: bool,
}

impl Job {
    /// For the calling thread. `None` if Android's notifications can't be reached.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        Some(Self {
            android: AndroidNotifications::new(android_app)?,
            monitor: None,
            retry: None,
            tries: 0,
            shown: false,
        })
    }

    /// The session's compositor is there, so its bus is (or will be soon).
    pub fn connected(&mut self) {
        self.tries = 0;
        self.watch();
    }

    pub fn disconnected(&mut self) {
        self.monitor = None;
        self.retry = None;
    }

    fn watch(&mut self) {
        self.retry = None;
        match become_monitor() {
            Ok(monitor) => {
                log::info!("Desktop notifications: watching the session's bus");
                self.monitor = Some(monitor);
            }
            Err(error) => {
                self.tries += 1;
                if self.tries < WATCH_TRIES {
                    self.retry = Some(Instant::now() + WATCH_PAUSE);
                } else {
                    log::info!("Desktop notifications: no session bus to watch: {error}");
                }
            }
        }
    }

    /// The app's window got or lost focus.
    pub fn focus(&mut self, focused: bool) {
        if focused && self.shown {
            self.shown = false;
            self.android.cancel_all();
        }
    }

    pub fn waits_for(&self, entries: &mut Vec<libc::pollfd>) {
        if let Some(monitor) = &self.monitor {
            entries.push(super::poll_entry(monitor.fd(), libc::POLLIN));
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.retry
    }

    /// `fd` has something to read, if it's the monitor's.
    pub fn ready(&mut self, fd: RawFd) {
        let Some(monitor) = self.monitor.as_mut().filter(|it| it.fd() == fd) else {
            return;
        };
        match monitor.read_ready() {
            Ok(messages) => {
                for message in messages {
                    self.heard(&message);
                }
            }
            Err(error) => {
                log::info!("Desktop notifications: the session's bus went away ({error})");
                self.monitor = None;
            }
        }
    }

    pub fn expire(&mut self, now: Instant) {
        if self.retry.is_some_and(|at| at <= now) {
            self.watch();
        }
    }

    fn heard(&mut self, message: &Message) {
        if message.header.kind != dbus::METHOD_CALL
            || message.header.member.as_deref() != Some("Notify")
            || super::focused()
        {
            return;
        }
        // Notify(app_name s, replaces_id u, app_icon s, summary s, body s, actions as, hints
        // a{sv}, expire_timeout i): the first five tell enough.
        let mut arguments = message.arguments();
        let read = (|| -> Result<_, dbus::Malformed> {
            let app = arguments.string()?;
            arguments.u32()?;
            arguments.string()?;
            let summary = arguments.string()?;
            let body = arguments.string()?;
            Ok((app, summary, body))
        })();
        let Ok((app, summary, body)) = read else {
            return;
        };
        log::debug!("Desktop notifications: one from {app}");
        self.android
            .post(&app, &dbus::plain_text(&summary), &dbus::plain_text(&body));
        self.shown = true;
    }
}

/// A connection that sees every notification sent on the session's bus.
fn become_monitor() -> io::Result<Bus> {
    let address = bus::session_address()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the session has no bus yet"))?;
    let mut monitor = Bus::connect(&address)?;
    let mut rules = Writer::new();
    rules.strings(&[RULE]).u32(0);
    monitor.call(
        bus::BUS,
        bus::BUS_PATH,
        "org.freedesktop.DBus.Monitoring",
        "BecomeMonitor",
        "asu",
        &rules.into_bytes(),
    )?;
    monitor.set_nonblocking()?;
    Ok(monitor)
}
