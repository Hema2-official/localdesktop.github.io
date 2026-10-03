//! What the app does inside the desktop session, from outside of it.
//!
//! Under proot the session's programs are the app's own children, and their sockets are files
//! in the rootfs, so the app connects to the session's compositor as one more Wayland client.
//! That takes no process in the guest: nothing to install or start there, nothing more for
//! Android's phantom process killer to count, no system calls through proot.
//!
//! One thread, the link, does all of it and sleeps in `poll()` until something happens: the
//! compositor has an event, its socket appears, or another thread has news (`notify`). Each job
//! has a module of its own with its Wayland event handlers, and leaves the link alone while it
//! has nothing to do.

pub mod clipboard;

use crate::android::utils::application_context::get_application_context;
use crate::core::config::ARCH_FS_ROOT;
use std::collections::VecDeque;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use wayland_client::backend::{ReadEventsGuard, WaylandError};
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use winit::platform::android::activity::AndroidApp;

/// A socket that was just created may not be listening yet: how often and how long to ask.
const CONNECT_TRIES: u32 = 8;
const CONNECT_PAUSE: Duration = Duration::from_millis(250);

/// What other threads tell the link.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    /// The app's window got or lost focus.
    Focus(bool),
    /// Android's clipboard has something new.
    AndroidClipboard,
    /// A desktop session is starting, maybe with another config.
    SessionStarting,
}

struct Shared {
    /// An eventfd, to wake the link with.
    wake: OwnedFd,
    events: Mutex<VecDeque<Event>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
static FOCUSED: AtomicBool = AtomicBool::new(false);
static INPUT: AtomicBool = AtomicBool::new(false);

/// Whether the app's window has focus.
fn focused() -> bool {
    FOCUSED.load(Ordering::Acquire)
}

/// The user pressed a key, touched or clicked on the desktop. Cheap enough for each of those: no
/// system call and no wake-up, the link only looks when the desktop's clipboard changes.
pub fn user_input() {
    if !INPUT.load(Ordering::Relaxed) {
        INPUT.store(true, Ordering::Relaxed);
    }
}

/// Whether the user has pressed a key, touched or clicked on the desktop since the link
/// connected to it.
fn input() -> bool {
    INPUT.load(Ordering::Relaxed)
}

/// Tell the link, from any thread. Doesn't wait for anything.
pub fn notify(event: Event) {
    if let Event::Focus(focused) = event {
        FOCUSED.store(focused, Ordering::Release);
    }
    let Some(shared) = SHARED.get() else {
        return;
    };
    shared.events.lock().unwrap().push_back(event);
    let one = 1u64.to_ne_bytes();
    unsafe { libc::write(shared.wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
}

/// Start the link, once.
pub fn start(android_app: &AndroidApp) {
    if SHARED.get().is_some() {
        return;
    }
    let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wake < 0 {
        log::error!("Failed to start the guest link: {}", io::Error::last_os_error());
        return;
    }
    let shared = Shared {
        wake: unsafe { OwnedFd::from_raw_fd(wake) },
        events: Mutex::new(VecDeque::new()),
    };
    if SHARED.set(shared).is_err() {
        return;
    }
    let android_app = android_app.clone();
    let spawned = thread::Builder::new()
        .name("guest-link".into())
        .spawn(move || Link::new(&android_app).run());
    if let Err(error) = spawned {
        log::error!("Failed to start the guest link: {error}");
    }
}

/// What the session's compositor offers.
pub struct Globals {
    registry: wl_registry::WlRegistry,
    list: Vec<(u32, String, u32)>,
    /// The compositor has told all of them.
    complete: bool,
}

impl Globals {
    /// Bind the compositor's `I`, if it has one, at `version` or the compositor's if lower.
    pub fn bind<I>(&self, queue: &QueueHandle<State>, version: u32) -> Option<I>
    where
        I: Proxy + 'static,
        State: Dispatch<I, ()>,
    {
        let (name, _, offered) = self
            .list
            .iter()
            .find(|(_, interface, _)| interface == I::interface().name)?;
        Some(self.registry.bind(*name, version.min(*offered), queue, ()))
    }
}

/// What the events of the session's compositor act on.
pub struct State {
    globals: Globals,
    pub clipboard: Option<clipboard::Desktop>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => state.globals.list.push((name, interface, version)),
            wl_registry::Event::GlobalRemove { name } => {
                state.globals.list.retain(|(it, _, _)| *it != name)
            }
            _ => {}
        }
    }
}

/// Answers the `sync` after asking for the globals.
impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.globals.complete = true;
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Why a connection to a compositor ended.
enum Ended {
    /// It has nothing the jobs could use.
    Unsuitable,
    Lost(String),
}

/// The connection to the session's compositor.
struct Session {
    socket: PathBuf,
    created: SystemTime,
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    /// The jobs have bound what they need.
    bound: bool,
    /// Requests are waiting for the socket to take them.
    clogged: bool,
}

impl Session {
    fn connect(socket: &Path, created: SystemTime) -> io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        let connection = Connection::from_socket(stream).map_err(io::Error::other)?;
        let queue = connection.new_event_queue();
        let handle = queue.handle();
        let display = connection.display();
        let registry = display.get_registry(&handle, ());
        display.sync(&handle, ());
        Ok(Self {
            socket: socket.to_owned(),
            created,
            connection,
            queue,
            state: State {
                globals: Globals {
                    registry,
                    list: Vec::new(),
                    complete: false,
                },
                clipboard: None,
            },
            bound: false,
            clogged: false,
        })
    }

