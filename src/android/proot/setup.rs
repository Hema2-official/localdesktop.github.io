use super::process::ArchProcess;
use crate::{
    android::{
        app::build::PolarBearBackend,
        backend::{
            wayland::{Compositor, TouchMode, WaylandBackend},
            webview::{ErrorVariant, WebviewBackend},
        },
        session,
        utils::application_context::{get_application_context, reload_local_config},
        utils::ndk::{density_dpi, long_press_timeout_ms, scale_factor, time_zone, touch_slop_px},
    },
    core::{
        config::{
            CommandConfig, DesktopPreset, ARCH_FS_ARCHIVE, ARCH_FS_ROOT, CONFIG_FILE,
            DOCS_HOME_URL, PIPEWIRE_GUEST_RUNTIME_DIR, PULSE_GUEST_SERVER,
        },
        hard_links,
    },
};
use pathdiff::diff_paths;
use sha2::{Digest, Sha256};
use smithay::utils::Clock;
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process,
    sync::{
        mpsc::{self, Receiver, Sender, TryRecvError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tar::Archive;
use winit::platform::android::activity::AndroidApp;
use xz2::read::XzDecoder;

#[derive(Debug)]
pub enum SetupMessage {
    Progress(String),
    Error(String),
    /// A stage failed and setup has stopped; the app has to be restarted.
    Failed(String),
    /// Ask the setup page which desktop preset to install; it answers through `desktop_choice`.
    ChooseDesktop,
}

pub struct SetupOptions {
    pub android_app: AndroidApp,
    pub mpsc_sender: Sender<SetupMessage>,
    /// Set on a fresh install, where the setup page asks which desktop to install.
    pub desktop_choice: Option<Arc<Mutex<Receiver<String>>>>,
}

/// Setup is a process that should be done **only once** when the user installed the app.
/// The setup process consists of several stages.
/// Each stage is a function that takes the `SetupOptions` and returns a `StageOutput`.
type SetupStage = Box<dyn Fn(&SetupOptions) -> StageOutput + Send>;

/// Each stage should indicate whether the associated task is done previously or not.
/// Thus, it should return a finished status if the task is done, so that the setup process can move on to the next stage.
/// Otherwise, it should return a `JoinHandle`, so that the setup process can wait for the task to finish, but not block the main thread so that the setup progress can be reported to the user.
///
/// For coding agents: READ THIS BEFORE ADDING WORK HERE.
/// - Heavy/long work belongs inside the spawned thread of a returned `Some(JoinHandle)`, so it runs once at install and surfaces as setup progress.
/// - Simple/light tasks or important settings that must be run every launch (e.g. the Firefox config) can be done inline on the `None` path.
pub type StageOutput = Option<JoinHandle<()>>;

const PIPEWIRE_GUEST_LOCK_PACKAGES: &[&str] = &[
    "libpipewire",
    "pipewire",
    "pipewire-alsa",
    "pipewire-audio",
    "pipewire-jack",
    "pipewire-pulse",
    "pipewire-v4l2",
    "pipewire-zeroconf",
    "gst-plugin-pipewire",
    "wireplumber",
];

fn setup_arch_fs(options: &SetupOptions) -> StageOutput {
    let context = get_application_context();
    let temp_file = context.data_dir.join("archlinux-fs.tar.xz");
    let fs_root = Path::new(ARCH_FS_ROOT);
    let extracted_dir = context.data_dir.join("archlinux-aarch64");
    let mpsc_sender = options.mpsc_sender.clone();

    // Only run if the fs_root is missing or empty
    // TODO: Setup integration test to make sure on clean install, the fs_root is either non existent or empty
    let need_setup = fs_root.read_dir().map_or(true, |mut d| d.next().is_none());
    if need_setup {
        return Some(thread::spawn(move || {
            // Download if the archive doesn't exist
            loop {
                if !temp_file.exists() {
                    mpsc_sender
                        .send(SetupMessage::Progress(
                            "Downloading Arch Linux FS...".to_string(),
                        ))
                        .expect("Failed to send log message");
                    download(ARCH_FS_ARCHIVE, &temp_file, &mpsc_sender, "Downloading Arch Linux FS");
                }

                mpsc_sender
                    .send(SetupMessage::Progress(
                        "Extracting Arch Linux FS...".to_string(),
                    ))
                    .expect("Failed to send log message");

                // Ensure the extracted directory is clean
                let _ = fs::remove_dir_all(&extracted_dir);

                // Extract tar file directly to the final destination
                let tar_file =
                    File::open(&temp_file).expect("Failed to open downloaded Arch Linux FS file");
                let tar = XzDecoder::new(tar_file);
                let mut archive = Archive::new(tar);

                // Try to extract, if it fails, remove temp file and restart download
                if let Err(e) = archive.unpack(context.data_dir.clone()) {
                    // Clean up the failed extraction
                    let _ = fs::remove_dir_all(&extracted_dir);
                    let _ = fs::remove_file(&temp_file);

                    mpsc_sender
                        .send(SetupMessage::Error(format!(
                            "Failed to extract Arch Linux FS: {}. Restarting download...",
                            e
                        )))
                        .unwrap_or(());

                    // Continue the outer loop to retry the download
                    continue;
                }

                // If we get here, extraction was successful
                break;
            }

            // Move the extracted files to the final destination
            fs::rename(&extracted_dir, fs_root)
                .expect("Failed to rename extracted files to final destination");

            // Clean up the temporary file
            fs::remove_file(&temp_file).expect("Failed to remove temporary file");
        }));
    }
    None
}

/// Download `url` to `path`, resuming after network errors (the phone dozing, Wi-Fi dropping)
/// instead of failing the setup. `path` only appears once the download is complete.
fn download(url: &str, path: &Path, sender: &Sender<SetupMessage>, label: &str) {
    let part = PathBuf::from(format!("{}.part", path.display()));
    let client = reqwest::blocking::Client::new();
    for failures in 1u64.. {
        match download_attempt(&client, url, &part, sender, label) {
            Ok(()) => {
                fs::rename(&part, path).expect("Failed to move the finished download into place");
                return;
            }
            Err(error) => {
                let wait = (5 * failures).min(60);
                log::warn!("Download of {url} failed: {error}");
                sender
                    .send(SetupMessage::Progress(format!(
                        "Download interrupted ({error}), retrying in {wait} s..."
                    )))
                    .unwrap_or(());
                thread::sleep(Duration::from_secs(wait));
            }
        }
    }
}

/// One try, continuing from what `part` already holds when the server allows ranges.
fn download_attempt(
    client: &reqwest::blocking::Client,
    url: &str,
    part: &Path,
    sender: &Sender<SetupMessage>,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let have = fs::metadata(part).map(|it| it.len()).unwrap_or(0);
    let mut request = client.get(url);
    if have > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut response = request.send()?;
    let status = response.status();
    let (mut file, mut downloaded, total) = if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let total = response.content_length().map(|it| it + have);
        (OpenOptions::new().append(true).open(part)?, have, total)
    } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        // Nothing left to fetch; a broken file fails extraction and gets downloaded again.
        return Ok(());
    } else if status.is_success() {
        (File::create(part)?, 0, response.content_length())
    } else {
        return Err(format!("HTTP {status}").into());
    };

    let mut buffer = [0u8; 65536];
    let mut last_percent = None;
    loop {
        let n = response.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
        downloaded += n as u64;
        if let Some(total) = total.filter(|it| *it > 0) {
            let percent = (downloaded * 100 / total).min(100);
            if last_percent != Some(percent) {
                sender
                    .send(SetupMessage::Progress(format!(
                        "{label}... {}% ({:.2} MB / {:.2} MB)",
                        percent,
                        downloaded as f64 / 1024.0 / 1024.0,
                        total as f64 / 1024.0 / 1024.0
                    )))
                    .unwrap_or(());
                last_percent = Some(percent);
            }
        }
    }
    match total {
        Some(total) if downloaded < total => Err("the connection closed early".into()),
        _ => Ok(()),
    }
}

/// The empty directory proot binds over `/sys/fs/selinux`. The rootfs ships `/sys` read-only
/// (0555), so it has to become writable first; earlier setups skipped that and never created it.
fn ensure_empty_sys_dir(fs_root: &Path) {
    let empty = fs_root.join("sys/.empty");
    if empty.exists() {
        return;
    }
    let _ = fs::create_dir_all(fs_root.join("sys"));
    let _ = fs::set_permissions(fs_root.join("sys"), fs::Permissions::from_mode(0o700));
    if let Err(error) = fs::create_dir_all(&empty) {
        log::warn!("Failed to create {}: {error}", empty.display());
    }
    let _ = fs::set_permissions(&empty, fs::Permissions::from_mode(0o700));
}

fn simulate_linux_sysdata_stage(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let mpsc_sender = options.mpsc_sender.clone();

    ensure_empty_sys_dir(fs_root);
    if !fs_root.join("proc/.version").exists() {
        return Some(thread::spawn(move || {
            mpsc_sender
                .send(SetupMessage::Progress(
                    "Simulating Linux system data...".to_string(),
                ))
                .expect(&format!("Failed to send log message"));

            // Create necessary directories - don't fail if they already exist
            let _ = fs::create_dir_all(fs_root.join("proc"));
            // Try to set permissions, but don't fail if we can't
            let _ = fs::set_permissions(fs_root.join("proc"), fs::Permissions::from_mode(0o700));

            // Create fake proc files
            let proc_files = [
                    ("proc/.loadavg", "0.12 0.07 0.02 2/165 765\n"),
                    ("proc/.stat", "cpu  1957 0 2877 93280 262 342 254 87 0 0\ncpu0 31 0 226 12027 82 10 4 9 0 0\n"),
                    ("proc/.uptime", "124.08 932.80\n"),
                    ("proc/.version", "Linux version 6.2.1 (proot@termux) (gcc (GCC) 12.2.1 20230201, GNU ld (GNU Binutils) 2.40) #1 SMP PREEMPT_DYNAMIC Wed, 01 Mar 2023 00:00:00 +0000\n"),
                    ("proc/.vmstat", "nr_free_pages 1743136\nnr_zone_inactive_anon 179281\nnr_zone_active_anon 7183\n"),
                    ("proc/.sysctl_entry_cap_last_cap", "40\n"),
                    ("proc/.sysctl_inotify_max_user_watches", "4096\n"),
                ];

            for (path, content) in proc_files {
                let _ = fs::write(fs_root.join(path), content)
                    .expect(&format!("Permission denied while writing to {}", path));
            }
        }));
    }
    None
}

