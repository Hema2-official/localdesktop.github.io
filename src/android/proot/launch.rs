use super::process::ArchProcess;
use crate::android::guest;
use crate::android::guest::bus::{self, Bus};
use crate::android::session;
use crate::core::dbus::{self, Writer};
use crate::android::utils::application_context::{get_application_context, reload_local_config};
use crate::core::config::ARCH_FS_ROOT;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static LAUNCH_RUNNING: AtomicBool = AtomicBool::new(false);
/// The proot running the desktop session, 0 when there is none.
static DESKTOP_PID: AtomicU32 = AtomicU32::new(0);
/// Set while the app ends the session on purpose (restart, quit), which isn't worth reporting.
static STOPPING: AtomicBool = AtomicBool::new(false);
/// The session ended by itself and hasn't been started again.
static STOPPED: AtomicBool = AtomicBool::new(false);
/// A session that ends this soon failed to start.
const FAILED_START: Duration = Duration::from_secs(30);
/// The session's output, inside the rootfs so the terminal can show it. The previous session's
/// is kept next to it with `.old` appended.
pub const SESSION_LOG: &str = "/var/log/localdesktop-session.log";

/// The proot running the desktop session, while there is one.
pub fn desktop_pid() -> Option<u32> {
    Some(DESKTOP_PID.load(Ordering::Acquire)).filter(|it| *it != 0)
}

/// Whether the desktop session ended by itself.
pub fn stopped() -> bool {
    STOPPED.load(Ordering::Acquire)
}

fn open_session_log() -> Option<File> {
    let path = Path::new(ARCH_FS_ROOT).join(SESSION_LOG.trim_start_matches('/'));
    let _ = fs::create_dir_all(path.parent()?);
    let _ = fs::rename(&path, path.with_extension("log.old"));
    File::create(&path).ok()
}

struct LaunchRunningGuard;

impl Drop for LaunchRunningGuard {
    fn drop(&mut self) {
        DESKTOP_PID.store(0, Ordering::Release);
        LAUNCH_RUNNING.store(false, Ordering::Release);
    }
}

pub fn launch() {
    if LAUNCH_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        log::info!("Skipping launch because the desktop session is already running");
        return;
    }
    guest::notify(guest::Event::SessionStarting);

    thread::spawn(move || {
        let _guard = LaunchRunningGuard;

        // Clean up potential leftover files for display :1, and keep the X11 socket directory
        // open to every user: Xwayland creates its socket there as the session's user.
        ArchProcess {
            command: "rm -f /tmp/.X1-lock /tmp/.X11-unix/X1; mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix".into(),
            user: None,
            log: None,
        }
        .run();

        let local_config = get_application_context().local_config;
        super::ssh::start(&local_config);
        let username = local_config.user.username;

        let desktop = ArchProcess {
            command: local_config.command.launch,
            user: Some(username),
            log: None,
        };
        STOPPED.store(false, Ordering::Release);
        let mut session_log = open_session_log();
        let started = Instant::now();
        let spawned = desktop
            .command()
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                log::error!("Failed to start the desktop session: {error}");
                return;
            }
        };
        DESKTOP_PID.store(child.id(), Ordering::Release);
        // Log the output on a thread of its own: processes left behind can keep the pipe open
        // after the session has ended.
        let output = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if let Some(file) = session_log.as_mut() {
                    let _ = writeln!(file, "{line}");
                }
                log::trace!("{}", line);
            }
        });
        let status = child.wait();
        log::info!("Desktop session ended: {status:?}");
        if !STOPPING.swap(false, Ordering::AcqRel) {
            STOPPED.store(true, Ordering::Release);
            session::desktop_stopped(started.elapsed() < FAILED_START);
        }
    });
}

/// Make a proot end everything running in it, then exit. It can't catch SIGKILL, which would
/// leave its processes running untraced.
pub fn stop_proot(pid: u32) {
    unsafe { libc::kill(pid as i32, libc::SIGQUIT) };
}

/// How long the session's programs get to save and quit once it was asked to log out, before
/// the app looks at what holds the logout up.
const LOGOUT_GRACE: Duration = Duration::from_secs(8);
/// How long a logout may wait for the user to answer the programs' questions.
const LOGOUT_LIMIT: Duration = Duration::from_secs(180);

/// How a logout went.
#[derive(Debug, PartialEq, Eq)]
enum Logout {
    /// The session ended.
    Ended,
    /// It was cancelled on the desktop (a program's question): the session goes on.
    Cancelled,
    /// It couldn't be asked, or didn't end: the app has to end the session itself.
    Failed,
}

/// The desktop's session manager, which logs out the way the desktop's own Log Out does.
#[derive(Debug, Clone, Copy)]
enum SessionManager {
    Plasma,
    Xfce,
}

impl SessionManager {
    /// Ask the session's manager to log out. Without waiting for its answer: Plasma's only
    /// comes once the session is gone, if at all.
    fn ask(bus: &mut Bus) -> Option<Self> {
        let has = |bus: &mut Bus, name: &str| {
            let mut body = Writer::new();
            body.string(name);
            bus.call(bus::BUS, bus::BUS_PATH, bus::BUS, "NameHasOwner", "s", &body.into_bytes())
                .ok()
                .and_then(|reply| reply.arguments().boolean().ok())
                .unwrap_or(false)
        };
        // Plasma's session manager, ksmserver, always runs; org.kde.Shutdown is only started when
        // called, which the bus does for the logout.
        let manager = if has(bus, "org.kde.ksmserver") {
            bus.send(
                "org.kde.Shutdown",
                "/Shutdown",
                "org.kde.Shutdown",
                "logout",
                "",
                &[],
                dbus::NO_REPLY_EXPECTED,
            )
            .ok()?;
            Self::Plasma
        } else if has(bus, "org.xfce.SessionManager") {
            // No confirmation dialog, and the programs may save.
            let mut body = Writer::new();
            body.boolean(false).boolean(true);
            bus.send(
                "org.xfce.SessionManager",
                "/org/xfce/SessionManager",
                "org.xfce.Session.Manager",
                "Logout",
                "bb",
                &body.into_bytes(),
                dbus::NO_REPLY_EXPECTED,
            )
            .ok()?;
            Self::Xfce
        } else {
            return None;
        };
        Some(manager)
    }