    /// The clipboard's side of the session, once it is bound.
    fn clipboard(&mut self) -> Option<clipboard::Link<'_>> {
        let handle = self.queue.handle();
        self.state.clipboard.as_mut().map(|it| (it, handle))
    }

    /// Bind what the jobs need, once the compositor has told what it offers.
    fn bind(&mut self, clipboard: &mut Option<clipboard::Job>) -> Result<(), Ended> {
        if self.bound || !self.state.globals.complete {
            return Ok(());
        }
        self.bound = true;
        let handle = self.queue.handle();
        let seat: wl_seat::WlSeat = self.state.globals.bind(&handle, 1).ok_or(Ended::Unsuitable)?;
        self.state.clipboard = clipboard::Desktop::bind(&self.state.globals, &seat, &handle);
        let (Some(job), Some(desktop)) = (clipboard.as_mut(), self.state.clipboard.as_mut())
        else {
            return Err(Ended::Unsuitable);
        };
        log::info!(
            "Clipboard sharing: connected to the desktop at {}",
            self.socket.display()
        );
        job.connected((desktop, handle));
        Ok(())
    }

    /// Handle what the compositor sent, send what there is to send, and get ready to read.
    fn pump(
        &mut self,
        clipboard: &mut Option<clipboard::Job>,
    ) -> Result<ReadEventsGuard, Ended> {
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|error| Ended::Lost(error.to_string()))?;
            self.bind(clipboard)?;
            if let (Some(job), Some(desktop)) = (clipboard.as_mut(), self.state.clipboard.as_mut())
            {
                job.dispatched((desktop, self.queue.handle()));
            }
            match self.connection.flush() {
                Ok(()) => self.clogged = false,
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.clogged = true
                }
                Err(error) => return Err(Ended::Lost(error.to_string())),
            }
            // None: more has arrived meanwhile.
            if let Some(guard) = self.connection.prepare_read() {
                return Ok(guard);
            }
        }
    }
}

/// Watches for the session's compositor to make its socket.
struct Watcher {
    inotify: OwnedFd,
    directory: PathBuf,
    /// The watch on /tmp, for as long as the directory isn't there.
    parent: Option<i32>,
}

impl Watcher {
    fn new(directory: &Path) -> io::Result<Self> {
        let inotify = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut watcher = Self {
            inotify: unsafe { OwnedFd::from_raw_fd(inotify) },
            directory: directory.to_owned(),
            parent: None,
        };
        watcher.watch();
        Ok(watcher)
    }