fn setup_machine_id(_: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let machine_id = fs_root.join("etc/machine-id");

    let existing = fs::read_to_string(&machine_id).unwrap_or_default();
    if !is_valid_machine_id(&existing) {
        if let Some(parent) = machine_id.parent() {
            fs::create_dir_all(parent).expect("Failed to create /etc for machine-id");
        }

        let _ = fs::set_permissions(&machine_id, fs::Permissions::from_mode(0o644));
        fs::write(&machine_id, format!("{}\n", generate_machine_id()))
            .expect("Failed to write machine-id");
        let _ = fs::set_permissions(&machine_id, fs::Permissions::from_mode(0o444));
        log::info!("Seeded guest /etc/machine-id");
    }

    let dbus_dir = fs_root.join("var/lib/dbus");
    fs::create_dir_all(&dbus_dir).expect("Failed to create /var/lib/dbus");
    let dbus_machine_id = dbus_dir.join("machine-id");
    match fs::symlink_metadata(&dbus_machine_id) {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {
            symlink("/etc/machine-id", &dbus_machine_id)
                .expect("Failed to symlink /var/lib/dbus/machine-id");
        }
        Err(err) => panic!("Failed to inspect /var/lib/dbus/machine-id: {}", err),
    }

    None
}

/// Follow Android's time zone, so Linux clocks show the phone's time. A relative link, like the
/// xkb one below, so it also resolves outside proot.
/// Sets `TZ` from the `/etc/localtime` link. Without it glibc stats `/etc/localtime` on every
/// `localtime()` call and Qt looks for `/etc/timezone` and `/etc/TZ`, and under proot each of those
/// is a traced syscall: Plasma's clock alone made about 15 a second, `ls -l` one per file.
const TZ_FROM_LOCALTIME: &str = r#"if [ -z "${TZ:-}" ] && zone=$(readlink /etc/localtime 2>/dev/null); then
    case "$zone" in */zoneinfo/*) export TZ="${zone#*/zoneinfo/}" ;; esac
fi
"#;

fn setup_time_zone(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    // For login shells (the terminal, SSH); the desktop sessions get it from `session_environment`.
    let profile = fs_root.join("etc/profile.d/localdesktop-tz.sh");
    if fs::read_to_string(&profile).ok().as_deref() != Some(TZ_FROM_LOCALTIME) {
        if let Err(error) = fs::write(&profile, TZ_FROM_LOCALTIME) {
            log::warn!("Failed to write {}: {error}", profile.display());
        }
    }

    let Some(zone) = time_zone(&options.android_app) else {
        return None;
    };
    if zone.split('/').any(|part| part.is_empty() || part == "..")
        || !fs_root.join("usr/share/zoneinfo").join(&zone).is_file()
    {
        log::warn!("No zone file for Android's time zone {zone}");
        return None;
    }
    let target = Path::new("../usr/share/zoneinfo").join(&zone);
    let localtime = fs_root.join("etc/localtime");
    if fs::read_link(&localtime).is_ok_and(|it| it == target) {
        return None;
    }
    let _ = fs::remove_file(&localtime);
    match symlink(&target, &localtime) {
        Ok(()) => log::info!("Set the guest's time zone to {zone}"),
        Err(error) => log::warn!("Failed to set the guest's time zone to {zone}: {error}"),
    }
    None
}

fn is_valid_machine_id(value: &str) -> bool {
    let value = value.trim();
    value.len() == 32
        && value.chars().all(|c| c.is_ascii_hexdigit())
        && value.chars().any(|c| c != '0')
}

fn generate_machine_id() -> String {
    if let Ok(uuid) = fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let id = uuid.trim().replace('-', "").to_ascii_lowercase();
        if is_valid_machine_id(&id) {
            return id;
        }
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{:016x}{:016x}", nanos as u64, process::id() as u64)
}

/// On a fresh install the setup page asks which desktop to install while the rootfs downloads;
/// write the answer to the config before `install_dependencies` reads it.
fn apply_desktop_choice(options: &SetupOptions) -> StageOutput {
    let receiver = options.desktop_choice.clone()?;
    let config_path = Path::new(ARCH_FS_ROOT).join(CONFIG_FILE.trim_start_matches('/'));
    if config_path.exists() {
        return None;
    }

    let mpsc_sender = options.mpsc_sender.clone();
    Some(thread::spawn(move || {
        let receiver = receiver.lock().unwrap();
        let choice = match receiver.try_recv() {
            Ok(choice) => choice,
            Err(TryRecvError::Empty) => {
                mpsc_sender
                    .send(SetupMessage::Progress(
                        "Choose a desktop to continue".to_string(),
                    ))
                    .unwrap_or(());
                receiver.recv().unwrap_or_default()
            }
            Err(TryRecvError::Disconnected) => String::new(),
        };
        let preset = if choice == "plasma" { "plasma" } else { "xfce" };

        fs::create_dir_all(config_path.parent().unwrap())
            .expect("Failed to create the config directory");
        fs::write(
            &config_path,
            format!(
                "# Local Desktop's config: {DOCS_HOME_URL}docs/user/configurations\n\
                 [desktop]\n\
                 # \"xfce\" or \"plasma\". Changing it installs the other desktop on the next start.\n\
                 preset = \"{preset}\"\n"
            ),
        )
        .expect("Failed to write the config");
        reload_local_config();
        log::info!("Desktop preset chosen during setup: {preset}");
    }))
}

fn install_dependencies(options: &SetupOptions) -> StageOutput {
    let SetupOptions { mpsc_sender, .. } = options;

    let context = get_application_context();
    let CommandConfig {
        check,
        install,
        launch: _,
    } = context.local_config.command;

    let installed = move || {
        ArchProcess {
            command: check.clone(),
            user: None,
            log: None,
        }
        .run()
        .status
        .success()
    };

    let mut broken = broken_packages();
    if broken.is_empty() && installed() {
        return None;
    }

    clear_pipewire_package_lock_for_install();

    let mpsc_sender = mpsc_sender.clone();
    return Some(thread::spawn(move || {
        const MAX_INSTALL_ATTEMPTS: usize = 10;

        // Install dependencies until `check` succeeds.
        for attempt in 1..=MAX_INSTALL_ATTEMPTS {
            let output = ArchProcess {
                command: "rm -f /var/lib/pacman/db.lck".into(),
                user: None,
                log: None,
            }
            .run();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if !broken.is_empty() {
                let names: Vec<&str> = broken.iter().map(|(_, name)| name.as_str()).collect();
                let dirs: Vec<&str> = broken.iter().map(|(dir, _)| dir.as_str()).collect();
                log::warn!("Reinstalling packages with broken database entries: {names:?}");
                mpsc_sender
                    .send(SetupMessage::Progress(format!(
                        "Repairing packages an interrupted install left behind: {}",
                        names.join(" ")
                    )))
                    .unwrap_or(());
                // Forget the broken entries, then install the packages again over whatever
                // files they left. Packages the repositories don't have (built from the AUR,
                // say) can't be reinstalled here; they stay unregistered rather than failing
                // the setup on every attempt.
                let sender = mpsc_sender.clone();
                let repaired = ArchProcess {
                    command: format!(
                        "cd /var/lib/pacman/local && rm -rf {} && pacman -Sy --noconfirm || exit 1
                        set --
                        for p in {}; do
                            if pacman -Si \"$p\" >/dev/null 2>&1; then set -- \"$@\" \"$p\"
                            else echo \"$p isn't in the repositories; reinstall it yourself\"; fi
                        done
                        [ $# -eq 0 ] || pacman -S --noconfirm --overwrite '*' \"$@\"",
                        dirs.join(" "),
                        names.join(" ")
                    ),
                    user: None,
                    log: Some(Arc::new(move |it| {
                        sender.send(SetupMessage::Progress(it)).unwrap_or(());
                    })),
                }
                .run()
                .status
                .success();
                if repaired {
                    broken.clear();
                }
            }
            let sender = mpsc_sender.clone();
            ArchProcess {
                command: install.clone(),
                user: None,
                log: Some(Arc::new(move |it| {
                    sender
                        .send(SetupMessage::Progress(it))
                        .expect("Failed to send log message");
                })),
            }
            .run();

            if broken.is_empty() && installed() {
                download_user_manual();
                return;
            }
            if attempt < MAX_INSTALL_ATTEMPTS {
                // Wait a little longer each time, so a network outage doesn't use up every
                // attempt.
                let wait = (10 * attempt as u64).min(60);
                mpsc_sender
                    .send(SetupMessage::Progress(format!(
                        "Retrying installation in {} s... (attempt {}/{})",
                        wait,
                        attempt + 1,
                        MAX_INSTALL_ATTEMPTS
                    )))
                    .expect("Failed to send dependency install progress");
                thread::sleep(Duration::from_secs(wait));
            } else {
                let error_message = format!(
                    "Failed to install desktop dependencies after {} attempts. Please check your net connection and try restarting the app.",
                    MAX_INSTALL_ATTEMPTS
                );
                mpsc_sender
                    .send(SetupMessage::Error(error_message.clone()))
                    .unwrap_or(());
                panic!("{}", error_message);
            }
        }
    }));
}

/// Packages whose entry in pacman's local database lacks `desc` or `files`, as `(directory,
/// name)`. That happens when the app is killed during a transaction. pacman still counts them as
/// installed (by the directory name), so `--needed` never replaces their half-extracted files.
fn broken_packages() -> Vec<(String, String)> {
    let local = Path::new(ARCH_FS_ROOT).join("var/lib/pacman/local");
    let Ok(entries) = fs::read_dir(local) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| !entry.path().join("desc").is_file() || !entry.path().join("files").is_file())
        .filter_map(|entry| {
            let dir = entry.file_name().into_string().ok()?;
            // "<name>-<pkgver>-<pkgrel>"
            let name = dir.rsplitn(3, '-').nth(2)?.to_string();
            Some((dir, name))
        })
        .collect()
}

/// Drop the offline User Manual for this app version onto the guest desktop.
///
/// The filename carries no version so an update overwrites the previous copy instead of landing
/// beside it. Called once a fresh install or update has just succeeded — the only moment the
/// manual on disk can be out of date — and best-effort: a failed download is not worth a retry.
fn download_user_manual() {
    let username = get_application_context().local_config.user.username;
    let desktop_dir = chroot_home_dir(Path::new(ARCH_FS_ROOT), &username).join("Desktop");
    if fs::create_dir_all(&desktop_dir).is_err() {
        return;
    }

    let url = crate::core::config::user_manual_url();
    let response = reqwest::blocking::get(&url).and_then(|it| it.error_for_status());
    if let Ok(bytes) = response.and_then(|it| it.bytes()) {
        let _ = fs::write(desktop_dir.join("Local Desktop - User Manual.pdf"), &bytes);
    }
}

