//! Let the screen turn off while nobody uses the desktop.
//!
//! The app keeps the screen on while it's in front (`keep_screen_on` at start), since a video or
//! a long build gets no touches that would hold off Android's timeout; but that also kept it on
//! with the phone lying unused. The session's compositor knows better: `ext-idle-notify` tells
//! when nobody has used the desktop for a while, and holds back while something inhibits idling,
//! such as a video player. Then the app lets Android's own timeout apply, which has run out by
//! then as well: what reaches the desktop is Android's input too.

use super::{Globals, State};
use crate::android::screen::AndroidScreen;
use std::path::Path;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
use winit::platform::android::activity::AndroidApp;

/// The shortest idle time to wait for, whatever Android's timeout says.
const MIN_TIMEOUT_MS: u32 = 10_000;

/// The compositor's side: a notification after the screen's timeout without use.
pub struct Desktop {
    _notifier: ExtIdleNotifierV1,
    _notification: ExtIdleNotificationV1,
    /// What the compositor said last and the job hasn't acted on: idle or in use again.
    idle: Option<bool>,
}

impl Desktop {
    /// `None` if the compositor doesn't tell its clients when it's idle.
    pub fn bind(
        globals: &Globals,
        seat: &WlSeat,
        queue: &QueueHandle<State>,
        timeout_ms: u32,
    ) -> Option<Self> {
        let notifier: ExtIdleNotifierV1 = globals.bind(queue, 1)?;
        // Version 1's notification, which waits while something inhibits idling.
        let notification = notifier.get_idle_notification(timeout_ms, seat, queue, ());
        Some(Self {
            _notifier: notifier,
            _notification: notification,
            idle: None,
        })
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(desktop) = state.screen.as_mut() else {
            return;
        };
        match event {
            ext_idle_notification_v1::Event::Idled => desktop.idle = Some(true),
            ext_idle_notification_v1::Event::Resumed => desktop.idle = Some(false),
            _ => {}
        }
    }
}

pub struct Job {
    android: AndroidScreen,
    /// Whether the app keeps the screen on now; it does from the start.
    kept_on: bool,
}

impl Job {
    /// For the calling thread. `None` if the app's window can't be reached.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        Some(Self {
            android: AndroidScreen::new(android_app)?,
            kept_on: true,
        })
    }

    /// Turned on or off in the config. Off keeps the screen on, as the app did before.
    pub fn turn(&mut self, on: bool) {
        if !on {
            self.keep_on(true);
        }
    }

    /// How long nobody has to use the desktop for the screen to be let go: Android's timeout.
    pub fn timeout_ms(&self) -> u32 {
        self.android
            .timeout_ms()
            .unwrap_or(60_000)
            .max(MIN_TIMEOUT_MS)
    }

    pub fn connected(&mut self, socket: &Path, timeout_ms: u32) {
        log::info!(
            "Screen: lets Android turn it off after {} s without use of the desktop at {}",
            timeout_ms / 1000,
            socket.display()
        );
    }

    /// Without a desktop to ask, the screen stays on, as it did before.
    pub fn disconnected(&mut self) {
        self.keep_on(true);
    }

    /// Act on what the compositor said.
    pub fn dispatched(&mut self, desktop: &mut Desktop) {
        let Some(idle) = desktop.idle.take() else {
            return;
        };
        log::info!(
            "Screen: the desktop is {}",
            if idle { "idle" } else { "in use" }
        );
        self.keep_on(!idle);
    }

    /// A new activity took the app over; its window starts kept on (`keep_screen_on`).
    pub fn new_window(&mut self) {
        if !self.kept_on {
            self.android.keep_on(false);
        }
    }

    fn keep_on(&mut self, on: bool) {
        if self.kept_on != on {
            self.kept_on = on;
            self.android.keep_on(on);
        }
    }
}
