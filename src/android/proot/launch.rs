use super::process::ArchProcess;
use crate::android::utils::application_context::get_application_context;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static LAUNCH_RUNNING: AtomicBool = AtomicBool::new(false);
/// The proot running the desktop session, 0 when there is none.
static DESKTOP_PID: AtomicU32 = AtomicU32::new(0);

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
                log::trace!("{}", line);
            }
        });
        let status = child.wait();
        log::info!("Desktop session ended: {status:?}");
    });
}

/// Make a proot end everything running in it, then exit. It can't catch SIGKILL, which would
/// leave its processes running untraced.
pub fn stop_proot(pid: u32) {
    unsafe { libc::kill(pid as i32, libc::SIGQUIT) };
}

/// End the desktop session and start it again, e.g. when it hangs.
pub fn restart() {
    let pid = DESKTOP_PID.load(Ordering::Acquire);
    if pid != 0 {
        stop_proot(pid);
        let deadline = Instant::now() + Duration::from_secs(10);
        while LAUNCH_RUNNING.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
    }
    launch();
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
    kill_children();
    std::process::exit(0);
}
