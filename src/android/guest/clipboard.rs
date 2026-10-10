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
//! Text, HTML and images go across. An image goes as it is if it has a type programs know, and
//! as PNG made of it on request; on Android it is a content URI, which `Clipboard.java` reads, and
//! for the desktop's images `ClipProvider.java` serves.
//!
//! What is copied never goes to the log, only how much of it.

use super::{Globals, State};
use crate::android::clipboard::{AndroidClipboard, Content};
use crate::android::utils::application_context::get_application_context;
use crate::core::clipboard::{self as rules, Kind, Kinds, Step, Sync, Unread};
use std::cell::RefCell;
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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

/// How many of the desktop's images stay for Android's apps to paste.
const IMAGES_KEPT: usize = 5;

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

/// What a program on the desktop gets of Android's clip, which may still be on its way.
struct Payload {
    bytes: Vec<u8>,
    /// All of it has arrived, or as much as will: nothing if it couldn't be read.
    complete: bool,
    /// When the last of it arrived.
    arrived: Instant,
}

impl Payload {
    fn new(bytes: Vec<u8>, complete: bool) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            bytes,
            complete,
            arrived: Instant::now(),
        }))
    }
}

/// Android's clip on offer on the desktop.
struct Offered {
    kinds: Kinds,
    /// Read when a program first asks for its text or HTML.
    content: Option<Content>,
    /// Its image in the types programs asked for, read on the first request for each.
    images: Vec<(&'static str, Rc<RefCell<Payload>>)>,
}

/// Android's image on its way from the thread that `Clipboard.java` reads (and converts) it on.
struct Loading {
    pipe: File,
    payload: Rc<RefCell<Payload>>,
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
    payload: Rc<RefCell<Payload>>,
    written: usize,
    /// When the program last took some.
    progress: Instant,
}

impl Writing {
    fn new(pipe: OwnedFd, payload: Rc<RefCell<Payload>>) -> Self {
        set_nonblocking(&pipe);
        Self {
            pipe: File::from(pipe),
            payload,
            written: 0,
            progress: Instant::now(),
        }
    }

