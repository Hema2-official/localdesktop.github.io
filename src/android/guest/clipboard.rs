//! One clipboard for Android and the desktop (`[clipboard] sync`).
//!
//! On the desktop the link is a clipboard manager like Klipper: the session's compositor tells
//! it every selection and takes selections from it (ext-data-control, or the wlroots protocol
//! that one was made from). When to copy which way is decided by `core::clipboard::Sync`.
//!
//! Nothing is copied before someone needs it. Android's clip goes to the desktop as an offer,
//! and is read when a program there asks for it (Klipper does right away, for its history).
//! If Android can't give it then, the offer is withdrawn. The desktop's selection is read when
//! the app's window loses focus.
//!
//! What is copied never goes to the log, only how much of it.

use super::{Globals, State};
use crate::android::clipboard::{AndroidClipboard, Content};
use crate::core::clipboard::{self as rules, Kind, Kinds, Step, Sync, Unread};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use winit::platform::android::activity::AndroidApp;

/// How long a program on the desktop gets to hand over its selection, or to take Android's.
const TRANSFER_TIME: Duration = Duration::from_secs(10);

/// The types an offer has announced.
#[derive(Default)]
pub struct OfferTypes(Mutex<Vec<String>>);

/// The same objects in both protocols.
enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

#[derive(PartialEq)]
enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl Manager {
    fn device(&self, seat: &WlSeat, queue: &QueueHandle<State>) -> Device {
        match self {
            Self::Ext(manager) => Device::Ext(manager.get_data_device(seat, queue, ())),
            Self::Wlr(manager) => Device::Wlr(manager.get_data_device(seat, queue, ())),
        }
    }

    fn source(&self, queue: &QueueHandle<State>) -> Source {
        match self {
            Self::Ext(manager) => Source::Ext(manager.create_data_source(queue, ())),
            Self::Wlr(manager) => Source::Wlr(manager.create_data_source(queue, ())),
        }
    }
}

impl Device {
    fn select(&self, source: &Source) {
        match (self, source) {
            (Self::Ext(device), Source::Ext(source)) => device.set_selection(Some(source)),
            (Self::Wlr(device), Source::Wlr(source)) => device.set_selection(Some(source)),
            _ => {}
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(device) => device.destroy(),
            Self::Wlr(device) => device.destroy(),
        }
    }
}

impl Offer {
    fn types(&self) -> Vec<String> {
        let types = match self {
            Self::Ext(offer) => offer.data::<OfferTypes>(),
            Self::Wlr(offer) => offer.data::<OfferTypes>(),
        };
        types.map_or_else(Vec::new, |it| it.0.lock().unwrap().clone())
    }

