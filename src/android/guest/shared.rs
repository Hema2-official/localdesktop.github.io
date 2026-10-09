//! What other apps share with the desktop: Android's share sheet and "Open with"
//! (`ShareActivity.java`). The activity copies each share into the app's storage, a request in
//! `files/shared/<id>/`, complete once its name has lost the leading dot. Here its files move into
//! the session user's Downloads folder and open on the desktop: one file in the program for its
//! type, several in the file manager, a link (the request's `.link`) in the browser.
//!
//! The desktop's own programs have to open them: a program the app started would run under a
//! proot of its own, which ends it as soon as the command that started it returns. So the
//! session's D-Bus starts `localdesktop-open` (the setup writes it), which opens what the link
//! left in its spool, and the file manager through `org.freedesktop.FileManager1`. Both wait for
//! a program to start, so a thread of its own does the work.

use super::bus::{self, Bus};
use crate::android::utils::application_context::get_application_context;
use crate::core::config::ARCH_FS_ROOT;
use crate::core::dbus::Writer;
use crate::core::sharing;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// `localdesktop-open`'s D-Bus name. It never takes the name, so the bus starts it every time.
pub const OPEN_SERVICE: &str = "app.polarbear.Open";
pub const OPEN_HELPER: &str = "/usr/local/bin/localdesktop-open";
/// Where `localdesktop-open` finds what to open, a file each, in the session's runtime directory.
pub const SPOOL: &str = "localdesktop-open";

const FILE_MANAGER: &str = "org.freedesktop.FileManager1";
const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";
/// Programs take a while to start under proot, the file manager especially.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// A share can start the app, whose desktop then takes a while to come up.
const BUS_TRIES: u32 = 90;
const BUS_PAUSE: Duration = Duration::from_secs(2);

static WORKING: AtomicBool = AtomicBool::new(false);

/// Where `ShareActivity` leaves its requests.
fn requests_directory() -> PathBuf {
    get_application_context().data_dir.join("shared")
}

/// The complete requests, oldest first.
fn requests() -> Vec<PathBuf> {
    let mut requests: Vec<PathBuf> = fs::read_dir(requests_directory())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    requests.sort();
    requests
}

/// Hand what was shared to the desktop, on a thread of its own, unless one is at it.
pub fn look() {
    if requests().is_empty() || WORKING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = thread::Builder::new().name("shared".into()).spawn(|| loop {
        let progress = hand_over();
        WORKING.store(false, Ordering::Release);
        // Shares that came meanwhile, whose `look()` found this thread at work.
        if !progress || requests().is_empty() || WORKING.swap(true, Ordering::AcqRel) {
            return;
        }
    });
    if let Err(error) = spawned {
        WORKING.store(false, Ordering::Release);
        log::error!("Sharing: no thread to hand shares to the desktop: {error}");
    }
}

/// What a request had, in the Downloads folder now: the guest's paths.
struct Shared {
    files: Vec<String>,
    link: Option<String>,
}

/// Move the requests' files to the desktop and open them. Whether any of them got there.
fn hand_over() -> bool {
    let Some(mut bus) = session_bus() else {
        log::info!("Sharing: no desktop to take the shares; they wait for the next one");
        return false;
    };
    let downloads = download_directory();
    let mut progress = false;
    for request in requests() {
        let id = request
            .file_name()
            .map(|it| it.to_string_lossy().into_owned())
            .unwrap_or_default();
        match take(&request, &downloads) {
            Ok(shared) => {
                progress = true;
                if let Err(error) = open(&mut bus, &shared, &downloads, &id) {
                    log::error!("Sharing: the desktop didn't open it: {error}");
                }
            }
            Err(error) => log::error!("Sharing: a share didn't get to the desktop: {error}"),
        }
    }
    progress
}

/// The session's bus, waiting for the desktop to come up.
fn session_bus() -> Option<Bus> {
    for _ in 0..BUS_TRIES {
        if let Some(bus) = bus::session_address().and_then(|it| Bus::connect(&it).ok()) {
            return Some(bus);
        }
        thread::sleep(BUS_PAUSE);
    }
    None
}