fn clear_pipewire_package_lock_for_install() {
    let pacman_conf = Path::new(ARCH_FS_ROOT).join("etc/pacman.conf");
    let content = match fs::read_to_string(&pacman_conf) {
        Ok(content) => content,
        Err(error) => {
            log::warn!(
                "Skipping PipeWire pacman unlock before install; failed to read {}: {error}",
                pacman_conf.display()
            );
            return;
        }
    };

    let updated = remove_pacman_ignore_pkg(&content, PIPEWIRE_GUEST_LOCK_PACKAGES);
    if updated != content {
        fs::write(&pacman_conf, updated)
            .expect("Failed to clear PipeWire pacman lock before install");
        log::info!("Temporarily cleared guest PipeWire package lock before dependency install");
    }
}

fn setup_pipewire_package_lock(_: &SetupOptions) -> StageOutput {
    let pacman_conf = Path::new(ARCH_FS_ROOT).join("etc/pacman.conf");
    let content = match fs::read_to_string(&pacman_conf) {
        Ok(content) => content,
        Err(error) => {
            log::warn!(
                "Skipping PipeWire pacman lock; failed to read {}: {error}",
                pacman_conf.display()
            );
            return None;
        }
    };

    let updated = ensure_pacman_ignore_pkg(&content, PIPEWIRE_GUEST_LOCK_PACKAGES);
    if updated != content {
        fs::write(&pacman_conf, updated).expect("Failed to write PipeWire pacman lock");
        log::info!(
            "Locked guest PipeWire packages in {}: {}",
            pacman_conf.display(),
            PIPEWIRE_GUEST_LOCK_PACKAGES.join(" ")
        );
    }

    None
}

/// Mesa that drives Adreno GPUs through KGSL, built by
/// https://github.com/lfdevs/mesa-for-android-container. Arch's own Mesa only drives GPUs through
/// `/dev/dri`, which Android doesn't give apps, so with it Turnip (Vulkan) and Freedreno (OpenGL)
/// find no GPU.
const ADRENO_MESA_RELEASES: &str =
    "https://api.github.com/repos/lfdevs/mesa-for-android-container/releases?per_page=30";
/// The packages its Arch release replaces (its `mesa-docs` isn't needed).
const ADRENO_MESA_PACKAGES: &[&str] = &[
    "mesa",
    "vulkan-freedreno",
    "vulkan-mesa-implicit-layers",
    "vulkan-mesa-layers",
];
/// The installed drivers as `<size> <path>` lines and the libraries they link as `- <path>`, so
/// that each launch can tell with a few `stat`s whether an update replaced the drivers or removed
/// a library they need (an LLVM update removing their `libLLVM`, say). Updated libraries keep
/// their name and work on.
const ADRENO_MESA_STATE: &str = "var/lib/localdesktop/adreno-mesa";
/// When an install last failed; it isn't tried again for a day, since every try brings up the
/// setup page (and without network it can't succeed).
const ADRENO_MESA_FAILED: &str = "var/lib/localdesktop/adreno-mesa.failed";
const ADRENO_MESA_RETRY_SECS: u64 = 24 * 60 * 60;
/// `gpu <program>` runs an OpenGL program on the Adreno. Vulkan programs use it without help.
const GPU_HELPER: &str = "usr/local/bin/gpu";
const GPU_HELPER_SCRIPT: &str = r#"#!/bin/sh
# Run an OpenGL program on the Adreno GPU: through Zink on Turnip, under X11 (Xwayland), because
# the compositors here only take shared-memory buffers and OpenGL through Wayland can't use them.
# Vulkan programs use the GPU without this.
[ $# -gt 0 ] || { echo "Usage: gpu <program> [arguments]" >&2; exit 2; }
export MESA_LOADER_DRIVER_OVERRIDE=zink LIBGL_KOPPER_DRI2=1
export DISPLAY="${DISPLAY:-:0}"
unset WAYLAND_DISPLAY
export QT_QPA_PLATFORM=xcb GDK_BACKEND=x11 SDL_VIDEODRIVER=x11 MOZ_ENABLE_WAYLAND=0
exec "$@"
"#;

fn setup_adreno_mesa(options: &SetupOptions) -> StageOutput {
    if !Path::new("/dev/kgsl-3d0").exists() {
        return None;
    }
    let fs_root = Path::new(ARCH_FS_ROOT);
    let wanted = get_application_context().local_config.graphics.adreno_drivers;
    let installed = fs_root.join(ADRENO_MESA_STATE).exists();
    if wanted == installed && (!wanted || adreno_mesa_intact(fs_root)) {
        let helper = fs_root.join(GPU_HELPER);
        if wanted && fs::read_to_string(&helper).ok().as_deref() != Some(GPU_HELPER_SCRIPT) {
            write_executable(&helper, GPU_HELPER_SCRIPT);
        }
        return None;
    }
    let failed_at = fs::read_to_string(fs_root.join(ADRENO_MESA_FAILED))
        .ok()
        .and_then(|it| it.trim().parse::<u64>().ok());
    if failed_at.is_some_and(|it| unix_time().saturating_sub(it) < ADRENO_MESA_RETRY_SECS) {
        return None;
    }

    let sender = options.mpsc_sender.clone();
    Some(thread::spawn(move || {
        let result = if wanted {
            install_adreno_mesa(&sender)
        } else {
            restore_arch_mesa(&sender)
        };
        let failed = fs_root.join(ADRENO_MESA_FAILED);
        match result {
            Ok(()) => {
                let _ = fs::remove_file(failed);
            }
            // Not worth failing the setup over: the desktop works without the GPU.
            Err(error) => {
                log::warn!("GPU drivers: {error}");
                sender
                    .send(SetupMessage::Error(format!(
                        "GPU drivers: {error}. Trying again tomorrow; the desktop works without them."
                    )))
                    .unwrap_or(());
                let _ = fs::create_dir_all(fs_root.join("var/lib/localdesktop"));
                let _ = fs::write(failed, unix_time().to_string());
            }
        }
    }))
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |it| it.as_secs())
}

/// Whether the drivers recorded at install time are still there with the same size, and the
/// libraries they link are still there.
fn adreno_mesa_intact(fs_root: &Path) -> bool {
    let Ok(state) = fs::read_to_string(fs_root.join(ADRENO_MESA_STATE)) else {
        return false;
    };
    let mut files = 0;
    for line in state.lines() {
        let Some((size, path)) = line.split_once(' ') else {
            continue;
        };
        let host_path = fs_root.join(path.trim_start_matches('/'));
        let found = fs::metadata(&host_path).map(|it| it.len().to_string()).ok();
        if found.is_none() || (size != "-" && found.as_deref() != Some(size)) {
            log::info!("GPU drivers changed or broke: {path}");
            return false;
        }
        files += 1;
    }
    files > 0
}

fn install_adreno_mesa(sender: &Sender<SetupMessage>) -> Result<(), String> {
    let fs_root = Path::new(ARCH_FS_ROOT);
    sender
        .send(SetupMessage::Progress(
            "Installing GPU drivers for the Adreno (Mesa for Android containers)...".into(),
        ))
        .unwrap_or(());

    let client = reqwest::blocking::Client::builder()
        .user_agent("Local Desktop")
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())?;
    let releases: serde_json::Value = client
        .get(ADRENO_MESA_RELEASES)
        .send()
        .and_then(|it| it.error_for_status())
        .and_then(|it| it.text())
        .map_err(|error| format!("couldn't list the releases: {error}"))
        .and_then(|it| serde_json::from_str(&it).map_err(|error| error.to_string()))?;
    // The newest regular release; the `turnip-` ones only carry the Vulkan driver.
    let (tag, name, url, sha256) = releases
        .as_array()
        .into_iter()
        .flatten()
        .filter(|release| release["draft"] == false && release["prerelease"] == false)
        .filter(|release| {
            release["tag_name"]
                .as_str()
                .is_some_and(|tag| tag.starts_with("mesa-"))
        })
        .find_map(|release| {
            let asset = release["assets"].as_array()?.iter().find(|asset| {
                asset["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with("_archlinux_arm64.tar"))
            })?;
            let name = asset["name"].as_str()?.to_string();
            // GitHub's own digest, or the checksum list in the release notes.
            let sha256 = asset["digest"]
                .as_str()
                .and_then(|it| it.strip_prefix("sha256:"))
                .map(str::to_string)
                .or_else(|| {
                    release["body"].as_str()?.lines().find_map(|line| {
                        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
                        (file.trim() == name).then(|| hash.to_string())
                    })
                })?;
            Some((
                release["tag_name"].as_str()?.to_string(),
                name,
                asset["browser_download_url"].as_str()?.to_string(),
                sha256.to_lowercase(),
            ))
        })
        .ok_or("no release for Arch Linux found")?;
    log::info!("Installing Mesa for Adreno from release {tag}");

    let dir = fs_root.join("tmp/localdesktop-adreno-mesa");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let archive = dir.join(&name);
    download(&url, &archive, sender, "Downloading GPU drivers");

    let mut hasher = Sha256::new();
    let mut file = File::open(&archive).map_err(|error| error.to_string())?;
    std::io::copy(&mut file, &mut hasher).map_err(|error| error.to_string())?;
    if format!("{:x}", hasher.finalize()) != sha256 {
        let _ = fs::remove_dir_all(&dir);
        return Err(format!("{name} doesn't match its checksum"));
    }

    // The archive holds one makepkg package per Mesa package.
    let mut packages = Vec::new();
    let mut tar = Archive::new(File::open(&archive).map_err(|error| error.to_string())?);
    for entry in tar.entries().map_err(|error| error.to_string())? {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path().map_err(|error| error.to_string())?.into_owned();
        let Some(file_name) = path.file_name().and_then(|it| it.to_str()).map(str::to_string)
        else {
            continue;
        };
        // "<name>-<version>...", where versions start with a digit (so mesa-docs isn't mesa).
        let wanted = ADRENO_MESA_PACKAGES.iter().any(|package| {
            file_name
                .strip_prefix(package)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        });
        if wanted && file_name.contains(".pkg.tar") {
            entry
                .unpack(dir.join(&file_name))
                .map_err(|error| error.to_string())?;
            packages.push(format!("'{file_name}'"));
        }
    }
    if packages.len() != ADRENO_MESA_PACKAGES.len() {
        let _ = fs::remove_dir_all(&dir);
        return Err(format!("{name} lacks some of {ADRENO_MESA_PACKAGES:?}"));
    }

    let log_sender = sender.clone();
    let installed = ArchProcess {
        command: format!(
            "rm -f /var/lib/pacman/db.lck && cd /tmp/localdesktop-adreno-mesa && pacman -U --noconfirm {} \
             && {{ pacman -S --needed --noconfirm vulkan-tools || pacman -Sy --needed --noconfirm vulkan-tools; }}",
            packages.join(" ")
        ),
        user: None,
        log: Some(Arc::new(move |it| {
            log_sender.send(SetupMessage::Progress(it)).unwrap_or(());
        })),
    }
    .run()
    .status
    .success();
    let _ = fs::remove_dir_all(&dir);
    if !installed {
        return Err("pacman couldn't install them".into());
    }

    // Everything the drivers load has to resolve, and Turnip has to find the GPU.
    let check = ArchProcess {
        command: "for f in /usr/lib/libvulkan_freedreno.so /usr/lib/libgallium-*.so; do \
                      echo \"$f => $f (\"; ldd \"$f\"; done; \
                  vulkaninfo --summary 2>/dev/null | grep -q 'driverName *= turnip' && echo TURNIP_OK"
            .into(),
        user: None,
        log: None,
    }
    .run();
    let output = String::from_utf8_lossy(&check.stdout);
    let broken = output.contains("not found") || !output.contains("TURNIP_OK");
    if broken {
        log::warn!("Mesa for Adreno doesn't work here:\n{output}");
        restore_arch_mesa(sender)?;
        return Err(format!(
            "the {tag} build doesn't work with this system's libraries, so Arch's Mesa is back"
        ));
    }
    let mut state = String::new();
    let mut paths: Vec<&str> = output
        .lines()
        .filter_map(|line| line.split_once("=> ")?.1.split(" (").next())
        .filter(|path| path.starts_with('/'))
        .collect();
    paths.sort();
    paths.dedup();
    for path in paths {
        let driver =
            path == "/usr/lib/libvulkan_freedreno.so" || path.starts_with("/usr/lib/libgallium-");
        match fs::metadata(fs_root.join(path.trim_start_matches('/'))) {
            Ok(metadata) if driver => state.push_str(&format!("{} {path}\n", metadata.len())),
            Ok(_) => state.push_str(&format!("- {path}\n")),
            Err(_) => {}
        }
    }
    fs::create_dir_all(fs_root.join("var/lib/localdesktop")).map_err(|error| error.to_string())?;
    fs::write(fs_root.join(ADRENO_MESA_STATE), state).map_err(|error| error.to_string())?;

    write_executable(&fs_root.join(GPU_HELPER), GPU_HELPER_SCRIPT);

    // Keep `pacman -Syu` from putting Arch's Mesa back.
    let pacman_conf = fs_root.join("etc/pacman.conf");
    if let Ok(content) = fs::read_to_string(&pacman_conf) {
        let updated = ensure_pacman_ignore_pkg(&content, ADRENO_MESA_PACKAGES);
        if updated != content {
            fs::write(&pacman_conf, updated).map_err(|error| error.to_string())?;
        }
    }
    sender
        .send(SetupMessage::Progress(format!(
            "GPU drivers installed ({tag})"
        )))
        .unwrap_or(());
    Ok(())
}

