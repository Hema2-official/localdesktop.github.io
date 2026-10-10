//! The `localdesktop` command (`core::control`): programs in the guest write a line to a pipe in
//! the rootfs, and the link has Android do what it says.
//!
//! The link holds the pipe open for reading and writing both, so it never sees the end of it
//! when a writer goes, and a writer never waits for a reader while the app runs.

use crate::android::proot::launch;
use crate::android::terminal;
use crate::android::utils::application_context::get_application_context;
use crate::android::utils::java::AppClass;
use crate::core::config::ARCH_FS_ROOT;
use crate::core::control::{self, Command};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::thread;
use winit::platform::android::activity::AndroidApp;

/// A line longer than any command is dropped.
const MAX_LINE: usize = 16 << 10;

pub struct Job {
    commands: AppClass,
    pipe: Option<OwnedFd>,
    /// What was read and doesn't make a whole line yet.
    buffer: Vec<u8>,
}

impl Job {
    /// For the calling thread. `None` if the app's Java side can't be reached.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        Some(Self {
            commands: AppClass::new(android_app, "app.polarbear.Commands")?,
            pipe: None,
            buffer: Vec::new(),
        })
    }

    /// Make the pipe, once there is a rootfs; again after a fresh install.
    pub fn open(&mut self) {
        if self.pipe.is_some() || !Path::new(ARCH_FS_ROOT).join("run").is_dir() {
            return;
        }
        let path = Path::new(ARCH_FS_ROOT).join(control::PIPE.trim_start_matches('/'));
        match make_pipe(&path) {
            Ok(pipe) => {
                log::info!("Commands: listening at {}", control::PIPE);
                self.pipe = Some(pipe);
            }
            Err(error) => log::error!("Commands: no pipe at {}: {error}", path.display()),
        }
    }

    pub fn waits_for(&self, entries: &mut Vec<libc::pollfd>) {
        if let Some(pipe) = &self.pipe {
            entries.push(super::poll_entry(pipe.as_raw_fd(), libc::POLLIN));
        }
    }

    /// `fd` has something to read, if it's the pipe.
    pub fn ready(&mut self, fd: RawFd) {
        if self.pipe.as_ref().is_none_or(|it| it.as_raw_fd() != fd) {
            return;
        }
        let mut chunk = [0u8; 4096];
        loop {
            let read = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if read <= 0 {
                break;
            }
            self.buffer.extend_from_slice(&chunk[..read as usize]);
        }
        while let Some(end) = self.buffer.iter().position(|it| *it == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..end]).into_owned();
            match control::parse(&line) {
                Some(command) => self.run(command),
                None => log::info!("Commands: ignored a line that isn't one"),
            }
        }
        if self.buffer.len() > MAX_LINE {
            self.buffer.clear();
        }
    }

    fn run(&mut self, command: Command) {
        match command {
            Command::OpenUrl(url) => {
                log::info!("Commands: opens a link on Android");
                self.commands.call("open a link", |env, class, activity| {
                    let url = env.new_string(&url)?;
                    env.call_static_method(
                        class,
                        "openUrl",
                        "(Landroid/content/Context;Ljava/lang/String;)V",
                        &[activity.into(), (&url).into()],
                    )?;
                    Ok(())
                });
            }
            Command::OpenSettings => {
                log::info!("Commands: opens Android's settings for the app");
                self.commands
                    .call("open the app's settings", |env, class, activity| {
                        env.call_static_method(
                            class,
                            "openSettings",
                            "(Landroid/content/Context;)V",
                            &[activity.into()],
                        )?;
                        Ok(())
                    });
            }
            Command::OpenTerminal => {
                let url = match terminal::url() {
                    Ok(url) => url,
                    Err(error) => {
                        log::error!("Commands: no terminal to open: {error}");
                        return;
                    }
                };
                log::info!("Commands: opens the terminal");
                self.commands
                    .call("open the terminal", |env, class, activity| {
                        let url = env.new_string(&url)?;
                        env.call_static_method(
                            class,
                            "openTerminal",
                            "(Landroid/content/Context;Ljava/lang/String;)V",
                            &[activity.into(), (&url).into()],
                        )?;
                        Ok(())
                    });
            }
            Command::RestartDesktop => {
                log::info!("Commands: restarts the desktop");
                // Restarting waits for the session to end, which is the link's to watch.
                thread::spawn(launch::restart);
            }
            Command::Install(guest) => self.install(&guest),
        }
    }

    fn install(&mut self, guest: &str) {
        let shared_storage = get_application_context().permission_all_files_access;
        let name = Path::new(guest)
            .file_name()
            .map(|it| it.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(path) = control::host_path(guest, ARCH_FS_ROOT, shared_storage) else {
            log::info!("Commands: can't read an app to install where it is");
            self.notice(&format!("Local Desktop can't read {name} where it is"));
            return;
        };
        log::info!("Commands: installs an app on Android");
        self.commands
            .call("install an app", |env, class, activity| {
                let path = env.new_string(path.to_string_lossy())?;
                let name = env.new_string(&name)?;
                env.call_static_method(
                    class,
                    "install",
                    "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)V",
                    &[activity.into(), (&path).into(), (&name).into()],
                )?;
                Ok(())
            });
    }

    fn notice(&self, text: &str) {
        self.commands.call("tell the user", |env, class, activity| {
            let text = env.new_string(text)?;
            env.call_static_method(
                class,
                "notice",
                "(Landroid/content/Context;Ljava/lang/String;)V",
                &[activity.into(), (&text).into()],
            )?;
            Ok(())
        });
    }
}

/// The pipe at `path`, made anew unless it's there, open for reading and writing.
fn make_pipe(path: &Path) -> io::Result<OwnedFd> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    }
    let is_pipe = fs::symlink_metadata(path).map(|it| it.file_type().is_fifo());
    if !matches!(is_pipe, Ok(true)) {
        let _ = fs::remove_file(path);
        let name = CString::new(path.as_os_str().as_bytes())?;
        if unsafe { libc::mkfifo(name.as_ptr(), 0o666) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // Every user of the guest may write to it, whatever the app's umask.
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    let name = CString::new(path.as_os_str().as_bytes())?;
    let flags = libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let fd = unsafe { libc::open(name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
