//! Backups of the Linux system from the settings page (`core::backup` makes and reads them).
//!
//! Android's file picker chooses where a backup goes or which one to restore (`SettingsBridge`),
//! which hands the file over as a descriptor. A backup logs a running desktop out first, so that
//! its programs save and nothing changes while it's copied, then starts it again. A restore lays
//! the system out beside the current one, which can go on running meanwhile; the next time the
//! app starts, before setup, the restored system takes the current one's place
//! ([`swap_in_restored`]).

use crate::android::proot::launch;
use crate::android::session;
use crate::core::backup::{self, Manifest, Progress};
use crate::core::config::{self, ARCH_FS_ROOT, CONFIG_FILE};
use serde::Serialize;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// What the settings page shows of a backup or a restore.
#[derive(Debug, Clone, Default, Serialize)]
struct Job {
    /// "backup" or "restore".
    kind: &'static str,
    /// What it does now.
    phase: String,
    done: Progress,
    total: Progress,
    /// The bytes written to the backup file so far.
    written: u64,
    finished: bool,
    error: Option<String>,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);
/// The backup the page last looked at for a restore.
static INSPECTED: Mutex<Option<Manifest>> = Mutex::new(None);

/// A sibling of the system's folder.
fn beside(name: &str) -> PathBuf {
    Path::new(ARCH_FS_ROOT).with_file_name(name)
}

/// Where a restore lays the system out, and where it waits once complete.
fn restoring() -> PathBuf {
    beside("arch.restoring")
}
fn restored() -> PathBuf {
    beside("arch.restored")
}
/// What the restored system is, for the page.
fn restored_description() -> PathBuf {
    beside("arch.restored.json")
}
/// The system a restore replaced, until it's deleted.
fn replaced() -> PathBuf {
    beside("arch.replaced")
}

fn update(change: impl FnOnce(&mut Job)) {
    if let Some(job) = JOB.lock().unwrap_or_else(|it| it.into_inner()).as_mut() {
        change(job);
    }
}

/// Start a job, unless one runs.
fn begin(kind: &'static str, phase: &str) -> Result<(), String> {
    let mut job = JOB.lock().unwrap_or_else(|it| it.into_inner());
    if job.as_ref().is_some_and(|it| !it.finished) {
        return Err("A backup or a restore is still going on".into());
    }
    *job = Some(Job {
        kind,
        phase: phase.into(),
        ..Job::default()
    });
    Ok(())
}

fn end(error: Option<String>) {
    update(|job| {
        job.finished = true;
        job.phase.clear();
        job.error = error;
    });
}

/// What the page shows: the job, a restored system waiting for the next start, and how big the
/// package cache is.
pub fn state() -> Value {
    let job = JOB.lock().unwrap_or_else(|it| it.into_inner()).clone();
    let waiting = restored().is_dir().then(|| {
        fs::read(restored_description())
            .ok()
            .and_then(|it| serde_json::from_slice::<Value>(&it).ok())
            .unwrap_or_else(|| json!({}))
    });
    let cache = Path::new(ARCH_FS_ROOT).join(backup::PACKAGE_CACHE);
    let cache_bytes: u64 = fs::read_dir(cache)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|it| it.metadata().ok())
                .map(|it| it.len())
                .sum()
        })
        .unwrap_or(0);
    json!({ "job": job, "restored": waiting, "package_cache_bytes": cache_bytes })
}