/// Put Arch's own Mesa back, and let `pacman -Syu` update it again.
fn restore_arch_mesa(sender: &Sender<SetupMessage>) -> Result<(), String> {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let pacman_conf = fs_root.join("etc/pacman.conf");
    if let Ok(content) = fs::read_to_string(&pacman_conf) {
        let updated = remove_pacman_ignore_pkg(&content, ADRENO_MESA_PACKAGES);
        if updated != content {
            fs::write(&pacman_conf, updated).map_err(|error| error.to_string())?;
        }
    }
    let log_sender = sender.clone();
    let restored = ArchProcess {
        command: format!(
            "rm -f /var/lib/pacman/db.lck && pacman -Sy --noconfirm {}",
            ADRENO_MESA_PACKAGES.join(" ")
        ),
        user: None,
        log: Some(Arc::new(move |it| {
            log_sender.send(SetupMessage::Progress(it)).unwrap_or(());
        })),
    }
    .run()
    .status
    .success();
    if !restored {
        return Err("pacman couldn't reinstall Arch's Mesa".into());
    }
    let _ = fs::remove_file(fs_root.join(ADRENO_MESA_STATE));
    let _ = fs::remove_file(fs_root.join(GPU_HELPER));
    Ok(())
}

fn setup_firefox_config(_: &SetupOptions) -> StageOutput {
    // Create the Firefox root directory if it doesn't exist
    let firefox_root = format!("{}/usr/lib/firefox", ARCH_FS_ROOT);
    let _ = fs::create_dir_all(&firefox_root).expect("Failed to create Firefox root directory");

    // Create the defaults/pref directory
    let pref_dir = format!("{}/defaults/pref", firefox_root);
    let _ = fs::create_dir_all(&pref_dir).expect("Failed to create Firefox pref directory");

    // Create autoconfig.js in defaults/pref
    let autoconfig_js = r#"pref("general.config.filename", "localdesktop.cfg");
pref("general.config.obscure_value", 0);
pref("general.config.sandbox_enabled", false);
"#;

    let _ = fs::write(format!("{}/autoconfig.js", pref_dir), autoconfig_js)
        .expect("Failed to write Firefox autoconfig.js");

    // Create localdesktop.cfg in the Firefox root directory
    let firefox_cfg = r#"// Auto updated by Local Desktop on each startup, do not edit manually
defaultPref("media.cubeb.sandbox", false);
defaultPref("security.sandbox.content.level", 0);
defaultPref("media.allow-audio-non-utility", true);
defaultPref("media.rdd-process.enabled", false);

try {
  var { SandboxUtils } = ChromeUtils.importESModule("resource://gre/modules/SandboxUtils.sys.mjs");
  SandboxUtils.maybeWarnAboutDisabledContentSandbox = () => {};
  SandboxUtils.observeContentSandboxPref = () => {};
} catch (_) {}
"#; // It is required that the first line of this file is a comment, even if you have nothing to comment. Docs: https://support.mozilla.org/en-US/kb/customizing-firefox-using-autoconfig

    let _ = fs::write(format!("{}/localdesktop.cfg", firefox_root), firefox_cfg)
        .expect("Failed to write Firefox configuration");

    None
}

#[derive(Debug)]
enum KvLine {
    Entry {
        key: String,
        value: String,
        prefix: String,
        delimiter: char,
    },
    Other(String),
}

fn parse_kv_lines(content: &str, delimiter: char) -> Vec<KvLine> {
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                return KvLine::Other(line.to_string());
            }
            if let Some((left, right)) = line.split_once(delimiter) {
                let key = left.trim().to_string();
                if key.is_empty() {
                    return KvLine::Other(line.to_string());
                }
                let prefix_len = line.len() - trimmed.len();
                let prefix = line[..prefix_len].to_string();
                let value = right.trim().to_string();
                KvLine::Entry {
                    key,
                    value,
                    prefix,
                    delimiter,
                }
            } else {
                KvLine::Other(line.to_string())
            }
        })
        .collect()
}

fn set_kv_value(lines: &mut Vec<KvLine>, key: &str, value: &str, delimiter: char) {
    let mut updated = false;
    for line in lines.iter_mut() {
        if let KvLine::Entry {
            key: entry_key,
            value: entry_value,
            ..
        } = line
        {
            if entry_key == key {
                *entry_value = value.to_string();
                updated = true;
            }
        }
    }
    if !updated {
        lines.push(KvLine::Entry {
            key: key.to_string(),
            value: value.to_string(),
            prefix: String::new(),
            delimiter,
        });
    }
}

fn render_kv_lines(lines: &[KvLine]) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        match line {
            KvLine::Entry {
                key,
                value,
                prefix,
                delimiter,
            } => out.push(format!("{}{}{} {}", prefix, key, delimiter, value)),
            KvLine::Other(raw) => out.push(raw.to_string()),
        }
    }
    let mut content = out.join("\n");
    content.push('\n');
    content
}

fn upsert_kv_file(path: &Path, delimiter: char, updates: &[(&str, String)]) {
    let content = fs::read_to_string(path).unwrap_or_default();
    let mut lines = parse_kv_lines(&content, delimiter);
    for (key, value) in updates {
        set_kv_value(&mut lines, key, value, delimiter);
    }
    let content = render_kv_lines(&lines);
    fs::write(path, content).expect("Failed to write key/value file");
}