    fn add(&self, path: &Path, mask: u32) -> Option<i32> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let watch =
            unsafe { libc::inotify_add_watch(self.inotify.as_raw_fd(), path.as_ptr(), mask) };
        (watch >= 0).then_some(watch)
    }

    /// Watch the directory, or until it is there, its parent. Watching again changes nothing.
    fn watch(&mut self) {
        let created = libc::IN_CREATE | libc::IN_MOVED_TO;
        let watched = self.add(
            &self.directory,
            created | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_ONLYDIR,
        );
        match (watched, self.parent) {
            (Some(_), Some(parent)) => {
                // Bionic's takes the watch as the unsigned number the kernel made of it.
                unsafe { libc::inotify_rm_watch(self.inotify.as_raw_fd(), parent as _) };
                self.parent = None;
            }
            (None, None) => {
                self.parent = self
                    .directory
                    .parent()
                    .and_then(|parent| self.add(parent, created | libc::IN_ONLYDIR));
            }
            _ => {}
        }
    }

    /// Forget the events: they only say that it's time to look.
    fn drain(&self) {
        let mut buffer = [0u8; 4096];
        while unsafe {
            libc::read(
                self.inotify.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        } > 0
        {}
    }
}

/// The uid of the session's user in the guest.
fn session_uid(username: &str) -> u32 {
    fs::read_to_string(Path::new(ARCH_FS_ROOT).join("etc/passwd"))
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            (fields.next()? == username).then_some(())?;
            fields.nth(1)?.parse().ok()
        })
        .unwrap_or(0)
}

/// Where the session's compositor makes its socket: the runtime directory every preset's
/// launcher sets (see `session_environment` in the setup).
fn runtime_directory() -> PathBuf {
    let username = get_application_context().local_config.user.username;
    Path::new(ARCH_FS_ROOT).join(format!("tmp/runtime-{}", session_uid(&username)))
}

/// The compositor sockets in the directory, the newest first: of compositors running in one
/// another, the innermost has the programs, and started last.
fn compositor_sockets(directory: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut sockets: Vec<_> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("wayland-") && !name.ends_with(".lock")
        })
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            let created = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            metadata
                .file_type()
                .is_socket()
                .then(|| (entry.path(), created))
        })
        .collect();
    sockets.sort_by(|a, b| b.1.cmp(&a.1));
    sockets
}

fn poll_entry(fd: RawFd, events: i16) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

struct Link {
    shared: &'static Shared,
    watcher: Option<Watcher>,
    session: Option<Session>,
    /// Compositors that had nothing for the jobs.
    unsuitable: Vec<(PathBuf, SystemTime)>,
    /// Look for the session's compositor (again).
    look: bool,
    /// When to ask a socket again that didn't answer, and how often it was asked.
    retry: Option<Instant>,
    tries: u32,
    clipboard: Option<clipboard::Job>,
}