    /// Whether the logout is still under way; `None` once the session's bus has gone with it.
    fn logging_out(self, bus: &mut Bus) -> Option<bool> {
        match self {
            Self::Plasma => bus
                .call(
                    "org.kde.ksmserver",
                    "/KSMServer",
                    "org.kde.KSMServerInterface",
                    "isShuttingDown",
                    "",
                    &[],
                )
                .ok()?
                .arguments()
                .boolean()
                .ok(),
            // 1 is idle, 3 and 4 the phases of a shutdown.
            Self::Xfce => bus
                .call(
                    "org.xfce.SessionManager",
                    "/org/xfce/SessionManager",
                    "org.xfce.Session.Manager",
                    "GetState",
                    "",
                    &[],
                )
                .ok()?
                .arguments()
                .u32()
                .ok()
                .map(|state| state >= 3),
        }
    }
}

/// Log the desktop out like its own Log Out does, so that its programs can save, or ask about
/// unsaved work, and wait for the session to end. `before` is what comes next, for the
/// notification.
fn log_out(before: &str) -> Logout {
    let Some(address) = bus::session_address() else {
        return Logout::Failed;
    };
    let mut bus = match Bus::connect(&address) {
        Ok(bus) => bus,
        Err(error) => {
            log::info!("Logout: no connection to the session's bus ({error})");
            return Logout::Failed;
        }
    };
    let Some(manager) = SessionManager::ask(&mut bus) else {
        log::info!("Logout: the session has no manager to ask");
        return Logout::Failed;
    };
    log::info!("Logout: asked {manager:?}'s session manager");
    let asked = Instant::now();
    let mut said = false;
    let mut looked = asked;
    let outcome = loop {
        if !LAUNCH_RUNNING.load(Ordering::Acquire) {
            break Logout::Ended;
        }
        let waited = asked.elapsed();
        if waited >= LOGOUT_LIMIT {
            log::info!("Logout: the session didn't end in {} s", LOGOUT_LIMIT.as_secs());
            break Logout::Failed;
        }
        if waited >= LOGOUT_GRACE && looked.elapsed() >= Duration::from_secs(1) {
            looked = Instant::now();
            match manager.logging_out(&mut bus) {
                Some(false) => {
                    log::info!("Logout: cancelled on the desktop");
                    break Logout::Cancelled;
                }
                // Programs wait for answers.
                Some(true) if !said => {
                    said = true;
                    session::status(Some(format!(
                        "A program on the desktop asks something before {before}. \
                         Open Local Desktop to answer."
                    )));
                }
                Some(true) => {}
                // The bus went with the session: what is left behind are stragglers.
                None => {
                    log::info!("Logout: the session's bus is gone");
                    break Logout::Failed;
                }
            }
        }
        thread::sleep(Duration::from_millis(200));
    };
    if said {
        session::status(None);
    }
    outcome
}

/// End the desktop session, logging it out first if it can be. False if the logout was
/// cancelled on the desktop, so the session goes on.
fn end_session(before: &str) -> bool {
    let Some(pid) = desktop_pid() else {
        return true;
    };
    STOPPING.store(true, Ordering::Release);
    match log_out(before) {
        Logout::Ended => return true,
        Logout::Cancelled => {
            STOPPING.store(false, Ordering::Release);
            return false;
        }
        Logout::Failed => {}
    }
    stop_proot(pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    while LAUNCH_RUNNING.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    true
}

/// End the desktop session and start it again, e.g. when it hangs. A session that can log out
/// does first, so that its programs can save.
pub fn restart() {
    static RESTARTING: AtomicBool = AtomicBool::new(false);
    if RESTARTING.swap(true, Ordering::AcqRel) {
        log::info!("Ignoring a restart request while the desktop is already restarting");
        return;
    }
    if !end_session("restarting") {
        session::refresh();
        RESTARTING.store(false, Ordering::Release);
        return;
    }
    // Pick up whatever was fixed in the config meanwhile.
    reload_local_config();
    session::refresh();
    launch();
    RESTARTING.store(false, Ordering::Release);
}

/// The notification's Quit: log the desktop out, so that its programs can save, then leave. A
/// logout cancelled on the desktop cancels the quitting too.
pub fn log_out_and_quit() {
    if end_session("quitting") {
        quit();
    }
    session::refresh();
}

/// End every process the app started (proot and everything inside it, the audio daemons), as
/// the app process itself exits: Android doesn't always take them down with it.
pub fn kill_children() {
    let me = std::process::id();
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|it| it.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        // Not the /proc entry's owner: that reads as root for non-dumpable processes.
        let owned = std::fs::read_to_string(entry.path().join("status"))
            .ok()
            .and_then(|status| {
                let line = status.lines().find(|it| it.starts_with("Uid:"))?;
                line.split_whitespace().nth(1)?.parse::<u32>().ok()
            })
            == Some(uid);
        if owned {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

/// Stop everything and leave.
pub fn quit() -> ! {
    log::info!("Quitting");
    STOPPING.store(true, Ordering::Release);
    kill_children();
    std::process::exit(0);
}