    /// Have the selection written to `pipe` as `mime_type`.
    fn receive(&self, mime_type: &str, pipe: BorrowedFd) {
        match self {
            Self::Ext(offer) => offer.receive(mime_type.to_owned(), pipe),
            Self::Wlr(offer) => offer.receive(mime_type.to_owned(), pipe),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

impl Source {
    fn offer(&self, mime_type: &str) {
        match self {
            Self::Ext(source) => source.offer(mime_type.to_owned()),
            Self::Wlr(source) => source.offer(mime_type.to_owned()),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }
}

/// What the compositor told, for `Job::dispatched` to act on.
enum Happened {
    /// The desktop has another selection, or none.
    Selected,
    /// A program wants the link's selection.
    Asked { mime_type: String, pipe: OwnedFd },
    /// The link's selection was replaced.
    Withdrawn,
}

/// The clipboard on the desktop's side.
pub struct Desktop {
    manager: Manager,
    device: Option<Device>,
    /// The desktop's selection.
    selection: Option<Offer>,
    /// The link's own selection there: Android's clip.
    source: Option<Source>,
    /// The selection the compositor tells first is the one it had already.
    met: bool,
    happened: Vec<Happened>,
}

/// The desktop's side and the queue its new objects belong to.
pub type Link<'a> = (&'a mut Desktop, QueueHandle<State>);

impl Desktop {
    /// `None` if the compositor doesn't let clients manage the clipboard.
    pub fn bind(globals: &Globals, seat: &WlSeat, queue: &QueueHandle<State>) -> Option<Self> {
        let manager = globals
            .bind::<ExtDataControlManagerV1>(queue, 1)
            .map(Manager::Ext)
            .or_else(|| {
                globals
                    .bind::<ZwlrDataControlManagerV1>(queue, 2)
                    .map(Manager::Wlr)
            })?;
        let device = manager.device(seat, queue);
        Some(Self {
            manager,
            device: Some(device),
            selection: None,
            source: None,
            met: false,
            happened: Vec::new(),
        })
    }

    fn selected(&mut self, offer: Option<Offer>) {
        if let Some(replaced) = std::mem::replace(&mut self.selection, offer) {
            replaced.destroy();
        }
        self.happened.push(Happened::Selected);
    }

    fn asked(&mut self, source: Source, mime_type: String, pipe: OwnedFd) {
        if self.source.as_ref() == Some(&source) {
            self.happened.push(Happened::Asked { mime_type, pipe });
        }
    }

    fn cancelled(&mut self, source: Source) {
        source.destroy();
        if self.source.as_ref() == Some(&source) {
            self.source = None;
            self.happened.push(Happened::Withdrawn);
        }
    }

    /// Take the link's selection off the desktop. Destroying the source leaves the selection
    /// alone if another program has made one meanwhile, as unsetting it wouldn't.
    fn withdraw(&mut self) {
        if let Some(source) = self.source.take() {
            source.destroy();
        }
    }

    /// The seat is gone, and the device with it.
    fn finished(&mut self) {
        if let Some(device) = self.device.take() {
            device.destroy();
        }
    }
}

macro_rules! data_control {
    ($protocol:ident, $Manager:ty,
     $device:ident :: $Device:ty, $offer:ident :: $Offer:ty, $source:ident :: $Source:ty) => {
        impl Dispatch<$Manager, ()> for State {
            fn event(
                _: &mut Self,
                _: &$Manager,
                _: <$Manager as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<$Device, ()> for State {
            fn event(
                state: &mut Self,
                _: &$Device,
                event: $device::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                let Some(desktop) = &mut state.clipboard else {
                    return;
                };
                match event {
                    $device::Event::Selection { id } => {
                        desktop.selected(id.map(Offer::$protocol))
                    }
                    // Not shared: Android has no such thing.
                    $device::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
                    $device::Event::Finished => desktop.finished(),
                    _ => {}
                }
            }

            event_created_child!(State, $Device, [
                $device::EVT_DATA_OFFER_OPCODE => ($Offer, OfferTypes::default()),
            ]);
        }

        impl Dispatch<$Offer, OfferTypes> for State {
            fn event(
                _: &mut Self,
                _: &$Offer,
                event: $offer::Event,
                types: &OfferTypes,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let $offer::Event::Offer { mime_type } = event {
                    types.0.lock().unwrap().push(mime_type);
                }
            }
        }

        impl Dispatch<$Source, ()> for State {
            fn event(
                state: &mut Self,
                source: &$Source,
                event: $source::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                let Some(desktop) = &mut state.clipboard else {
                    return;
                };
                let source = Source::$protocol(source.clone());
                match event {
                    $source::Event::Send { mime_type, fd } => {
                        desktop.asked(source, mime_type, fd)
                    }
                    $source::Event::Cancelled => desktop.cancelled(source),
                    _ => {}
                }
            }
        }
    };
}

data_control!(
    Ext,
    ExtDataControlManagerV1,
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1
);
data_control!(
    Wlr,
    ZwlrDataControlManagerV1,
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1
);

fn set_nonblocking(pipe: &impl AsRawFd) {
    unsafe {
        let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

/// A pipe whose reading end doesn't block. The writing end is for another program, which
/// expects it to.
fn pipe() -> io::Result<(File, OwnedFd)> {
    let mut ends = [0; 2];
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let (reading, writing) = unsafe { (File::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
    set_nonblocking(&reading);
    Ok((reading, writing))
}

/// Android's clip on offer on the desktop.
struct Offered {
    kinds: Kinds,
    /// Read when a program first asks for it.
    content: Option<Content>,
}

/// One type of the desktop's selection on its way to Android.
struct Part {
    kind: Kind,
    mime_type: String,
    pipe: File,
    bytes: Vec<u8>,
    done: bool,
}

/// Android's clip on its way to a program on the desktop.
struct Writing {
    pipe: File,
    bytes: Vec<u8>,
    written: usize,
    deadline: Instant,
}

impl Writing {
    /// Write what the pipe takes. Whether that was all there is to do.
    fn advance(&mut self) -> bool {
        while self.written < self.bytes.len() {
            match self.pipe.write(&self.bytes[self.written..]) {
                Ok(0) => return true,
                Ok(written) => self.written += written,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return error.kind() != io::ErrorKind::WouldBlock,
            }
        }
        true
    }
}

pub struct Job {
    android: AndroidClipboard,
    sync: Sync,
    offered: Option<Offered>,
    /// What Android's clipboard has, as far as it went through here.
    on_android: Option<Content>,
    reading: Vec<Part>,
    reading_deadline: Option<Instant>,
    writing: Vec<Writing>,
}

impl Job {
    /// For the calling thread. `None` if Android's clipboard can't be reached.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        let android = AndroidClipboard::new(android_app)?;
        Some(Self {
            android,
            sync: Sync::new(super::focused()),
            offered: None,
            on_android: None,
            reading: Vec::new(),
            reading_deadline: None,
            writing: Vec::new(),
        })
    }

    /// Sharing was turned on or off in the config.
    pub fn turn(&mut self, on: bool) {
        self.android.watch(on);
    }

    /// The session's compositor is there: a new session gets what Android has.
    pub fn connected(&mut self, desktop: Link) {
        self.sync.forget_android_clip();
        self.look_at_android(Some(desktop));
    }

    pub fn disconnected(&mut self) {
        self.sync.desktop_gone();
        self.offered = None;
        self.stop_reading();
        self.writing.clear();
    }

    /// The app's window got or lost focus.
    pub fn focus(&mut self, focused: bool, mut desktop: Option<Link>) {
        if self.sync.focus(focused) == Step::CopyToAndroid {
            self.copy_to_android(desktop.as_mut().map(|(desktop, _)| &mut **desktop));
        }
        if focused {
            self.look_at_android(desktop);
        }
    }

    /// Android says its clipboard changed.
    pub fn android_changed(&mut self, desktop: Option<Link>) {
        self.look_at_android(desktop);
    }

    fn look_at_android(&mut self, desktop: Option<Link>) {
        // Without focus Android wouldn't tell, and with a session yet to come there's nobody
        // to offer it to: `connected` looks again.
        let (true, Some(desktop)) = (self.sync.focused(), desktop) else {
            return;
        };
        let clip = self.android.describe();
        log::trace!("Clipboard sharing: Android has {clip:?}");
        if let Step::OfferToDesktop(kinds) = self.sync.android_clip(clip) {
            self.offer(kinds, desktop);
        }
    }

    /// Make Android's clip the desktop's selection.
    fn offer(&mut self, kinds: Kinds, (desktop, queue): Link) {
        let Some(device) = &desktop.device else {
            return;
        };
        let source = desktop.manager.source(&queue);
        for mime_type in rules::desktop_types(kinds) {
            source.offer(mime_type);
        }
        device.select(&source);
        // The compositor cancels the one it replaces, see `Desktop::cancelled`.
        desktop.source = Some(source);
        self.offered = Some(Offered {
            kinds,
            content: None,
        });
        self.stop_reading();
        log::trace!("Clipboard sharing: offered Android's clip to the desktop");
    }

    /// Act on what the compositor told.
    pub fn dispatched(&mut self, (desktop, _): Link) {
        for happened in std::mem::take(&mut desktop.happened) {
            match happened {
                Happened::Selected => {
                    // Whatever was being read is no longer the selection.
                    self.stop_reading();
                    let types = desktop.selection.as_ref().map(Offer::types);
                    let step = if std::mem::replace(&mut desktop.met, true) {
                        self.sync.desktop_selection(types.as_deref())
                    } else {
                        // Older than what Android has: it stays on the desktop.
                        self.sync.desktop_selection(None::<&[String]>)
                    };
                    if step == Step::CopyToAndroid {
                        self.copy_to_android(Some(&mut *desktop));
                    }
                }
                Happened::Asked { mime_type, pipe } => self.answer(desktop, &mime_type, pipe),
                Happened::Withdrawn => self.offered = None,
            }
        }
    }

    /// Ask the desktop's selection for its text and HTML.
    fn copy_to_android(&mut self, desktop: Option<&mut Desktop>) {
        self.stop_reading();
        let Some(desktop) = desktop else {
            return;
        };
        let Some(selection) = &desktop.selection else {
            return;
        };
        let types = selection.types();
        if rules::is_from_link(&types) {
            return;
        }
        let wanted = [
            (Kind::Text, rules::text_type(&types)),
            (Kind::Html, rules::html_type(&types)),
        ];
        for (kind, mime_type) in wanted {
            let Some(mime_type) = mime_type else {
                continue;
            };
            match pipe() {
                Ok((reading, writing)) => {
                    // The request takes a copy of the writing end; this one has to go, or
                    // the reading end never sees the end.
                    selection.receive(mime_type, writing.as_fd());
                    self.reading.push(Part {
                        kind,
                        mime_type: mime_type.to_owned(),
                        pipe: reading,
                        bytes: Vec::new(),
                        done: false,
                    });
                }
                Err(error) => log::error!("Clipboard sharing: no pipe: {error}"),
            }
        }
        if !self.reading.is_empty() {
            self.reading_deadline = Some(Instant::now() + TRANSFER_TIME);
        }
    }

    fn stop_reading(&mut self) {
        self.reading.clear();
        self.reading_deadline = None;
    }

    /// The desktop's selection is read: make it Android's clip.
    fn finish_reading(&mut self) {
        let mut text = String::new();
        let mut html = None;
        for part in &self.reading {
            let decoded = rules::decode(&part.mime_type, &part.bytes);
            match part.kind {
                Kind::Text => text = decoded,
                Kind::Html if !decoded.is_empty() => html = Some(decoded),
                Kind::Html => {}
            }
        }
        self.stop_reading();
        if text.is_empty() && html.is_none() {
            return;
        }
        if !rules::fits_android(&text, html.as_deref()) {
            log::info!(
                "Clipboard sharing: {} bytes are too much for Android's clipboard",
                text.len() + html.map_or(0, |it| it.len())
            );
            return;
        }
        let content = Content { text, html };
        // A clipboard manager taking over what came from Android, or the same copied again.
        if self.on_android.as_ref() == Some(&content) {
            return;
        }
        if self.android.write(&content) {
            log::trace!(
                "Clipboard sharing: copied {} bytes to Android",
                content.text.len()
            );
            self.on_android = Some(content);
            if self.sync.focused() {
                // Its own clip, which the rules only need to have seen.
                let clip = self.android.describe();
                self.sync.android_clip(clip);
            }
        }
    }

    /// A program on the desktop wants Android's clip.
    fn answer(&mut self, desktop: &mut Desktop, mime_type: &str, pipe: OwnedFd) {
        let Some(offered) = &mut self.offered else {
            return;
        };
        if offered.content.is_none() {
            // Android only lets the app whose window has focus read its clipboard.
            let read = if self.sync.focused() {
                self.android.read()
            } else {
                Err(Unread::Unfocused)
            };
            match read {
                Ok(content) => {
                    self.on_android = Some(content.clone());
                    offered.content = Some(content);
                }
                Err(why) if why.lasting() => {
                    // Requests already on their way find no offer: one line per clip, however
                    // many types and programs ask.
                    log::info!("Clipboard sharing: took Android's clip off the desktop, {why}");
                    desktop.withdraw();
                    self.offered = None;
                    return;
                }
                Err(why) => {
                    log::trace!("Clipboard sharing: Android's clip can't be read now, {why}");
                    return;
                }
            }
        }
        let Some(content) = &offered.content else {
            return;
        };
        let bytes = match rules::kind_of(mime_type) {
            Some(Kind::Text) => rules::encode(mime_type, &content.text),
            Some(Kind::Html) if offered.kinds.html => match &content.html {
                Some(html) => html.as_bytes().to_vec(),
                None => return,
            },
            // The link's mark says nothing.
            _ => return,
        };
        log::trace!(
            "Clipboard sharing: a program on the desktop gets {} bytes of Android's clip",
            bytes.len()
        );
        set_nonblocking(&pipe);
        let mut writing = Writing {
            pipe: File::from(pipe),
            bytes,
            written: 0,
            deadline: Instant::now() + TRANSFER_TIME,
        };
        if !writing.advance() {
            self.writing.push(writing);
        }
    }

    /// The pipes that are being read from and written to.
    pub fn waits_for(&self, entries: &mut Vec<libc::pollfd>) {
        let reading = self.reading.iter().filter(|it| !it.done).map(|it| (&it.pipe, libc::POLLIN));
        let writing = self.writing.iter().map(|it| (&it.pipe, libc::POLLOUT));
        for (pipe, events) in reading.chain(writing) {
            entries.push(super::poll_entry(pipe.as_raw_fd(), events));
        }
    }

    /// When the patience with them ends.
    pub fn deadline(&self) -> Option<Instant> {
        self.writing
            .iter()
            .map(|it| it.deadline)
            .chain(self.reading_deadline)
            .min()
    }

    /// One of the pipes can be read from or written to, or its other end is closed.
    pub fn ready(&mut self, pipe: RawFd) {
        self.writing
            .retain_mut(|it| it.pipe.as_raw_fd() != pipe || !it.advance());

        let Some(part) = self
            .reading
            .iter_mut()
            .find(|it| !it.done && it.pipe.as_raw_fd() == pipe)
        else {
            return;
        };
        let mut buffer = [0u8; 16384];
        loop {
            match part.pipe.read(&mut buffer) {
                Ok(0) => part.done = true,
                Ok(read) => {
                    part.bytes.extend_from_slice(&buffer[..read]);
                    if part.bytes.len() <= rules::DESKTOP_READ_LIMIT {
                        continue;
                    }
                    log::info!("Clipboard sharing: the selection is too much for Android's clipboard");
                    self.stop_reading();
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => part.done = true,
            }
            break;
        }
        if self.reading.iter().all(|it| it.done) {
            self.finish_reading();
        }
    }

    /// Give up on the programs that took too long.
    pub fn expire(&mut self, now: Instant) {
        self.writing.retain(|it| it.deadline > now);
        if self.reading_deadline.is_some_and(|at| at <= now) {
            log::info!("Clipboard sharing: the desktop's selection didn't arrive");
            self.stop_reading();
        }
    }
}