fn ensure_pacman_ignore_pkg(content: &str, packages: &[&str]) -> String {
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let value = packages.join(" ");

    let Some(options_start) = lines.iter().position(|line| line.trim() == "[options]") else {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("[options]".to_string());
        lines.push(format!("IgnorePkg   = {value}"));
        let mut out = lines.join("\n");
        out.push('\n');
        return out;
    };

    let options_end = lines
        .iter()
        .enumerate()
        .skip(options_start + 1)
        .find(|(_, line)| {
            let trimmed = line.trim();
            trimmed.starts_with('[') && trimmed.ends_with(']')
        })
        .map(|(index, _)| index)
        .unwrap_or(lines.len());

    let mut insert_after_comment = None;
    for index in options_start + 1..options_end {
        let trimmed = lines[index].trim_start();
        let active = !trimmed.starts_with('#');
        let candidate = if active {
            trimmed
        } else {
            trimmed.trim_start_matches('#').trim_start()
        };

        let Some((key, existing)) = candidate.split_once('=') else {
            continue;
        };
        if key.trim() != "IgnorePkg" {
            continue;
        }

        if active {
            let merged = merge_pacman_list(existing, packages);
            lines[index] = format!("IgnorePkg   = {merged}");
            let mut out = lines.join("\n");
            out.push('\n');
            return out;
        }

        insert_after_comment = Some(index + 1);
    }

    lines.insert(
        insert_after_comment.unwrap_or(options_start + 1),
        format!("IgnorePkg   = {value}"),
    );

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn merge_pacman_list(existing: &str, packages: &[&str]) -> String {
    let mut values: Vec<String> = existing.split_whitespace().map(str::to_string).collect();
    for package in packages {
        if !values.iter().any(|value| value == package) {
            values.push((*package).to_string());
        }
    }
    values.join(" ")
}

fn remove_pacman_ignore_pkg(content: &str, packages: &[&str]) -> String {
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();

    let Some(options_start) = lines.iter().position(|line| line.trim() == "[options]") else {
        let mut out = lines.join("\n");
        out.push('\n');
        return out;
    };

    let options_end = lines
        .iter()
        .enumerate()
        .skip(options_start + 1)
        .find(|(_, line)| {
            let trimmed = line.trim();
            trimmed.starts_with('[') && trimmed.ends_with(']')
        })
        .map(|(index, _)| index)
        .unwrap_or(lines.len());

    for line in lines.iter_mut().take(options_end).skip(options_start + 1) {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }

        let Some((key, existing)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "IgnorePkg" {
            continue;
        }

        let remaining = existing
            .split_whitespace()
            .filter(|value| !packages.iter().any(|package| package == value))
            .collect::<Vec<_>>()
            .join(" ");
        *line = if remaining.is_empty() {
            "IgnorePkg   =".to_string()
        } else {
            format!("IgnorePkg   = {remaining}")
        };
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn setup_fake_bwrap(_: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let wrapper_path = fs_root.join("usr/local/bin/bwrap");

    // bwrap (Bubblewrap) requires Linux user namespaces (CLONE_NEWUSER) which are
    // blocked by Android SELinux. We replace it with a shim that strips all
    // namespace/sandbox flags and directly exec's the target binary.
    // This unblocks glycin-svg (used by Onboard) which sandbox-loads SVG files via bwrap.
    let wrapper = r#"#!/bin/sh
# bwrap shim for proot/Android: namespaces are unavailable, exec directly.
# Strips all bwrap sandbox/namespace/bind flags, then exec's the target binary.
while [ $# -gt 0 ]; do
    case "$1" in
        # Three-argument flags (flag + src/key + dest/value)
        --ro-bind|--bind|--dev-bind|--bind-try|--ro-bind-try|--dev-bind-try|\
        --file|--bind-data|--ro-bind-data|--symlink|\
        --setenv|--chmod) shift 3 ;;
        # Two-argument flags (flag + single arg)
        --tmpfs|--proc|--dir|\
        --unsetenv|--perms|--cap-add|--cap-drop|\
        --seccomp|--add-seccomp-fd|--info-fd|--json-status-fd|\
        --block-fd|--userns-block-fd|--userns|--userns2|\
        --pidns|--chdir|--dev|--mqueue) shift 2 ;;
        # Zero-argument flags
        --unshare-all|--unshare-user|--unshare-user-try|--unshare-pid|\
        --unshare-ipc|--unshare-net|--unshare-uts|--unshare-cgroup|\
        --unshare-cgroup-try|--share-net|--remount-ro|\
        --as-pid-1|--die-with-parent|--new-session|--clearenv) shift ;;
        --) shift; break ;;
        *) break ;;
    esac
done
exec "$@"
"#;

    let _ = fs::create_dir_all(
        wrapper_path
            .parent()
            .expect("Failed to read bwrap wrapper parent directory"),
    );
    fs::write(&wrapper_path, wrapper).expect("Failed to write bwrap wrapper");
    fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755))
        .expect("Failed to mark bwrap wrapper executable");

    None
}

/// The realpath(3) that asks proot for the whole answer (src/guest/realpath.c), as the APK has it
/// and where the rootfs gets it.
const FAST_REALPATH_ASSET: &str = "guest/librealpath.so";
const FAST_REALPATH_LIB: &str = "/usr/local/lib/localdesktop/librealpath.so";
const LD_SO_PRELOAD: &str = "etc/ld.so.preload";

/// glibc's realpath(3) readlinks every component of a path to find its symlinks, and proot stops
/// the program for each; with Node, Vite resolves every module that way. The preloaded library
/// asks proot once per path instead. `[performance] fast_realpath = false` unloads it.
fn setup_fast_realpath(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let mut wanted = get_application_context()
        .local_config
        .performance
        .fast_realpath;
    if wanted {
        if let Err(error) = install_fast_realpath(&options.android_app, fs_root) {
            // A preloaded library that isn't there makes every program complain.
            log::warn!("Could not install {FAST_REALPATH_LIB}: {error}");
            wanted = false;
        }
    }
    if let Err(error) = set_preload_line(fs_root, FAST_REALPATH_LIB, wanted) {
        log::warn!("Could not update /{LD_SO_PRELOAD}: {error}");
    }
    None
}

fn install_fast_realpath(android_app: &AndroidApp, fs_root: &Path) -> std::io::Result<()> {
    let name = CString::new(FAST_REALPATH_ASSET).expect("asset name");
    let mut asset = android_app
        .asset_manager()
        .open(&name)
        .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "not in the APK"))?;
    let mut bytes = Vec::with_capacity(asset.length());
    asset.read_to_end(&mut bytes)?;

    let path = fs_root.join(FAST_REALPATH_LIB.trim_start_matches('/'));
    if fs::read(&path).is_ok_and(|installed| installed == bytes) {
        return Ok(());
    }
    // Replaced in one step: running programs may have the old one mapped.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let new = path.with_extension("so.new");
    fs::write(&new, &bytes)?;
    fs::set_permissions(&new, fs::Permissions::from_mode(0o755))?;
    fs::rename(&new, &path)
}