impl Link {
    fn new(android_app: &AndroidApp) -> Self {
        // Writing to a program that has left is an error to handle, not a signal to die of.
        unsafe {
            let mut signals: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGPIPE);
            libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
        }
        let clipboard = clipboard::Job::new(android_app);
        // Android's runtime names the threads that attach to it "Thread-<n>".
        unsafe { libc::prctl(libc::PR_SET_NAME, c"guest-link".as_ptr()) };
        Self {
            shared: SHARED.get().expect("the link's thread starts after its state is set"),
            watcher: None,
            session: None,
            unsuitable: Vec::new(),
            look: true,
            retry: None,
            tries: 0,
            clipboard,
        }
    }

    /// Whether any job is turned on.
    fn wanted(&self) -> bool {
        self.clipboard.is_some() && get_application_context().local_config.clipboard.sync
    }

    fn disconnect(&mut self, why: &str) {
        if let Some(session) = self.session.take() {
            log::info!(
                "Clipboard sharing: left the desktop at {} ({why})",
                session.socket.display()
            );
            if let Some(job) = &mut self.clipboard {
                job.disconnected();
            }
        }
    }

    /// Connect to the newest compositor of the session that the jobs can use.
    fn find_session(&mut self) {
        self.look = false;
        self.retry = None;
        let wanted = self.wanted();
        if let Some(job) = &mut self.clipboard {
            job.turn(wanted);
        }
        if !wanted {
            self.disconnect("turned off");
            self.watcher = None;
            return;
        }
        let directory = runtime_directory();
        match &mut self.watcher {
            Some(watcher) if watcher.directory == directory => watcher.watch(),
            watcher => {
                *watcher = Watcher::new(&directory)
                    .map_err(|error| log::error!("Failed to watch for the desktop: {error}"))
                    .ok()
            }
        }

        for (socket, created) in compositor_sockets(&directory) {
            if self.unsuitable.contains(&(socket.clone(), created)) {
                continue;
            }
            if self
                .session
                .as_ref()
                .is_some_and(|it| it.socket == socket && it.created == created)
            {
                return;
            }
            match Session::connect(&socket, created) {
                Ok(session) => {
                    self.disconnect("a newer compositor is there");
                    self.session = Some(session);
                    self.tries = 0;
                    INPUT.store(false, Ordering::Relaxed);
                    return;
                }
                // Not listening yet, or what a session that was killed left behind.
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    if self.tries < CONNECT_TRIES {
                        self.tries += 1;
                        self.retry = Some(Instant::now() + CONNECT_PAUSE);
                    }
                }
                Err(error) => log::info!(
                    "Clipboard sharing: no connection to {}: {error}",
                    socket.display()
                ),
            }
        }
    }

    /// What other threads told since the last time.
    fn hear(&mut self) {
        loop {
            let event = self.shared.events.lock().unwrap().pop_front();
            let Some(event) = event else {
                return;
            };
            let desktop = self.session.as_mut().and_then(Session::clipboard);
            match (event, &mut self.clipboard) {
                (Event::Focus(focused), Some(job)) => job.focus(focused, desktop),
                (Event::AndroidClipboard, Some(job)) => job.android_changed(desktop),
                (Event::SessionStarting, _) => {
                    self.tries = 0;
                    self.look = true;
                }
                _ => {}
            }
        }
    }

    fn run(mut self) {
        loop {
            self.hear();
            if self.look || self.retry.is_some_and(|at| at <= Instant::now()) {
                self.find_session();
            }

            let mut reading = None;
            if let Some(session) = &mut self.session {
                match session.pump(&mut self.clipboard) {
                    Ok(guard) => reading = Some(guard),
                    Err(Ended::Unsuitable) => {
                        let tried = (session.socket.clone(), session.created);
                        log::info!(
                            "Clipboard sharing: the compositor at {} has no data control",
                            tried.0.display()
                        );
                        self.unsuitable.push(tried);
                        self.disconnect("nothing to use there");
                        self.look = true;
                        continue;
                    }
                    Err(Ended::Lost(why)) => {
                        self.disconnect(&why);
                        self.look = true;
                        continue;
                    }
                }
            }

            let mut entries = vec![poll_entry(self.shared.wake.as_raw_fd(), libc::POLLIN)];
            if let Some(watcher) = &self.watcher {
                entries.push(poll_entry(watcher.inotify.as_raw_fd(), libc::POLLIN));
            }
            let session_entry = entries.len();
            if let Some(session) = &self.session {
                let events = if session.clogged {
                    libc::POLLIN | libc::POLLOUT
                } else {
                    libc::POLLIN
                };
                entries.push(poll_entry(
                    session.connection.as_fd().as_raw_fd(),
                    events,
                ));
            }
            let job_entries = entries.len();
            if let Some(job) = &self.clipboard {
                job.waits_for(&mut entries);
            }

            // Forever, unless something is under way.
            let deadline = [self.retry, self.clipboard.as_ref().and_then(|it| it.deadline())]
                .into_iter()
                .flatten()
                .min();
            let timeout = deadline.map_or(-1, |at| {
                at.saturating_duration_since(Instant::now()).as_millis() as i32 + 1
            });
            let ready = unsafe {
                libc::poll(entries.as_mut_ptr(), entries.len() as libc::nfds_t, timeout)
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    log::error!("The guest link stopped: {error}");
                    return;
                }
                continue;
            }

            if entries[0].revents != 0 {
                let mut count = [0u8; 8];
                unsafe {
                    libc::read(
                        self.shared.wake.as_raw_fd(),
                        count.as_mut_ptr().cast(),
                        count.len(),
                    )
                };
            }
            if let Some(watcher) = &self.watcher {
                if entries[1].revents != 0 {
                    watcher.drain();
                    self.tries = 0;
                    self.look = true;
                }
            }
            if let Some(guard) = reading {
                if entries[session_entry].revents & !libc::POLLOUT != 0 {
                    if let Err(error) = guard.read() {
                        self.disconnect(&error.to_string());
                        self.look = true;
                    }
                }
            }
            if let Some(job) = &mut self.clipboard {
                for entry in &entries[job_entries..] {
                    if entry.revents != 0 {
                        job.ready(entry.fd);
                    }
                }
                job.expire(Instant::now());
            }
        }
    }
}