    /// Write what the pipe takes of what has arrived. Whether that was all there is to do.
    fn advance(&mut self) -> bool {
        let payload = self.payload.borrow();
        while self.written < payload.bytes.len() {
            match self.pipe.write(&payload.bytes[self.written..]) {
                Ok(0) => return true,
                Ok(written) => {
                    self.written += written;
                    self.progress = Instant::now();
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return error.kind() != io::ErrorKind::WouldBlock,
            }
        }
        payload.complete
    }

    /// Whether it waits for the program to take more, rather than for more to arrive.
    fn blocked(&self) -> bool {
        self.written < self.payload.borrow().bytes.len()
    }

    /// When the patience with the program ends. Not while it waits for more to arrive: that has
    /// a deadline of its own.
    fn deadline(&self) -> Option<Instant> {
        let payload = self.payload.borrow();
        let waits = self.written == payload.bytes.len() && !payload.complete;
        (!waits).then(|| self.progress.max(payload.arrived) + TRANSFER_TIME)
    }
}

/// What Android's clipboard has, as far as it went through the link.
#[derive(PartialEq)]
enum Copied {
    Text(Content),
    /// An image, by a hash of it.
    Image(u64),
}

pub struct Job {
    android: AndroidClipboard,
    sync: Sync,
    offered: Option<Offered>,
    /// What Android's clipboard has, as far as it went through here.
    on_android: Option<Copied>,
    reading: Vec<Part>,
    reading_deadline: Option<Instant>,
    loading: Vec<Loading>,
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
            loading: Vec::new(),
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
        self.loading.clear();
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
            images: Vec::new(),
        });
        // Whatever it is, it isn't read yet.
        self.on_android = None;
        self.stop_reading();
        log::trace!("Clipboard sharing: offered Android's clip to the desktop");
    }

    /// Act on what the compositor told.
    pub fn dispatched(&mut self, (desktop, queue): Link) {
        for happened in std::mem::take(&mut desktop.happened) {
            match happened {
                Happened::Selected => {
                    // Whatever was being read is no longer the selection.
                    self.stop_reading();
                    let types = desktop.selection.as_ref().map(Offer::types);
                    if super::input() {
                        self.sync.input();
                    }
                    let step = if std::mem::replace(&mut desktop.met, true) {
                        self.sync.desktop_selection(types.as_deref())
                    } else {
                        // Older than what Android has: it stays on the desktop.
                        self.sync.desktop_selection(None::<&[String]>)
                    };
                    match step {
                        Step::CopyToAndroid => self.copy_to_android(Some(&mut *desktop)),
                        Step::OfferAgain => {
                            self.look_at_android(Some((&mut *desktop, queue.clone())))
                        }
                        _ => {}
                    }
                }
                Happened::Asked { mime_type, pipe } => self.answer(desktop, &mime_type, pipe),
                Happened::Withdrawn => self.offered = None,
            }
        }
    }

    /// Ask the desktop's selection for what Android's clipboard gets of it.
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
        for (kind, mime_type) in rules::to_read(&types) {
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
        let parts = std::mem::take(&mut self.reading);
        self.reading_deadline = None;
        // It comes alone, see `rules::to_read`.
        if let Some(image) = parts.iter().find(|it| it.kind == Kind::Image) {
            self.image_to_android(&image.mime_type, &image.bytes);
            return;
        }
        let mut text = String::new();
        let mut html = None;
        for part in &parts {
            let decoded = rules::decode(&part.mime_type, &part.bytes);
            match part.kind {
                Kind::Text => text = decoded,
                Kind::Html if !decoded.is_empty() => html = Some(decoded),
                _ => {}
            }
        }
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
        if matches!(&self.on_android, Some(Copied::Text(it)) if *it == content) {
            return;
        }
        if self.android.write(&content) {
            log::trace!(
                "Clipboard sharing: copied {} bytes to Android",
                content.text.len()
            );
            self.on_android = Some(Copied::Text(content));
            self.saw_own_clip();
        }
    }

    /// Make the desktop's image Android's clip: a file of the app's, which `ClipProvider.java`
    /// lets the apps that paste it read.
    fn image_to_android(&mut self, offered_type: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mime_type = rules::image_type_of(offered_type);
        let copied = Copied::Image(hash(bytes));
        if self.on_android.as_ref() == Some(&copied) {
            return;
        }
        let directory = images_directory();
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |it| it.as_millis());
        let name = format!("image-{millis}.{}", rules::image_extension(mime_type));
        let path = directory.join(&name);
        if let Err(error) = fs::create_dir_all(&directory).and_then(|()| fs::write(&path, bytes)) {
            log::error!(
                "Clipboard sharing: failed to keep the desktop's image for Android: {error}"
            );
            return;
        }
        if !self.android.write_image(&name, mime_type) {
            let _ = fs::remove_file(&path);
            return;
        }
        log::trace!(
            "Clipboard sharing: copied an image of {} bytes to Android",
            bytes.len()
        );
        self.on_android = Some(copied);
        forget_old_images(&directory);
        self.saw_own_clip();
    }

    /// The app's own clip, which the rules only need to have seen.
    fn saw_own_clip(&mut self) {
        if self.sync.focused() {
            let clip = self.android.describe();
            self.sync.android_clip(clip);
        }
    }

    /// A program on the desktop wants Android's clip.
    fn answer(&mut self, desktop: &mut Desktop, mime_type: &str, pipe: OwnedFd) {
        let Some(offered) = &self.offered else {
            return;
        };
        // One of the types offered, and not the link's mark, which says nothing.
        let types = rules::desktop_types(offered.kinds);
        let Some(mime_type) = types.into_iter().find(|it| *it == mime_type) else {
            return;
        };
        let payload = match rules::kind_of(mime_type) {
            Some(Kind::Image) => self.image(desktop, mime_type),
            Some(kind) => self.text(desktop, kind, mime_type).map(|bytes| {
                log::trace!(
                    "Clipboard sharing: a program on the desktop gets {} bytes of Android's clip",
                    bytes.len()
                );
                Payload::new(bytes, true)
            }),
            None => None,
        };
        let Some(payload) = payload else {
            return;
        };
        let mut writing = Writing::new(pipe, payload);
        if !writing.advance() {
            self.writing.push(writing);
        }
    }

    /// Android's text or HTML as `mime_type`, read when a program first asks for either.
    fn text(&mut self, desktop: &mut Desktop, kind: Kind, mime_type: &str) -> Option<Vec<u8>> {
        if self.offered.as_ref()?.content.is_none() {
            // Android only lets the app whose window has focus read its clipboard.
            let read = if self.sync.focused() {
                self.android.read()
            } else {
                Err(Unread::Unfocused)
            };
            match read {
                Ok(content) => {
                    self.on_android = Some(Copied::Text(content.clone()));
                    self.offered.as_mut()?.content = Some(content);
                }
                Err(why) => {
                    self.unread(desktop, why);
                    return None;
                }
            }
        }
        let content = self.offered.as_ref()?.content.as_ref()?;
        match kind {
            Kind::Html => content.html.as_ref().map(|html| html.as_bytes().to_vec()),
            _ => Some(rules::encode(mime_type, &content.text)),
        }
    }

    /// Android's image as `mime_type`. From the first request for that type on, it comes from
    /// `Clipboard.java` through a pipe, and goes on to the programs that asked while it arrives:
    /// making a PNG of a photo takes seconds, and programs give up on a selection whose first
    /// bytes take that long.
    fn image(
        &mut self,
        desktop: &mut Desktop,
        mime_type: &'static str,
    ) -> Option<Rc<RefCell<Payload>>> {
        let offered = self.offered.as_ref()?;
        if let Some((_, payload)) = offered.images.iter().find(|(it, _)| *it == mime_type) {
            log::trace!("Clipboard sharing: a program on the desktop gets Android's image again");
            return Some(Rc::clone(payload));
        }
        if !self.sync.focused() {
            // Android only lets the app whose window has focus read its clipboard.
            self.unread(desktop, Unread::Unfocused);
            return None;
        }
        let (reading, writing) = match pipe() {
            Ok(ends) => ends,
            Err(error) => {
                log::error!("Clipboard sharing: no pipe: {error}");
                return None;
            }
        };
        let started = self.android.read_image(writing.as_fd(), mime_type);
        // Java writes into a copy of its own: this one has to go, or the reading end never
        // sees the end.
        drop(writing);
        if let Err(why) = started {
            self.unread(desktop, why);
            return None;
        }
        log::trace!("Clipboard sharing: reading Android's image as {mime_type}");
        let payload = Payload::new(Vec::new(), false);
        self.offered
            .as_mut()?
            .images
            .push((mime_type, Rc::clone(&payload)));
        self.loading.push(Loading {
            pipe: reading,
            payload: Rc::clone(&payload),
        });
        Some(payload)
    }

    /// Android's clip couldn't be read for a program on the desktop: give up on it if that lasts.
    fn unread(&mut self, desktop: &mut Desktop, why: Unread) {
        if why.lasting() {
            // Requests already on their way find no offer: one line per clip, however many
            // types and programs ask.
            log::info!("Clipboard sharing: took Android's clip off the desktop, {why}");
            desktop.withdraw();
            self.offered = None;
        } else {
            log::trace!("Clipboard sharing: Android's clip can't be read now, {why}");
        }
    }

    /// The pipes that are being read from and written to.
    pub fn waits_for(&self, entries: &mut Vec<libc::pollfd>) {
        let reading = self.reading.iter().filter(|it| !it.done).map(|it| (&it.pipe, libc::POLLIN));
        let loading = self.loading.iter().map(|it| (&it.pipe, libc::POLLIN));
        // The others wait for more of an image to arrive.
        let writing = self
            .writing
            .iter()
            .filter(|it| it.blocked())
            .map(|it| (&it.pipe, libc::POLLOUT));
        for (pipe, events) in reading.chain(loading).chain(writing) {
            entries.push(super::poll_entry(pipe.as_raw_fd(), events));
        }
    }

    /// When the patience with them ends.
    pub fn deadline(&self) -> Option<Instant> {
        let loading = self
            .loading
            .iter()
            .map(|it| it.payload.borrow().arrived + TRANSFER_TIME);
        self.writing
            .iter()
            .filter_map(Writing::deadline)
            .chain(loading)
            .chain(self.reading_deadline)
            .min()
    }

    /// One of the pipes can be read from or written to, or its other end is closed.
    pub fn ready(&mut self, pipe: RawFd) {
        if self.load(pipe) {
            // More of an image for the programs that wait for it.
            self.writing.retain_mut(|it| !it.advance());
            return;
        }
        self.writing
            .retain_mut(|it| it.pipe.as_raw_fd() != pipe || !it.advance());

        let Some(part) = self
            .reading
            .iter_mut()
            .find(|it| !it.done && it.pipe.as_raw_fd() == pipe)
        else {
            return;
        };
        let limit = match part.kind {
            Kind::Image => rules::IMAGE_LIMIT,
            _ => rules::DESKTOP_READ_LIMIT,
        };
        let mut buffer = [0u8; 16384];
        loop {
            match part.pipe.read(&mut buffer) {
                Ok(0) => part.done = true,
                Ok(read) => {
                    part.bytes.extend_from_slice(&buffer[..read]);
                    if part.bytes.len() <= limit {
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

    /// Take what arrived of Android's image through `pipe`. Whether it is one of those.
    fn load(&mut self, pipe: RawFd) -> bool {
        let Some(index) = self
            .loading
            .iter()
            .position(|it| it.pipe.as_raw_fd() == pipe)
        else {
            return false;
        };
        let loading = &mut self.loading[index];
        let mut payload = loading.payload.borrow_mut();
        let mut buffer = [0u8; 65536];
        // A part at a time, for the programs to get theirs meanwhile.
        for _ in 0..16 {
            match loading.pipe.read(&mut buffer) {
                Ok(0) => {
                    payload.complete = true;
                    if payload.bytes.is_empty() {
                        log::info!("Clipboard sharing: Android's image couldn't be read");
                    } else {
                        log::trace!(
                            "Clipboard sharing: Android's image arrived, {} bytes",
                            payload.bytes.len()
                        );
                    }
                }
                Ok(read) => {
                    payload.bytes.extend_from_slice(&buffer[..read]);
                    payload.arrived = Instant::now();
                    if payload.bytes.len() <= rules::IMAGE_LIMIT {
                        continue;
                    }
                    log::info!("Clipboard sharing: Android's image is too large for the desktop");
                    payload.bytes = Vec::new();
                    payload.complete = true;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => payload.complete = true,
            }
            break;
        }
        let complete = payload.complete;
        drop(payload);
        if complete {
            self.loading.swap_remove(index);
        }
        true
    }

    /// Give up on the programs that took too long, and on what nobody waits for any more.
    pub fn expire(&mut self, now: Instant) {
        // Android's clip was replaced, and the programs that asked for its image have left.
        self.loading.retain(|it| Rc::strong_count(&it.payload) > 1);
        let loading = self.loading.len();
        self.loading.retain(|it| {
            let mut payload = it.payload.borrow_mut();
            if payload.arrived + TRANSFER_TIME > now {
                return true;
            }
            log::info!("Clipboard sharing: Android's image didn't arrive");
            payload.complete = true;
            false
        });
        if self.loading.len() < loading {
            // What there is of it for the programs that waited.
            self.writing.retain_mut(|it| !it.advance());
        }
        self.writing
            .retain(|it| it.deadline().map_or(true, |at| at > now));
        if self.reading_deadline.is_some_and(|at| at <= now) {
            log::info!("Clipboard sharing: the desktop's selection didn't arrive");
            self.stop_reading();
        }
    }
}

/// Where the desktop's images wait for Android's apps to paste them (`ClipProvider.java`).
fn images_directory() -> PathBuf {
    get_application_context().data_dir.join("clipboard")
}

/// Delete all but the newest few images: Android's clipboard history (Samsung's keyboard has
/// one) can paste the older ones too.
fn forget_old_images(directory: &Path) {
    let mut names: Vec<_> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with("image-"))
        .collect();
    // By the time in their names.
    names.sort();
    for name in &names[..names.len().saturating_sub(IMAGES_KEPT)] {
        let _ = fs::remove_file(directory.join(name));
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