/// Add `library` to the rootfs' /etc/ld.so.preload, or take it out, keeping anything else there.
fn set_preload_line(fs_root: &Path, library: &str, present: bool) -> std::io::Result<()> {
    let path = fs_root.join(LD_SO_PRELOAD);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut lines: Vec<&str> = content
        .lines()
        .filter(|line| line.trim() != library)
        .collect();
    if present {
        lines.push(library);
    }
    if lines.iter().all(|line| line.trim().is_empty()) {
        return match fs::remove_file(&path) {
            Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let wanted = lines.join("\n") + "\n";
    if wanted != content {
        fs::write(&path, wanted)?;
        // Every user's programs read it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

fn setup_chromium_no_sandbox(_: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);

    // Chromium's sandbox needs CLONE_NEWUSER, which Android SELinux blocks, so every
    // Chromium/Electron app has to be started with --no-sandbox. Electron apps pick that up
    // from ELECTRON_DISABLE_SANDBOX (exported by startxfce4-localdesktop), but Chromium itself
    // only takes the flag, and its desktop entry hardcodes an absolute path that a
    // /usr/local/bin wrapper cannot intercept. So shadow the affected application entries in
    // the user's own XDG directory, re-running every session to catch newly installed apps.
    write_executable(
        &fs_root.join("usr/local/bin/localdesktop-no-sandbox-entries"),
        r#"#!/bin/sh
target_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$target_dir" || exit 0

# The scan starts several processes per entry, which took seconds of every session start
# under proot, so it only runs when something in the application directories changed.
stamp="$target_dir/.localdesktop-no-sandbox-scanned"
if [ -e "$stamp" ] && [ -z "$(find /usr/share/applications /usr/local/share/applications \
        -maxdepth 1 -newer "$stamp" 2>/dev/null | head -n1)" ]; then
    exit 0
fi
# Before scanning, so entries installed meanwhile get seen next time.
touch "$stamp"

for src in /usr/share/applications/*.desktop /usr/local/share/applications/*.desktop; do
    [ -f "$src" ] || continue

    prog=$(sed -n 's/^Exec=//p' "$src" | head -n1 | awk '{print $1}')
    [ -n "$prog" ] || continue
    case "$prog" in
        /*) bin="$prog" ;;
        *) bin=$(command -v "$prog" 2>/dev/null) || continue ;;
    esac
    bin=$(readlink -f "$bin" 2>/dev/null)
    [ -n "$bin" ] || continue

    # Every Chromium/Electron build ships the setuid sandbox helper next to its binary,
    # or one level up when the launcher lives in a bin/ subdirectory.
    dir=$(dirname "$bin")
    [ -e "$dir/chrome-sandbox" ] || [ -e "$dir/../chrome-sandbox" ] || continue

    dst="$target_dir/$(basename "$src")"
    # Leave alone anything the user wrote themselves.
    if [ -e "$dst" ] && ! grep -q '^X-LocalDesktop-NoSandbox=' "$dst"; then
        continue
    fi

    awk '
        /^\[Desktop Entry\]/ && !seen { print; print "X-LocalDesktop-NoSandbox=true"; seen = 1; next }
        /^Exec=/ && !/--no-sandbox/ { sub(/^Exec=[^ ]+/, "& --no-sandbox") }
        { print }
    ' "$src" > "$dst"
done
"#,
    );

    // Same flag for terminal launches, following the /usr/local/bin PATH-priority pattern.
    write_executable(
        &fs_root.join("usr/local/bin/chromium"),
        r#"#!/bin/sh
[ -x /usr/bin/chromium ] || { echo "chromium is not installed" >&2; exit 127; }
exec /usr/bin/chromium --no-sandbox "$@"
"#,
    );

    None
}

fn setup_onboard_signal_fix(_: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let wrapper_path = fs_root.join("usr/local/bin/onboard");

    // proot intercepts fstat() on socket fds and follows /proc/self/fd/N which points
    // to "socket:[inode]" — not a real path. Python 3.14's signal.set_wakeup_fd()
    // calls fstat(fd) to validate the wakeup socket, which fails with ENOENT under proot.
    // We install a wrapper at /usr/local/bin/onboard (higher PATH priority than /usr/sbin)
    // that monkey-patches signal.set_wakeup_fd to swallow OSError before launching the
    // real Onboard binary.
    let wrapper = r#"#!/usr/bin/python3
# Onboard wrapper for proot/Android: patches signal.set_wakeup_fd to handle
# OSError (ENOENT) caused by proot's fstat translation on socket file descriptors.
import signal as _signal
_orig_swf = _signal.set_wakeup_fd
def _safe_swf(fd, **kwargs):
    try:
        return _orig_swf(fd, **kwargs)
    except OSError:
        return -1
_signal.set_wakeup_fd = _safe_swf

import runpy, sys
sys.argv[0] = '/usr/sbin/onboard'
runpy.run_path('/usr/sbin/onboard', run_name='__main__')
"#;

    let _ = fs::create_dir_all(
        wrapper_path
            .parent()
            .expect("Failed to read onboard wrapper parent directory"),
    );
    fs::write(&wrapper_path, wrapper).expect("Failed to write onboard wrapper");
    fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755))
        .expect("Failed to mark onboard wrapper executable");

    None
}

fn chroot_home_dir(fs_root: &Path, username: &str) -> PathBuf {
    if username == "root" {
        fs_root.join("root")
    } else {
        fs_root.join(format!("home/{username}"))
    }
}

fn write_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, contents).expect("Failed to write executable script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("Failed to mark executable script");
}

/// Map Android density to a whole-number UI scale factor (same baseline as the old LXQt setup).
fn android_ui_scale(density_dpi: i32) -> i32 {
    ((density_dpi as f32) / 160.0 * 1.1).max(1.0).round() as i32
}

/// The start of every session launcher: audio, a private runtime directory, the compositor socket.
fn session_environment() -> String {
    format!(
        r#"export PIPEWIRE_RUNTIME_DIR={PIPEWIRE_GUEST_RUNTIME_DIR}
export PULSE_SERVER={PULSE_GUEST_SERVER}
# A private runtime directory per user, like logind would provide, so sockets, locks and dconf
# state that another user's session left in the shared /tmp don't get in the way.
if [ -z "${{XDG_RUNTIME_DIR:-}}" ] || [ "$XDG_RUNTIME_DIR" = /tmp ]; then
    XDG_RUNTIME_DIR=/tmp/runtime-$(id -u)
fi
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR
# Local Desktop's own compositor socket stays in /tmp.
case "${{WAYLAND_DISPLAY:=wayland-0}}" in
    /*) ;;
    *) WAYLAND_DISPLAY=/tmp/$WAYLAND_DISPLAY ;;
esac
export WAYLAND_DISPLAY
# Electron adds --no-sandbox when this is set; Android has no user namespaces for it to use.
export ELECTRON_DISABLE_SANDBOX=1
# The compositors here only take shared-memory buffers (there's no GPU render node), so Vulkan
# apps have to present through them.
export MESA_VK_WSI_DEBUG=sw
# Firefox's main process can't reopen a memfd read-only through /proc/self/fd (Android's SELinux),
# so it shares memory through /dev/shm instead; its content processes still insist on memfd seals
# and crashed on every page ("Shared memory PlatformHandle is not safe to map").
export MOZ_SHM_NO_SEALS=1
{TZ_FROM_LOCALTIME}"#
    )
}

/// Desktop items are seeded create-if-missing (the run-once mechanism described on
/// `StageOutput`): write only when absent, so we never clobber the user's edits or re-create on
/// every launch. Deleting an item re-seeds it next launch, same as the rest of the managed
/// environment.
fn seed_desktop_items(home_dir: &Path, pdf_viewer: &str) {
    let desktop_dir = home_dir.join("Desktop");
    let _ = fs::create_dir_all(&desktop_dir);

    let online_docs = desktop_dir.join("localdesktop-online-docs.desktop");
    if !online_docs.exists() {
        let _ = fs::write(
            &online_docs,
            format!(
                r#"[Desktop Entry]
Version=1.0
Type=Application
Name=Local Desktop - Online Docs
Comment=Open the Local Desktop documentation website
Exec=firefox {DOCS_HOME_URL}
Icon=firefox
Terminal=false
StartupNotify=true
"#
            ),
        );
    }
    // Remove the launcher's former name so existing installs pick up the rename.
    let _ = fs::remove_file(desktop_dir.join("localdesktop-documentation.desktop"));

    // Open PDFs (e.g. the User Manual) in the desktop's viewer instead of Firefox.
    // Create-if-missing so we don't stomp a user's own default-app choices.
    let mimeapps = home_dir.join(".config/mimeapps.list");
    if !mimeapps.exists() {
        let _ = fs::create_dir_all(home_dir.join(".config"));
        let _ = fs::write(
            &mimeapps,
            format!("[Default Applications]\napplication/pdf={pdf_viewer}\n"),
        );
    }
}

fn setup_xfce_wayland(options: &SetupOptions) -> StageOutput {
    let local_config = get_application_context().local_config;
    if local_config.desktop.preset() != DesktopPreset::Xfce {
        return None;
    }
    let fs_root = Path::new(ARCH_FS_ROOT);
    let username = local_config.user.username;
    let home_dir = chroot_home_dir(fs_root, &username);
    let labwc_dir = home_dir.join(".config/xfce4/labwc");

    let ui_scale = android_ui_scale(density_dpi(&options.android_app));
    // Xft uses 96 as the default logical DPI; multiply by scale for HiDPI fonts.
    let xft_dpi = ui_scale * 96;

    // Still useful for Xwayland clients started by labwc.
    let xresources_path = home_dir.join(".Xresources");
    let _ = fs::create_dir_all(
        xresources_path
            .parent()
            .expect("Failed to read Xresources parent directory"),
    );
    upsert_kv_file(&xresources_path, ':', &[("Xft.dpi", xft_dpi.to_string())]);

    // xfconf is read when xfce4-session starts; agent toggles must exist before launch
    // (https://docs.xfce.org/xfce/xfce4-session/advanced — SSH and GPG Agents).
    let xfconf_dir = home_dir.join(".config/xfce4/xfconf/xfce-perchannel-xml");
    let _ = fs::create_dir_all(&xfconf_dir);
    fs::write(
        xfconf_dir.join("xfce4-session.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>

<channel name="xfce4-session" version="1.0">
  <property name="startup" type="empty">
    <property name="ssh-agent" type="empty">
      <property name="enabled" type="bool" value="false"/>
    </property>
    <property name="gpg-agent" type="empty">
      <property name="enabled" type="bool" value="false"/>
    </property>
  </property>
</channel>
"#,
    )
    .expect("Failed to write xfce4-session xfconf defaults");
    fs::write(
        xfconf_dir.join("xsettings.xml"),
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>

<channel name="xsettings" version="1.0">
  <property name="Xft" type="empty">
    <property name="DPI" type="int" value="{xft_dpi}"/>
  </property>
</channel>
"#
        ),
    )
    .expect("Failed to write xsettings xfconf defaults");

    // https://docs.xfce.org/xfce/getting-started — `startxfce4 --wayland` starts the
    // session manager, panel, compositor (labwc), and desktop manager.
    write_executable(
        &fs_root.join("usr/local/bin/startxfce4-localdesktop"),
        &format!(
            "#!/bin/sh\n{}exec startxfce4 --wayland \"$@\"\n",
            session_environment()
        ),
    );

    // Runs from ~/.config/autostart once xfsettingsd is up; reinforces pre-seeded /Xft/DPI and
    // refreshes the --no-sandbox application entries for anything installed since last session.
    write_executable(
        &fs_root.join("usr/local/bin/localdesktop-xfce-session-init"),
        &format!(
            r#"#!/bin/sh
for _ in $(seq 1 50); do
    xfconf-query -c xsettings -lv >/dev/null 2>&1 && break
    sleep 0.1
done

xfconf-query -c xsettings -p /Xft/DPI -n -t int -s {xft_dpi} 2>/dev/null || \
xfconf-query -c xsettings -p /Xft/DPI -t int -s {xft_dpi}

/usr/local/bin/localdesktop-no-sandbox-entries
"#
        ),
    );

    seed_desktop_items(&home_dir, "org.gnome.Evince.desktop");

    let autostart_dir = home_dir.join(".config/autostart");
    let _ = fs::create_dir_all(&autostart_dir);

    fs::write(
        autostart_dir.join("localdesktop-xfce-session-init.desktop"),
        r#"[Desktop Entry]
Version=1.0
Type=Application
Name=Local Desktop Xfce Session Init
Comment=Apply HiDPI font scaling and refresh sandbox-free application entries
Exec=/usr/local/bin/localdesktop-xfce-session-init
Terminal=false
OnlyShowIn=XFCE;
X-GNOME-Autostart-enabled=true
"#,
    )
    .expect("Failed to write Xfce session init autostart entry");

    // xfce4-power-manager expects host power interfaces that proot cannot provide.
    fs::write(
        autostart_dir.join("xfce4-power-manager.desktop"),
        r#"[Desktop Entry]
Type=Application
Name=Power Manager
Hidden=true
OnlyShowIn=XFCE;
"#,
    )
    .expect("Failed to disable xfce4-power-manager autostart");

    let _ = fs::remove_file(autostart_dir.join("localdesktop-xfce-scale.desktop"));
    let _ = fs::remove_file(autostart_dir.join("localdesktop-wlroots-output.desktop"));
    let _ = fs::remove_file(fs_root.join("usr/local/bin/localdesktop-xfce-scale"));

    // labwc runs wlr-randr from its autostart script once the compositor owns the output
    // (labwc-config.5). Xfce stores labwc config under ~/.config/xfce4/labwc/.
    //
    // Host geometry is written to /tmp/localdesktop-output by the Android compositor before
    // launch; the script waits for that file instead of applying a hardcoded fallback mode.
    write_executable(
        &fs_root.join("usr/local/bin/localdesktop-wlroots-output"),
        &format!(
            r#"#!/bin/sh
# Keep labwc's wlroots output aligned with the Android host window.
state_file="/tmp/localdesktop-output"
lock_file="${{XDG_RUNTIME_DIR:-/tmp}}/localdesktop-wlroots-output.pid"
fallback_scale="{ui_scale}"

if [ -r "$lock_file" ]; then
    old_pid=$(cat "$lock_file" 2>/dev/null)
    if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
        exit 0
    fi
fi
echo "$$" > "$lock_file"
trap 'rm -f "$lock_file"' EXIT INT TERM

first_output() {{
    wlr-randr 2>/dev/null | awk 'NF > 0 && $1 !~ /^Modes:/ && $1 !~ /^Current:/ && $1 !~ /^Position:/ && $1 !~ /^Transform:/ && $1 !~ /^Scale:/ {{ print $1; exit }}'
}}

read_output_state() {{
    target_mode=""
    target_scale="$fallback_scale"
    if [ -r "$state_file" ]; then
        . "$state_file"
        target_mode="${{LOCALDESKTOP_OUTPUT_MODE:-}}"
        target_scale="${{LOCALDESKTOP_OUTPUT_SCALE:-$target_scale}}"
    fi
    case "$target_mode" in
        *x*) ;;
        *) return 1 ;;
    esac
    case "$target_scale" in
        ''|*[!0-9]*) target_scale="$fallback_scale" ;;
    esac
}}

apply_output() {{
    output="$1"
    wlr-randr --output "$output" --custom-mode "${{target_mode}}@60Hz" --scale "$target_scale" >/dev/null 2>&1 && return 0
    wlr-randr --output "$output" --custom-mode "$target_mode" --scale "$target_scale" >/dev/null 2>&1 && return 0
    wlr-randr --output "$output" --mode "$target_mode" --scale "$target_scale" >/dev/null 2>&1 && return 0
    wlr-randr --output "$output" --scale "$target_scale" >/dev/null 2>&1 && return 0
    return 1
}}

last_config=""
while true; do
    if ! read_output_state; then
        sleep 0.2
        continue
    fi
    output=$(first_output)
    if [ -n "$output" ]; then
        config="$output $target_mode $target_scale"
        if [ "$config" != "$last_config" ] && apply_output "$output"; then
            last_config="$config"
        fi
    fi
    sleep 1
done
"#
        ),
    );

    let _ = fs::create_dir_all(&labwc_dir);
    // Nested on our compositor: reuse the parent wl_output mode when possible (labwc-config.5).
    fs::write(
        labwc_dir.join("rc.xml"),
        r#"<?xml version="1.0"?>
<labwc_config>
  <core>
    <reuseOutputMode>yes</reuseOutputMode>
  </core>
</labwc_config>
"#,
    )
    .expect("Failed to write labwc rc.xml defaults");
    write_executable(
        &labwc_dir.join("autostart"),
        r#"#!/bin/sh
/usr/local/bin/localdesktop-wlroots-output >"${XDG_RUNTIME_DIR:-/tmp}/localdesktop-wlroots-output.log" 2>&1 &
"#,
    );

    // Arch wiki: lock prevents startxfce4 from overwriting custom labwc environment.
    // https://wiki.archlinux.org/title/Xfce#Using_labwc_custom_keymaps
    fs::write(
        labwc_dir.join("environment"),
        "XDG_SESSION_TYPE=wayland\nXDG_CURRENT_DESKTOP=XFCE\n",
    )
    .expect("Failed to write labwc environment file");
    fs::write(labwc_dir.join("lock"), "").expect("Failed to write labwc environment lock file");

    let _ = fs::remove_file(home_dir.join(".config/labwc/autostart"));

    None
}

/// Local Desktop's defaults for Plasma. The launcher puts this directory first in
/// `XDG_CONFIG_DIRS`, so KDE reads it after the user's own settings and before the packages'
/// `/etc/xdg`: whatever the user changes in System Settings still wins.
const PLASMA_XDG_DIR: &str = "etc/localdesktop/plasma";

const PLASMA_DEFAULTS: &[(&str, &str)] = &[
    (
        "kwinrc",
        "[Wayland]\n# Plasma's on-screen keyboard, for touch-only use.\nInputMethod=/usr/share/applications/org.kde.plasma.keyboard.desktop\n# Show it for any input: touches reach KWin as mouse events, which it would otherwise ignore.\nVirtualKeyboardMode=2\n",
    ),
    (
        "kdeglobals",
        "[KDE]\n# KWin composites on the CPU here, so animations cost more than they add.\nAnimationDurationFactor=0\n",
    ),
    (
        "ksmserverrc",
        "[General]\n# Don't reopen the last session's apps; startup is slow enough already.\nloginMode=emptySession\n",
    ),
    (
        "kscreenlockerrc",
        "[Daemon]\n# Android already locks the phone.\nAutolock=false\nLockOnResume=false\n",
    ),
    (
        "kwalletrc",
        "[Wallet]\n# Without a PAM login nothing opens the wallet, so every app storing a secret would ask for its password.\nEnabled=false\nFirst Use=false\n",
    ),
    (
        "baloofilerc",
        "[Basic Settings]\n# File indexing costs CPU and battery in the background.\nIndexing-Enabled=false\n",
    ),
    (
        "kded6rc",
        "# Background services for hardware, disks and network services that proot doesn't have.\n\
         [Module-baloosearchmodule]\nautoload=false\n\n\
         [Module-device_automounter]\nautoload=false\n\n\
         [Module-devicenotifications]\nautoload=false\n\n\
         [Module-donationmessage]\nautoload=false\n\n\
         [Module-freespacenotifier]\nautoload=false\n\n\
         [Module-geotimezoned]\nautoload=false\n\n\
         [Module-kded_touchpad]\nautoload=false\n\n\
         [Module-oom_notifier]\nautoload=false\n\n\
         [Module-remotenotifier]\nautoload=false\n\n\
         [Module-smbwatcher]\nautoload=false\n\n\
         [Module-wpad_detector]\nautoload=false\n",
    ),
];

/// Autostart entries hidden by an entry of the same name in `PLASMA_XDG_DIR/autostart`.
const PLASMA_HIDDEN_AUTOSTART: &[&str] = &[
    // Indexing is off anyway.
    "baloo_file.desktop",
    // Global menus for GTK apps, which the default panel doesn't show.
    "gmenudbusmenuproxy.desktop",
    "kaccess.desktop",
    // The wallet is off.
    "pam_kwallet_init.desktop",
    // There is no system bus, so nothing can ask polkit for authorization.
    "polkit-kde-authentication-agent-1.desktop",
    // Android manages power, and PowerDevil's display power-off leaves a blank screen.
    "powerdevil.desktop",
];

/// A Plasma update script: plasmashell runs each one once per user, also on a brand-new layout.
const PLASMA_UNPIN_MISSING_APPS: (&str, &str) = (
    "usr/share/plasma/shells/org.kde.plasma.desktop/contents/updates/localdesktop_unpin_missing_apps.js",
    r#"// Local Desktop: unpin task manager launchers for applications that aren't installed, such as
// Discover in Plasma's default pins, which the Plasma preset leaves out. readConfig() can't see
// the widget's built-in defaults, so an unset list stands for this copy of them
// (plasma-desktop 6.7, applets/taskmanager/main.xml).
var defaultLaunchers = [
    "applications:systemsettings.desktop",
    "applications:org.kde.discover.desktop",
    "preferred://filemanager",
    "preferred://browser",
];

panels().forEach(function (panel) {
    panel.widgets().forEach(function (widget) {
        if (widget.type !== "org.kde.plasma.icontasks" && widget.type !== "org.kde.plasma.taskmanager") {
            return;
        }
        widget.currentConfigGroup = ["General"];
        var launchers = widget.readConfig("launchers", defaultLaunchers);
        if (typeof launchers === "string") {
            launchers = launchers ? launchers.split(",") : [];
        }
        var kept = launchers.filter(function (launcher) {
            var match = /^applications:(.+)$/.exec(launcher);
            return !match || applicationExists(match[1]);
        });
        if (kept.length !== launchers.length) {
            widget.writeConfig("launchers", kept);
            widget.reloadConfig();
        }
    });
});
"#,
);

fn setup_plasma(_: &SetupOptions) -> StageOutput {
    let local_config = get_application_context().local_config;
    if local_config.desktop.preset() != DesktopPreset::Plasma {
        return None;
    }
    let fs_root = Path::new(ARCH_FS_ROOT);

    write_executable(
        &fs_root.join("usr/local/bin/startplasma-localdesktop"),
        &format!(
            r#"#!/bin/sh
{env}export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=KDE
export XDG_CONFIG_DIRS=/{PLASMA_XDG_DIR}:${{XDG_CONFIG_DIRS:-/etc/xdg}}
# Qt sends its warnings to the journal, which nothing reads here, unless told otherwise; this
# way they land in the session log.
export QT_FORCE_STDERR_LOGGING=1
# Qt Quick draws with Vulkan on the GPU when the Adreno driver works (see `setup_adreno_mesa`),
# and with its own 2D renderer otherwise: without a GPU its OpenGL goes through llvmpipe, which
# took two cores to scroll a Plasma menu. The check costs about 0.2 s; without it a broken driver
# would leave Plasma without a panel.
if vulkaninfo --summary 2>/dev/null | grep -q 'driverName *= turnip'; then
    export QSG_RHI_BACKEND=vulkan
else
    export QT_QUICK_BACKEND=software
fi
/usr/local/bin/localdesktop-no-sandbox-entries
exec /usr/lib/plasma-dbus-run-session-if-needed startplasma-wayland "$@"
"#,
            env = session_environment()
        ),
    );

    let xdg_dir = fs_root.join(PLASMA_XDG_DIR);
    let autostart_dir = xdg_dir.join("autostart");
    fs::create_dir_all(&autostart_dir).expect("Failed to create the Plasma defaults directory");
    for (name, contents) in PLASMA_DEFAULTS {
        fs::write(xdg_dir.join(name), contents).expect("Failed to write a Plasma default");
    }
    for name in PLASMA_HIDDEN_AUTOSTART {
        fs::write(autostart_dir.join(name), "[Desktop Entry]\nHidden=true\n")
            .expect("Failed to hide a Plasma autostart entry");
    }
    let (script_path, script) = PLASMA_UNPIN_MISSING_APPS;
    let script_path = fs_root.join(script_path);
    if let Some(parent) = script_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(script_path, script).expect("Failed to write a Plasma update script");

    let home_dir = chroot_home_dir(fs_root, &local_config.user.username);
    seed_desktop_items(&home_dir, "org.kde.okular.desktop");

    // No splash screen: it only delays the desktop. Plasma puts its look-and-feel defaults
    // (~/.config/kdedefaults, which turn the splash back on) ahead of PLASMA_XDG_DIR, so this one
    // goes in the user's own config, create-if-missing so a splash chosen later sticks.
    let ksplashrc = home_dir.join(".config/ksplashrc");
    if !ksplashrc.exists() {
        let _ = fs::create_dir_all(home_dir.join(".config"));
        let _ = fs::write(&ksplashrc, "[KSplash]\nEngine=none\nTheme=None\n");
    }

    add_android_place(fs_root, &home_dir);

    None
}

/// Adds the phone's shared storage (bound at `/android` when the app may access all files) to
/// Dolphin's places, right after Home. Only once: someone who removes it keeps it removed.
fn add_android_place(fs_root: &Path, home_dir: &Path) {
    if !get_application_context().permission_all_files_access {
        return;
    }
    let data_dir = home_dir.join(".local/share");
    let stamp = data_dir.join("localdesktop/android-place-added");
    if stamp.exists() {
        return;
    }
    let places = data_dir.join("user-places.xbel");
    let guest_home = Path::new("/").join(home_dir.strip_prefix(fs_root).unwrap_or(home_dir));
    // Before the first session KDE hasn't written its defaults yet. A file holding only this
    // place would keep it from adding Home and Trash, so start from its defaults.
    let mut content =
        fs::read_to_string(&places).unwrap_or_else(|_| default_places(&guest_home.to_string_lossy()));
    if !content.contains("href=\"file:///android\"") {
        let Some(home_end) = content.find("</bookmark>").map(|it| it + "</bookmark>".len()) else {
            return;
        };
        content.insert_str(
            home_end,
            &places_bookmark("file:///android", "Android", "smartphone", "localdesktop/android", false),
        );
        let _ = fs::create_dir_all(&data_dir);
        if let Err(error) = fs::write(&places, content) {
            log::warn!("Failed to add the Android place to {}: {error}", places.display());
            return;
        }
    }
    let _ = fs::create_dir_all(data_dir.join("localdesktop"));
    let _ = fs::write(stamp, "");
}

fn places_bookmark(href: &str, title: &str, icon: &str, id: &str, system: bool) -> String {
    let system = if system {
        "    <isSystemItem>true</isSystemItem>\n"
    } else {
        ""
    };
    format!(
        "\n <bookmark href=\"{href}\">\n  <title>{title}</title>\n  <info>\n   \
         <metadata owner=\"http://freedesktop.org\">\n    <bookmark:icon name=\"{icon}\"/>\n   \
         </metadata>\n   <metadata owner=\"http://www.kde.org\">\n    <ID>{id}</ID>\n{system}   \
         </metadata>\n  </info>\n </bookmark>"
    )
}

/// The places KDE (KIO 6, places version 4) starts a user with, except "Recent Files" and "Recent
/// Locations": KDE adds those itself when the file doesn't say it did (`withRecentlyUsed`), which
/// would list them twice.
fn default_places(home: &str) -> String {
    let defaults = [
        (format!("file://{home}"), "Home", "user-home"),
        (format!("file://{home}/Desktop"), "Desktop", "user-desktop"),
        (format!("file://{home}/Documents"), "Documents", "folder-documents"),
        (format!("file://{home}/Downloads"), "Downloads", "folder-downloads"),
        (format!("file://{home}/Music"), "Music", "folder-music"),
        (format!("file://{home}/Pictures"), "Pictures", "folder-pictures"),
        (format!("file://{home}/Videos"), "Videos", "folder-videos"),
        ("remote:/".to_string(), "Network", "folder-network"),
        ("trash:/".to_string(), "Trash", "user-trash"),
    ];
    let bookmarks: String = defaults
        .iter()
        .enumerate()
        .map(|(index, (href, title, icon))| {
            places_bookmark(href, title, icon, &format!("localdesktop/{index}"), true)
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE xbel>\n<xbel \
         xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\" \
         xmlns:kdepriv=\"http://www.kde.org/kdepriv\" \
         xmlns:mime=\"http://www.freedesktop.org/standards/shared-mime-info\">\n <info>\n  \
         <metadata owner=\"http://www.kde.org\">\n   <kde_places_version>4</kde_places_version>\n  \
         </metadata>\n </info>{bookmarks}\n</xbel>\n"
    )
}

fn fix_xkb_symlink(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let xkb_path = fs_root.join("usr/share/X11/xkb");
    let mpsc_sender = options.mpsc_sender.clone();

    if let Ok(meta) = fs::symlink_metadata(&xkb_path) {
        if meta.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(&xkb_path) {
                if target.is_absolute() {
                    log::info!(
                        "Absolute symlink target detected: {} -> {}. This is a problem because libxkbcommon is loaded in NDK, whose / is not Arch FS root!",
                        xkb_path.display(),
                        target.display()
                    );
                    // Compute the relative path from /usr/share/X11/xkb to /usr/share/xkeyboard-config-2
                    // Both are inside the chroot, so strip the fs_root prefix
                    let xkb_inside = Path::new("/usr/share/X11/xkb");
                    let target_inside = Path::new("/usr/share/xkeyboard-config-2");
                    let rel_target = diff_paths(target_inside, xkb_inside.parent().unwrap())
                        .unwrap_or_else(|| target_inside.to_path_buf());
                    log::info!(
                        "Fixing with new relative symlink: {} -> {}",
                        xkb_path.display(),
                        rel_target.display()
                    );
                    // Remove the old symlink
                    let _ = fs::remove_file(&xkb_path);
                    // Create the new relative symlink
                    if let Err(e) = symlink(&rel_target, &xkb_path) {
                        mpsc_sender
                            .send(SetupMessage::Error(format!(
                                "Failed to create relative symlink for xkb: {}",
                                e
                            )))
                            .unwrap_or(());
                    }
                }
            }
        }
    }
    None
}

/// Hard links made before `PROOT_L2S_DIR` keep their data next to the first link; move it into
/// the shared store once, so removing that directory works like it does for newer links.
fn migrate_hard_links(_: &SetupOptions) -> StageOutput {
    match hard_links::migrate(Path::new(ARCH_FS_ROOT)) {
        Ok(Some(migration)) => {
            log::info!(
                "Moved {} hard link(s) into the shared store, repointed {} name(s)",
                migration.moved,
                migration.relinked
            );
            for (link, error) in migration.failed {
                log::warn!("Could not move hard link {}: {error}", link.display());
            }
        }
        Ok(None) => {}
        Err(error) => log::warn!("Hard link migration failed: {error}"),
    }
    None
}

pub fn setup(android_app: AndroidApp) -> PolarBearBackend {
    let (sender, receiver) = mpsc::channel();
    let progress = Arc::new(Mutex::new(0));

    ArchProcess::remove_stale_temp_files();
    if ArchProcess::is_supported(&android_app) {
        sender
            .send(SetupMessage::Progress(
                "✅ Your device is supported!".to_string(),
            ))
            .unwrap_or(());
    } else {
        log::info!("PRoot support check failed, showing Device Unsupported page");
        return PolarBearBackend::WebView(WebviewBackend {
            socket_port: 0,
            progress,
            error: ErrorVariant::Unsupported,
        });
    }

    // A fresh install asks which desktop to install; the answer is applied once the rootfs exists.
    let (choice_sender, choice_receiver) = mpsc::channel();
    let fresh_install = Path::new(ARCH_FS_ROOT)
        .read_dir()
        .map_or(true, |mut d| d.next().is_none());
    if fresh_install {
        sender.send(SetupMessage::ChooseDesktop).unwrap_or(());
    }

    let options = SetupOptions {
        android_app: android_app.clone(),
        mpsc_sender: sender.clone(),
        desktop_choice: fresh_install.then(|| Arc::new(Mutex::new(choice_receiver))),
    };

    let stages: Vec<SetupStage> = vec![
        Box::new(setup_arch_fs),                // Step 1. Setup Arch FS (extract)
        Box::new(simulate_linux_sysdata_stage), // Step 2. Simulate Linux system data
        Box::new(apply_desktop_choice),         // Step 3. Write the desktop chosen on a fresh install
        Box::new(install_dependencies),         // Step 4. Install dependencies
        Box::new(setup_fast_realpath), // Step 4b. realpath(3) in one question to proot, for every program
        Box::new(setup_machine_id),             // Step 5. Seed /etc/machine-id for D-Bus clients
        Box::new(setup_time_zone),              // Step 6. Follow Android's time zone
        Box::new(setup_pipewire_package_lock), // Step 7. Hold guest PipeWire packages for the Android-side PipeWire POC
        Box::new(setup_adreno_mesa), // Step 7b. Mesa that drives the Adreno GPU through KGSL
        Box::new(setup_firefox_config),        // Step 8. Setup Firefox config
        Box::new(setup_fake_bwrap), // Step 9. Replace bwrap with a no-sandbox shim (Android has no user namespaces)
        Box::new(setup_chromium_no_sandbox), // Step 10. Make Chromium/Electron apps launchable without a terminal
        Box::new(setup_onboard_signal_fix), // Step 11. Wrap Onboard to survive proot fstat/signal.set_wakeup_fd failure
        Box::new(setup_xfce_wayland),       // Step 12. Setup Xfce Wayland launch and HiDPI scaling
        Box::new(setup_plasma),             // Step 13. Setup the Plasma launcher and defaults
        Box::new(super::ssh::setup_ssh),    // Step 14. Install and configure sshd when [ssh] wants it
        Box::new(fix_xkb_symlink),          // Step 15. Fix xkb symlink
        Box::new(migrate_hard_links),       // Step 16. Move old hard link data into the shared store (once)
    ];

    let handle_stage_error = |e: Box<dyn std::any::Any + Send>, sender: &Sender<SetupMessage>| {
        let error_msg = if let Some(e) = e.downcast_ref::<String>() {
            format!("Stage execution failed: {}", e)
        } else if let Some(e) = e.downcast_ref::<&str>() {
            format!("Stage execution failed: {}", e)
        } else {
            "Stage execution failed: Unknown error".to_string()
        };
        sender
            .send(SetupMessage::Failed(error_msg.clone()))
            .unwrap_or(());
    };

    let fully_installed = 'outer: loop {
        for (i, stage) in stages.iter().enumerate() {
            if let Some(handle) = stage(&options) {
                let progress_clone = progress.clone();
                let sender_clone = sender.clone();
                thread::spawn(move || {
                    let progress = progress_clone;
                    let progress_value = ((i) as u16 * 100 / stages.len() as u16) as u16;
                    *progress.lock().unwrap() = progress_value;

                    // Wait for the current stage to finish
                    if let Err(e) = handle.join() {
                        handle_stage_error(e, &sender_clone);
                        return;
                    }

                    // Process the remaining stages in the same loop
                    for (j, next_stage) in stages.iter().enumerate().skip(i + 1) {
                        let progress_value = ((j) as u16 * 100 / stages.len() as u16) as u16;
                        *progress.lock().unwrap() = progress_value;
                        if let Some(next_handle) = next_stage(&options) {
                            if let Err(e) = next_handle.join() {
                                handle_stage_error(e, &sender_clone);
                                return;
                            }

                            // Increment progress and send it
                            let next_progress_value =
                                ((j + 1) as u16 * 100 / stages.len() as u16) as u16;
                            *progress.lock().unwrap() = next_progress_value;
                        }
                    }

                    // All stages are done, we need to replace the WebviewBackend with the WaylandBackend
                    // Or, easier, just restart the whole app
                    *progress.lock().unwrap() = 100;
                    sender_clone
                        .send(SetupMessage::Progress(
                            "Installation finished, please restart the app".to_string(),
                        ))
                        .expect("Failed to send installation finished message");
                });

                // Setup is still running in the background, but we need to return control
                // so that the main thread can continue to report progress to the user
                break 'outer false;
            }
        }

        // All stages were done previously, no need to wait for anything
        break 'outer true;
    };

    if fully_installed {
        PolarBearBackend::Wayland(WaylandBackend {
            compositor: Compositor::build().expect("Failed to build compositor"),
            graphic_renderer: None,
            clock: Clock::new(),
            key_counter: 0,
            guest_scale_factor: scale_factor(&android_app),
            touch_points: std::collections::HashMap::new(),
            scroll_centroid: None,
            touch_mode: TouchMode::Undecided,
            touch_down_position: None,
            touch_down_time: None,
            touch_slop_px: touch_slop_px(&android_app),
            long_press_timeout_ms: long_press_timeout_ms(&android_app),
            pointer_pressed: false,
            android_app,
        })
    } else {
        session::start_setup_service(&android_app);
        PolarBearBackend::WebView(WebviewBackend::build(receiver, progress, choice_sender))
    }
}
