//! Newer GPU drivers for the Adreno: a look for a newer release of Mesa for Adreno once a week
//! while `[graphics] driver_updates` is on, and whenever the settings page asks. A newer one is
//! noted in the rootfs, and setup installs it the next time the app starts (`setup_adreno_mesa`);
//! if it doesn't work here, setup goes back to the release before it and doesn't offer it again.

use crate::android::proot::setup::ADRENO_MESA_STATE;
use crate::core::config::{LocalConfig, ARCH_FS_ROOT};
use crate::core::gpu_drivers;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where the releases are listed.
const RELEASES: &str =
    "https://api.github.com/repos/lfdevs/mesa-for-android-container/releases?per_page=30";
/// The release installed.
const RELEASE: &str = "var/lib/localdesktop/adreno-mesa.release";
/// When the last look for a newer release was, in seconds since 1970.
const CHECKED: &str = "var/lib/localdesktop/adreno-mesa.checked";
/// A newer release, for setup to install.
const UPDATE: &str = "var/lib/localdesktop/adreno-mesa.update";
/// A release that didn't work here, which isn't offered again.
const SKIP: &str = "var/lib/localdesktop/adreno-mesa.skip";

static CHECKING: AtomicBool = AtomicBool::new(false);
static CHECK_FAILED: Mutex<Option<String>> = Mutex::new(None);

fn path(name: &str) -> PathBuf {
    Path::new(ARCH_FS_ROOT).join(name)
}

fn read(name: &str) -> Option<String> {
    let text = fs::read_to_string(path(name)).ok()?;
    Some(text.trim().to_string()).filter(|it| !it.is_empty())
}

fn write(name: &str, value: &str) {
    let path = path(name);
    if let Some(directory) = path.parent() {
        let _ = fs::create_dir_all(directory);
    }
    if let Err(error) = fs::write(&path, value) {
        log::warn!("GPU drivers: couldn't write {}: {error}", path.display());
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |it| it.as_secs())
}

/// The GitHub listing of the releases.
pub fn releases() -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Local Desktop")
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())?;
    let text = client
        .get(RELEASES)
        .send()
        .and_then(|it| it.error_for_status())
        .and_then(|it| it.text())
        .map_err(|error| format!("couldn't list the releases: {error}"))?;
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

/// The release installed, if setup noted it (it does since driver updates came).
pub fn installed() -> Option<String> {
    read(RELEASE)
}

/// What setup installed, so that a look for a newer release knows.
pub fn note_installed(tag: &str) {
    write(RELEASE, tag);
    let _ = fs::remove_file(path(UPDATE));
}

/// The newer release setup is to install, if there is one.
pub fn pending_update() -> Option<String> {
    read(UPDATE)
}

/// A release that didn't work here, if one didn't.
pub fn skipped() -> Option<String> {
    read(SKIP)
}

/// A release that didn't work here: not again.
pub fn skip(tag: &str) {
    write(SKIP, tag);
    let _ = fs::remove_file(path(UPDATE));
}

/// Look for a newer release now, and note it for setup. The newer release, if there is one.
pub fn check() -> Result<Option<String>, String> {
    let releases = releases()?;
    let newest = gpu_drivers::release(&releases, None, skipped().as_deref())
        .ok_or("no release for Arch Linux found")?;
    write(CHECKED, &unix_time().to_string());
    let installed_at = fs::metadata(path(ADRENO_MESA_STATE))
        .and_then(|it| it.modified())
        .ok()
        .and_then(|it| it.duration_since(UNIX_EPOCH).ok())
        .map(|it| it.as_secs());
    // Nothing installed to update: setup installs the newest anyway.
    let Some(installed_at) = installed_at else {
        return Ok(None);
    };
    let installed = installed();
    if gpu_drivers::is_newer(&newest, installed.as_deref(), Some(installed_at)) {
        log::info!(
            "GPU drivers: {} is newer than {}; it installs the next time Local Desktop starts",
            newest.tag,
            installed.as_deref().unwrap_or("the drivers installed")
        );
        write(UPDATE, &newest.tag);
        Ok(Some(newest.tag))
    } else {
        log::info!("GPU drivers: {} is the newest", newest.tag);
        Ok(None)
    }
}

/// `check` on a thread of its own, for the settings page, which asks for `status` meanwhile.
pub fn check_in_background() {
    if CHECKING.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::spawn(|| {
        let failed = check().err();
        if let Some(error) = &failed {
            log::info!("GPU drivers: {error}");
        }
        *CHECK_FAILED.lock().unwrap_or_else(|it| it.into_inner()) = failed;
        CHECKING.store(false, Ordering::Release);
    });
}

/// `check`, at a desktop launch, if `[graphics] driver_updates` wants it and a week has passed.
pub fn check_if_due(config: &LocalConfig) {
    let installed = path(ADRENO_MESA_STATE).exists();
    if !config.graphics.adreno_drivers || !config.graphics.driver_updates || !installed {
        return;
    }
    let due = || gpu_drivers::due(read(CHECKED).and_then(|it| it.parse().ok()), unix_time());
    if !due() {
        return;
    }
    thread::spawn(move || {
        // The desktop's start gets the network and the CPU first.
        thread::sleep(Duration::from_secs(60));
        // Unless the settings page looked meanwhile.
        if !due() {
            return;
        }
        if let Err(error) = check() {
            log::info!("GPU drivers: {error}; trying again at the next start");
        }
    });
}

/// What the settings page shows.
pub fn status() -> Value {
    let installed_at = fs::metadata(path(ADRENO_MESA_STATE))
        .and_then(|it| it.modified())
        .ok()
        .and_then(|it| it.duration_since(UNIX_EPOCH).ok())
        .map(|it| it.as_secs());
    json!({
        "installed": installed_at.is_some(),
        "release": installed(),
        "installed_at": installed_at,
        "checked": read(CHECKED).and_then(|it| it.parse::<u64>().ok()),
        "update": pending_update(),
        "checking": CHECKING.load(Ordering::Acquire),
        "failed": CHECK_FAILED.lock().unwrap_or_else(|it| it.into_inner()).clone(),
    })
}