/// Counts what goes through it.
struct Counting<W> {
    inner: W,
    written: Arc<AtomicU64>,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.written.fetch_add(written as u64, Ordering::Relaxed);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Back the system up into `fd`, a file Android's file picker opened for writing.
pub fn start_backup(fd: i32, package_cache: bool, version: &str) -> Value {
    if fd < 0 {
        return json!({ "error": "Android didn't open the file" });
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !Path::new(ARCH_FS_ROOT).join("etc").is_dir() {
        return json!({ "error": "Local Desktop isn't set up yet" });
    }
    if let Err(error) = begin("backup", "Starting") {
        return json!({ "error": error });
    }
    let version = version.to_string();
    thread::spawn(move || {
        let error = back_up(file, package_cache, version).err();
        if let Some(error) = &error {
            log::warn!("Backup: failed: {error}");
        }
        end(error);
    });
    json!({ "ok": true })
}

fn back_up(file: File, package_cache: bool, version: String) -> Result<(), String> {
    let running = launch::desktop_pid().is_some();
    if running {
        update(|job| job.phase = "Logging the desktop out".into());
        if !launch::log_out_for("the backup") {
            return Err("The logout was cancelled on the desktop".into());
        }
    }
    session::status(Some("Backing up the Linux system…".into()));
    update(|job| job.phase = "Backing up".into());
    let content =
        fs::read_to_string(Path::new(ARCH_FS_ROOT).join(CONFIG_FILE.trim_start_matches('/')))
            .unwrap_or_default();
    let config = config::parse_text(&content);
    let about = Manifest {
        app: env!("LOCALDESKTOP_PACKAGE").into(),
        version,
        desktop: config.desktop.preset.trim().to_string(),
        user: config.user.username.clone(),
        ..Manifest::default()
    };
    let written = Arc::new(AtomicU64::new(0));
    let output = Counting {
        inner: BufWriter::with_capacity(1 << 20, file),
        written: written.clone(),
    };
    log::info!("Backup: started, package cache included: {package_cache}");
    let result = backup::back_up(
        Path::new(ARCH_FS_ROOT),
        output,
        about,
        package_cache,
        &mut |done, total| {
            let bytes = written.load(Ordering::Relaxed);
            update(|job| {
                job.done = done;
                job.total = total;
                job.written = bytes;
            });
        },
    );
    session::status(None);
    let written = written.load(Ordering::Relaxed);
    update(|job| job.written = written);
    if running {
        update(|job| job.phase = "Starting the desktop again".into());
        launch::start_again();
    }
    let manifest = result.map_err(|error| error.to_string())?;
    log::info!(
        "Backup: {} entries, {} MB, written as {} MB",
        manifest.files,
        manifest.bytes >> 20,
        written >> 20
    );
    Ok(())
}

/// What the backup in `fd` (open for reading) holds, for the page to ask before restoring it.
pub fn inspect(fd: i32) -> Value {
    if fd < 0 {
        return json!({ "error": "Android didn't open the file" });
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let manifest = match backup::read_manifest(file) {
        Ok(manifest) => manifest,
        Err(error) => {
            return json!({ "error": format!("Not a backup Local Desktop can read: {error}") })
        }
    };
    *INSPECTED.lock().unwrap_or_else(|it| it.into_inner()) = manifest.clone();
    json!({ "manifest": manifest, "free_bytes": free_bytes() })
}

/// The space left where the app keeps its files.
fn free_bytes() -> u64 {
    let folder = Path::new(ARCH_FS_ROOT).parent().unwrap_or(Path::new("/"));
    let Ok(path) = std::ffi::CString::new(folder.as_os_str().as_bytes()) else {
        return 0;
    };
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return 0;
    }
    stats.f_bavail as u64 * stats.f_frsize as u64
}

/// Restore the backup in `fd` (open for reading) beside the current system.
pub fn start_restore(fd: i32) -> Value {
    if fd < 0 {
        return json!({ "error": "Android didn't open the file" });
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let manifest = INSPECTED
        .lock()
        .unwrap_or_else(|it| it.into_inner())
        .clone();
    // The files, their folders and records, with some room to spare.
    let needed = manifest
        .as_ref()
        .map_or(0, |it| it.bytes + it.bytes / 10 + (256 << 20));
    let free = free_bytes();
    if needed > free {
        return json!({ "error": format!(
            "Not enough space: the restore needs about {} MB, the phone has {} MB free",
            needed >> 20,
            free >> 20
        ) });
    }
    if let Err(error) = begin("restore", "Restoring") {
        return json!({ "error": error });
    }
    update(|job| {
        if let Some(manifest) = &manifest {
            job.total = Progress {
                files: manifest.files,
                bytes: manifest.bytes,
            };
        }
    });
    thread::spawn(move || {
        let error = restore(file).err();
        if let Some(error) = &error {
            log::warn!("Restore: failed: {error}");
        }
        end(error);
    });
    json!({ "ok": true })
}

fn restore(file: File) -> Result<(), String> {
    let staging = restoring();
    for leftover in [&staging, &restored()] {
        backup::remove_tree(leftover).map_err(|error| error.to_string())?;
    }
    let _ = fs::remove_file(restored_description());
    log::info!("Restore: started");
    let result = backup::restore(
        io::BufReader::with_capacity(1 << 20, file),
        &staging,
        Path::new(ARCH_FS_ROOT),
        &mut |done| update(|job| job.done = done),
    );
    let restored_system = match result {
        Ok(it) => it,
        Err(error) => {
            let _ = backup::remove_tree(&staging);
            return Err(error.to_string());
        }
    };
    if !backup::looks_like_a_system(&staging) {
        let _ = backup::remove_tree(&staging);
        return Err("The archive holds no Linux system (no /etc/passwd or /usr/bin)".into());
    }
    let description = json!({
        "manifest": restored_system.manifest,
        "files": restored_system.progress.files,
        "bytes": restored_system.progress.bytes,
        "left_out": restored_system.left_out,
    });
    fs::write(restored_description(), description.to_string()).map_err(|it| it.to_string())?;
    fs::rename(&staging, restored()).map_err(|it| it.to_string())?;
    log::info!(
        "Restore: {} entries, {} MB, {} left out; it replaces the system at the next start",
        restored_system.progress.files,
        restored_system.progress.bytes >> 20,
        restored_system.left_out
    );
    Ok(())
}

fn busy() -> bool {
    JOB.lock()
        .unwrap_or_else(|it| it.into_inner())
        .as_ref()
        .is_some_and(|it| !it.finished)
}

/// Whether a restored system waits to take the current one's place.
pub fn restored_ready() -> bool {
    !busy() && restored().is_dir()
}

/// Forget a restored system that waits for the next start.
pub fn discard_restored() -> Value {
    if busy() {
        return json!({ "error": "A backup or a restore is still going on" });
    }
    let _ = fs::remove_file(restored_description());
    let waiting = restored();
    // Out of the way at once, deleted in the background.
    let doomed = beside("arch.discarded");
    if fs::rename(&waiting, &doomed).is_ok() {
        thread::spawn(move || {
            let _ = backup::remove_tree(&doomed);
        });
    }
    log::info!("Restore: the restored system was discarded");
    json!({ "ok": true })
}

/// Put a restored system in the current one's place, before setup looks at it, and delete the
/// one it replaces in the background. True if it did: the config changed with the system.
pub fn swap_in_restored() -> bool {
    // The settings page can start the app while it restores, in the same process.
    if busy() {
        return false;
    }
    let root = Path::new(ARCH_FS_ROOT);
    let waiting = restored();
    let swapped = waiting.is_dir() && {
        let _ = backup::remove_tree(&replaced());
        let moved_away = !root.exists() || fs::rename(root, replaced()).is_ok();
        moved_away && fs::rename(&waiting, root).is_ok()
    };
    if swapped {
        let _ = fs::remove_file(restored_description());
        log::info!("Restore: the restored system replaced the previous one");
    }
    // A restore cut short, a discarded system, the one replaced: none is needed any more.
    let leftovers: Vec<PathBuf> = [restoring(), replaced(), beside("arch.discarded")]
        .into_iter()
        .filter(|it| it.exists())
        .collect();
    if !leftovers.is_empty() {
        thread::spawn(move || {
            for leftover in leftovers {
                if let Err(error) = backup::remove_tree(&leftover) {
                    log::warn!("Restore: couldn't delete {}: {error}", leftover.display());
                }
            }
        });
    }
    swapped
}