fn host_path(guest: &str) -> PathBuf {
    Path::new(ARCH_FS_ROOT).join(guest.trim_start_matches('/'))
}

/// The session user's Downloads folder, the guest's path.
fn download_directory() -> String {
    let user = get_application_context().local_config.user.username;
    let passwd = fs::read_to_string(host_path("/etc/passwd")).unwrap_or_default();
    let home = sharing::home_from_passwd(&passwd, &user).unwrap_or_else(|| {
        if user == "root" {
            "/root".into()
        } else {
            format!("/home/{user}")
        }
    });
    let user_dirs = fs::read_to_string(host_path(&format!("{home}/.config/user-dirs.dirs"))).ok();
    sharing::download_directory(&home, user_dirs.as_deref())
}

/// Move a request's files into `downloads` (the guest's path), under names nothing there has.
fn take(request: &Path, downloads: &str) -> io::Result<Shared> {
    let target = host_path(downloads);
    fs::create_dir_all(&target)?;
    let mut names: Vec<String> = fs::read_dir(request)?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    let link = fs::read_to_string(request.join(".link"))
        .ok()
        .map(|it| it.trim().to_string())
        .filter(|it| !it.is_empty());
    let mut files = Vec::new();
    for name in names {
        let mut free = name.clone();
        for n in 2.. {
            if target.join(&free).symlink_metadata().is_err() {
                break;
            }
            free = sharing::numbered(&name, n);
        }
        fs::rename(request.join(&name), target.join(&free))?;
        files.push(format!("{downloads}/{free}"));
    }
    fs::remove_dir_all(request)?;
    Ok(Shared { files, link })
}

fn open(bus: &mut Bus, shared: &Shared, downloads: &str, id: &str) -> io::Result<()> {
    match (&shared.link, shared.files.as_slice()) {
        (Some(link), _) => {
            log::info!("Sharing: a link, opening on the desktop");
            launch(bus, link, id)
        }
        (None, []) => Ok(()),
        (None, [file]) => {
            log::info!("Sharing: a file, into the desktop's Downloads folder, opening");
            launch(bus, file, id)
        }
        (None, files) => {
            log::info!(
                "Sharing: {} files, into the desktop's Downloads folder, showing them",
                files.len()
            );
            show(bus, files).or_else(|error| {
                log::info!("Sharing: no file manager to show them ({error}), opening the folder");
                launch(bus, downloads, id)
            })
        }
    }
}

/// Have the session's D-Bus start `localdesktop-open` for `target`.
fn launch(bus: &mut Bus, target: &str, id: &str) -> io::Result<()> {
    let spool = super::runtime_directory().join(SPOOL);
    fs::create_dir_all(&spool)?;
    // Whole files only: the helper leaves dot files alone.
    let partial = spool.join(format!(".{id}"));
    fs::write(&partial, target)?;
    fs::rename(&partial, spool.join(id))?;
    let mut body = Writer::new();
    body.string(OPEN_SERVICE).u32(0);
    let body = body.into_bytes();
    let started = bus.call_within(
        START_TIMEOUT,
        bus::BUS,
        bus::BUS_PATH,
        bus::BUS,
        "StartServiceByName",
        "su",
        &body,
    );
    match started {
        // The helper exits without taking its name, which the bus reports once it has run.
        Err(error) if error.to_string().contains("Spawn.ChildExited") => Ok(()),
        other => other.map(|_| ()),
    }
}

/// Show `files` (the guest's paths) in the file manager.
fn show(bus: &mut Bus, files: &[String]) -> io::Result<()> {
    let uris: Vec<String> = files.iter().map(|it| sharing::file_uri(it)).collect();
    let uris: Vec<&str> = uris.iter().map(String::as_str).collect();
    let mut body = Writer::new();
    body.strings(&uris).string("");
    let body = body.into_bytes();
    bus.call_within(
        START_TIMEOUT,
        FILE_MANAGER,
        FILE_MANAGER_PATH,
        FILE_MANAGER,
        "ShowItems",
        "ass",
        &body,
    )?;
    Ok(())
}
