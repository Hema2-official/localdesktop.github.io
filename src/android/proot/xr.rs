//! VR support on headsets: Monado, the OpenXR runtime of Linux apps, with Local Desktop's driver
//! (patches/monado), which renders into the buffers of the app's immersive mode, and the Turnip
//! the headset's GPU needs (patches/mesa).
//!
//! scripts/guest/build-xr.sh builds both into a bundle. CI builds one for each release, which the
//! APK names (`LOCALDESKTOP_XR_BUNDLE_URL` and `LOCALDESKTOP_XR_BUNDLE_SHA256` at build time).
//! Where there is none, or it doesn't work with the system's libraries (Arch moves on between
//! releases), setup runs the script itself, with the copy of it and the patches in the app.

use super::process::ArchProcess;
use super::setup::{
    download_attempt, ensure_pacman_list, unix_time, SetupMessage, SetupOptions, StageOutput,
    GPU_HELPER, GPU_HELPER_SCRIPT,
};
use crate::core::config::ARCH_FS_ROOT;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, ErrorKind, Read},
    os::unix::fs::{symlink, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{mpsc::Sender, Arc},
    thread,
    time::Duration,
};
use tar::{Archive, EntryType};
use xz2::read::XzDecoder;

/// What build-xr.sh builds from: itself and the patches, by their paths in the repository,
/// sorted (build.rs).
const RECIPE: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/xr_recipe.rs"));
const SCRIPT: &str = "scripts/guest/build-xr.sh";

/// The bundle CI built for this build, if any.
const BUNDLE_URL: Option<&str> = option_env!("LOCALDESKTOP_XR_BUNDLE_URL");
const BUNDLE_SHA256: Option<&str> = option_env!("LOCALDESKTOP_XR_BUNDLE_SHA256");

/// Installed with the packages the bundle names: the OpenXR loader apps link, and vulkaninfo,
/// which tells whether Turnip finds the GPU.
const EXTRA_PACKAGES: &[&str] = &["openxr", "vulkan-tools"];

/// What's installed: `recipe <hash>`, then `file <size> <path>` and `link <path>` for what the
/// bundle put there, and `needs <path>` for the libraries its files link. Each launch can tell
/// with a few `stat`s whether it's all still there (an update may have removed a library).
const STATE: &str = "var/lib/localdesktop/xr";
/// `<time> <recipe hash>` of the last failed install. The same recipe isn't tried again for a
/// day, since every try brings up the setup page.
const FAILED: &str = "var/lib/localdesktop/xr.failed";
const RETRY_SECS: u64 = 24 * 60 * 60;
/// Downloads, the copy of the recipe and the script's builds. Removed once installed.
const CACHE: &str = "var/cache/localdesktop-xr";

const MONADO_WRAPPER: &str = "usr/local/bin/localdesktop-monado";
const MONADO_WRAPPER_SCRIPT: &str = r#"#!/bin/sh
# Monado, the OpenXR runtime of Linux apps on this headset, as the desktop session starts it.
# XRT_NO_STDIN: it would watch stdin for commands, and epoll refuses the session's /dev/null.
mkdir -p "$HOME/.cache"
XRT_NO_STDIN=1 exec monado-service > "$HOME/.cache/monado-service.log" 2>&1
"#;
const MONADO_AUTOSTART: &str = "etc/xdg/autostart/localdesktop-monado.desktop";
const MONADO_AUTOSTART_ENTRY: &str = "[Desktop Entry]
Type=Application
Name=Monado
Comment=The OpenXR runtime of Linux apps on this headset
Exec=localdesktop-monado
NoDisplay=true
";
/// The system's OpenXR runtime, made Monado unless one was chosen already.
const ACTIVE_RUNTIME: &str = "etc/xdg/openxr/1/active_runtime.json";
const MONADO_RUNTIME: &str = "/usr/local/share/openxr/1/openxr_monado.json";
/// Arch's Turnip only drives GPUs through `/dev/dri`, and its manifest names its library without
/// a path, which found ours too: the GPU was listed twice. It goes, and pacman leaves it out.
const ARCH_TURNIP_MANIFEST: &str = "usr/share/vulkan/icd.d/freedreno_icd.json";

/// Install VR support on headsets, or bring it up to date with this build's recipe.
pub fn setup_xr(options: &SetupOptions) -> StageOutput {
    if !crate::android::xr::headset() {
        return None;
    }
    let fs_root = Path::new(ARCH_FS_ROOT);
    let recipe = recipe_hash();
    if intact(fs_root, &recipe) {
        if let Err(error) = integrate(fs_root) {
            log::warn!("VR support: {error}");
        }
        return None;
    }
    let failed = fs::read_to_string(fs_root.join(FAILED)).unwrap_or_default();
    if let Some((time, failed_recipe)) = failed.trim().split_once(' ') {
        let recent = time
            .parse::<u64>()
            .is_ok_and(|it| unix_time().saturating_sub(it) < RETRY_SECS);
        if recent && failed_recipe == recipe {
            return None;
        }
    }

    let sender = options.mpsc_sender.clone();
    Some(thread::spawn(move || {
        let failed = fs_root.join(FAILED);
        match install(&sender, &recipe) {
            Ok(()) => {
                let _ = fs::remove_file(failed);
                if let Err(error) = fs::remove_dir_all(fs_root.join(CACHE)) {
                    log::warn!("VR support: couldn't remove /{CACHE}: {error}");
                }
                progress(&sender, "VR support installed");
            }
            // Not worth failing the setup over: the desktop works without it. What the build
            // fetched and made stays for the next try.
            Err(error) => {
                log::warn!("VR support: {error}");
                sender
                    .send(SetupMessage::Error(format!(
                        "VR support: {error}. Trying again tomorrow; the desktop works without it."
                    )))
                    .unwrap_or(());
                let _ = fs::create_dir_all(fs_root.join("var/lib/localdesktop"));
                let _ = fs::write(failed, format!("{} {recipe}", unix_time()));
            }
        }
    }))
}

/// The recipe's hash, as build-xr.sh computes it: SHA-256 of `sha256sum`'s lines for its files.
fn recipe_hash() -> String {
    let sums: String = RECIPE
        .iter()
        .map(|(path, contents)| format!("{:x}  {path}\n", Sha256::digest(contents)))
        .collect();
    format!("{:x}", Sha256::digest(sums.as_bytes()))
}

fn progress(sender: &Sender<SetupMessage>, message: &str) {
    sender
        .send(SetupMessage::Progress(message.to_string()))
        .unwrap_or(());
}

/// The path of `path`, a path inside the rootfs, from outside.
fn host_path(fs_root: &Path, path: &str) -> PathBuf {
    fs_root.join(path.trim_start_matches('/'))
}

/// Whether this recipe's bundle is installed, with each file as it was and each library they
/// link still there.
fn intact(fs_root: &Path, recipe: &str) -> bool {
    let Ok(state) = fs::read_to_string(fs_root.join(STATE)) else {
        return false;
    };
    let mut lines = state.lines();
    if lines.next() != Some(format!("recipe {recipe}").as_str()) {
        return false;
    }
    let mut files = 0;
    for line in lines {
        let present = match line.split_once(' ') {
            Some(("file", rest)) => {
                let Some((size, path)) = rest.split_once(' ') else {
                    continue;
                };
                files += 1;
                fs::metadata(host_path(fs_root, path)).is_ok_and(|it| it.len().to_string() == size)
            }
            Some(("link", path)) => fs::symlink_metadata(host_path(fs_root, path)).is_ok(),
            Some(("needs", path)) => fs::metadata(host_path(fs_root, path)).is_ok(),
            _ => true,
        };
        if !present {
            log::info!("VR support changed or broke: {line}");
            return false;
        }
    }
    files > 0
}

/// The files and links the last install put there, by their paths inside the rootfs.
fn installed_paths(fs_root: &Path) -> Vec<String> {
    let state = fs::read_to_string(fs_root.join(STATE)).unwrap_or_default();
    state
        .lines()
        .filter_map(|line| match line.split_once(' ')? {
            ("file", rest) => Some(rest.split_once(' ')?.1.to_string()),
            ("link", path) => Some(path.to_string()),
            _ => None,
        })
        .collect()
}

fn install(sender: &Sender<SetupMessage>, recipe: &str) -> Result<(), String> {
    let cache = Path::new(ARCH_FS_ROOT).join(CACHE);
    progress(
        sender,
        "Installing VR support for this headset (Monado and the GPU driver it needs)...",
    );
    fs::create_dir_all(&cache).map_err(|error| error.to_string())?;
    if let (Some(url), Some(sha256)) = (BUNDLE_URL, BUNDLE_SHA256) {
        match download(url, sha256, &cache, sender)
            .and_then(|bundle| install_bundle(&bundle, recipe, sender))
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                log::warn!("VR support: the prebuilt bundle didn't install: {error}");
                progress(
                    sender,
                    &format!("The prebuilt VR support didn't install ({error}): building it here"),
                );
            }
        }
    }
    let bundle = build(&cache, sender)?;
    install_bundle(&bundle, recipe, sender)
}

/// Download the bundle into `dir` and check it, giving up on errors that waiting won't fix (a
/// release without it, say).
fn download(
    url: &str,
    sha256: &str,
    dir: &Path,
    sender: &Sender<SetupMessage>,
) -> Result<PathBuf, String> {
    let name = url
        .rsplit('/')
        .next()
        .filter(|it| !it.is_empty() && *it != "..")
        .ok_or("the bundle's URL has no file name")?;
    let path = dir.join(name);
    let part = dir.join(format!("{name}.part"));
    let client = reqwest::blocking::Client::builder()
        .user_agent("Local Desktop")
        .build()
        .map_err(|error| error.to_string())?;
    for attempt in 1u64.. {
        match download_attempt(&client, url, &part, sender, "Downloading VR support") {
            Ok(()) => break,
            Err(error) if attempt >= 5 || error.to_string().starts_with("HTTP 4") => {
                return Err(format!("couldn't download it: {error}"));
            }
            Err(error) => {
                log::warn!("VR support: download of {url} failed: {error}");
                thread::sleep(Duration::from_secs(10 * attempt));
            }
        }
    }

    let mut hasher = Sha256::new();
    let mut file = File::open(&part).map_err(|error| error.to_string())?;
    io::copy(&mut file, &mut hasher).map_err(|error| error.to_string())?;
    if !format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(sha256) {
        let _ = fs::remove_file(&part);
        return Err(format!("{name} doesn't match its checksum"));
    }
    fs::rename(&part, &path).map_err(|error| error.to_string())?;
    Ok(path)
}

/// Build the bundle in the rootfs with the app's copy of the recipe: about 15 minutes on a
/// Quest 3, plus the downloads.
fn build(cache: &Path, sender: &Sender<SetupMessage>) -> Result<PathBuf, String> {
    progress(
        sender,
        "Building VR support on this headset, which takes about 15 minutes...",
    );
    let recipe = cache.join("recipe");
    let _ = fs::remove_dir_all(&recipe);
    for (path, contents) in RECIPE {
        let file = recipe.join(path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(&file, contents).map_err(|error| error.to_string())?;
    }
    let bundle = cache.join("built.tar");
    let _ = fs::remove_file(&bundle);

    let log_sender = sender.clone();
    let built = ArchProcess {
        command: format!(
            "rm -f /var/lib/pacman/db.lck && sh /{CACHE}/recipe/{SCRIPT} -w /{CACHE}/work \
             --remove-build-deps /{CACHE}/recipe /{CACHE}/built.tar 2>&1"
        ),
        user: None,
        log: Some(Arc::new(move |it| {
            log_sender.send(SetupMessage::Progress(it)).unwrap_or(());
        })),
    }
    .run()
    .status
    .success();
    if !built || !bundle.is_file() {
        return Err("building it failed".into());
    }
    Ok(bundle)
}

/// Install a bundle over what's there: the packages it names, its files, and what makes them the
/// system's; then check that everything they link is there and that Turnip finds the GPU.
fn install_bundle(
    bundle: &Path,
    recipe: &str,
    sender: &Sender<SetupMessage>,
) -> Result<(), String> {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let file = File::open(bundle).map_err(|error| error.to_string())?;
    let reader: Box<dyn Read> = if bundle.extension().is_some_and(|it| it == "xz") {
        Box::new(XzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = Archive::new(reader);
    let mut entries = archive.entries().map_err(|error| error.to_string())?;

    // The manifest comes first.
    let mut manifest = String::new();
    {
        let mut entry = entries
            .next()
            .ok_or("the bundle is empty")?
            .map_err(|error| error.to_string())?;
        if entry.path().map_err(|error| error.to_string())?.as_ref() != Path::new("manifest") {
            return Err("the bundle doesn't start with its manifest".into());
        }
        entry
            .read_to_string(&mut manifest)
            .map_err(|error| error.to_string())?;
    }
    let mut built_from = None;
    let mut packages: Vec<&str> = EXTRA_PACKAGES.to_vec();
    for line in manifest.lines() {
        match line.split_once(' ') {
            Some(("recipe", hash)) => built_from = Some(hash.trim()),
            Some(("packages", names)) => packages.extend(names.split_whitespace()),
            _ => {}
        }
    }
    if built_from != Some(recipe) {
        return Err(format!(
            "it was built from another recipe ({})",
            built_from.unwrap_or("none")
        ));
    }
    let valid_name = |name: &&str| {
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._+-".contains(c))
    };
    if !packages.iter().all(valid_name) {
        return Err("its manifest names strange packages".into());
    }
    let packages = packages.join(" ");
    progress(
        sender,
        &format!("Installing what VR support needs: {packages}"),
    );
    if !pacman(
        &format!(
            "pacman -S --needed --noconfirm --noprogressbar {packages} \
             || pacman -Sy --needed --noconfirm --noprogressbar {packages}"
        ),
        sender,
    ) {
        return Err(format!("pacman couldn't install {packages}"));
    }

    progress(sender, "Installing Monado and Turnip...");
    let previous = installed_paths(fs_root);
    let mut state = format!("recipe {recipe}\n");
    let mut paths = HashSet::new();
    let mut files = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let path = entry
            .path()
            .map_err(|error| error.to_string())?
            .into_owned();
        let Ok(relative) = path.strip_prefix("root") else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        if !relative
            .components()
            .all(|it| matches!(it, Component::Normal(_)))
        {
            return Err(format!("the bundle has a strange path: {}", path.display()));
        }
        let target = fs_root.join(relative);
        let guest_path = format!("/{}", relative.display());
        let kind = entry.header().entry_type();
        if kind == EntryType::Directory {
            make_dirs(&target).map_err(|error| format!("{guest_path}: {error}"))?;
            continue;
        }
        if kind != EntryType::Regular && kind != EntryType::Symlink {
            continue;
        }
        if let Some(parent) = target.parent() {
            make_dirs(parent).map_err(|error| format!("{guest_path}: {error}"))?;
        }
        // A new file rather than the old one rewritten: programs may have it mapped.
        match fs::symlink_metadata(&target) {
            Ok(it) if it.is_dir() => return Err(format!("{guest_path} is a directory")),
            Ok(_) => fs::remove_file(&target).map_err(|error| format!("{guest_path}: {error}"))?,
            Err(_) => {}
        }
        remove_record(&target);
        entry
            .unpack(&target)
            .map_err(|error| format!("{guest_path}: {error}"))?;
        if kind == EntryType::Symlink {
            state.push_str(&format!("link {guest_path}\n"));
        } else {
            let size = fs::metadata(&target)
                .map_err(|error| error.to_string())?
                .len();
            state.push_str(&format!("file {size} {guest_path}\n"));
            files.push(guest_path.clone());
        }
        paths.insert(guest_path);
    }
    if files.is_empty() {
        return Err("the bundle has no files".into());
    }
    // What the previous bundle had and this one hasn't.
    for path in previous.iter().filter(|it| !paths.contains(*it)) {
        let file = host_path(fs_root, path);
        let _ = fs::remove_file(&file);
        remove_record(&file);
    }
    integrate(fs_root)?;

    for library in check(&files)? {
        state.push_str(&format!("needs {library}\n"));
    }
    // Explicitly installed now, for no package depends on them: removing the packages nothing
    // needs must leave them.
    pacman(&format!("pacman -D --asexplicit {packages}"), sender);
    fs::create_dir_all(fs_root.join("var/lib/localdesktop")).map_err(|error| error.to_string())?;
    fs::write(fs_root.join(STATE), state).map_err(|error| error.to_string())?;
    Ok(())
}

fn pacman(command: &str, sender: &Sender<SetupMessage>) -> bool {
    let log_sender = sender.clone();
    ArchProcess {
        command: format!("rm -f /var/lib/pacman/db.lck && {command}"),
        user: None,
        log: Some(Arc::new(move |it| {
            log_sender.send(SetupMessage::Progress(it)).unwrap_or(());
        })),
    }
    .run()
    .status
    .success()
}

/// The libraries `files` link, outside /usr/local, or what's wrong: a library missing, or Turnip
/// not finding the GPU.
fn check(files: &[String]) -> Result<Vec<String>, String> {
    let linked: Vec<String> = files
        .iter()
        .filter(|it| it.starts_with("/usr/local/bin/") || it.starts_with("/usr/local/lib/"))
        .map(|it| format!("'{it}'"))
        .collect();
    let output = ArchProcess {
        command: format!(
            "for f in {}; do ldd \"$f\"; done 2>/dev/null; \
             vulkaninfo --summary 2>/dev/null | grep -q 'driverName *= turnip' && echo TURNIP_OK",
            linked.join(" ")
        ),
        user: None,
        log: None,
    }
    .run();
    let output = String::from_utf8_lossy(&output.stdout);
    let mut missing: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("=> not found"))
        .filter_map(|line| line.split(" => ").next())
        .map(str::trim)
        .collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        return Err(format!("missing {}", missing.join(", ")));
    }
    if !output.lines().any(|line| line == "TURNIP_OK") {
        return Err("Turnip doesn't find the GPU".into());
    }
    let mut libraries: Vec<String> = output
        .lines()
        .filter_map(|line| line.split_once(" => ")?.1.split(" (").next())
        .filter(|it| it.starts_with('/') && !it.starts_with("/usr/local/"))
        .map(str::to_string)
        .collect();
    libraries.sort();
    libraries.dedup();
    Ok(libraries)
}

/// What makes Monado the runtime of the desktop's apps and Turnip their Vulkan driver, written
/// again on each launch where it changed.
fn integrate(fs_root: &Path) -> Result<(), String> {
    write_if_changed(&fs_root.join(MONADO_WRAPPER), MONADO_WRAPPER_SCRIPT, 0o755)?;
    write_if_changed(
        &fs_root.join(MONADO_AUTOSTART),
        MONADO_AUTOSTART_ENTRY,
        0o644,
    )?;
    write_if_changed(&fs_root.join(GPU_HELPER), GPU_HELPER_SCRIPT, 0o755)?;
    let active = fs_root.join(ACTIVE_RUNTIME);
    if fs::symlink_metadata(&active).is_err() {
        if let Some(parent) = active.parent() {
            make_dirs(parent).map_err(|error| error.to_string())?;
        }
        symlink(MONADO_RUNTIME, &active).map_err(|error| error.to_string())?;
    }
    let pacman_conf = fs_root.join("etc/pacman.conf");
    if let Ok(content) = fs::read_to_string(&pacman_conf) {
        let updated = ensure_pacman_list(&content, "NoExtract", &[ARCH_TURNIP_MANIFEST]);
        if updated != content {
            fs::write(&pacman_conf, updated).map_err(|error| error.to_string())?;
        }
    }
    match fs::remove_file(fs_root.join(ARCH_TURNIP_MANIFEST)) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.to_string()),
        _ => Ok(()),
    }
}

fn write_if_changed(path: &Path, contents: &str, mode: u32) -> Result<(), String> {
    if fs::read_to_string(path).is_ok_and(|it| it == contents) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        make_dirs(parent).map_err(|error| error.to_string())?;
    }
    fs::write(path, contents).map_err(|error| format!("{}: {error}", path.display()))?;
    remove_record(path);
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| error.to_string())
}

/// Forget what proot recorded about `file`'s owner and mode (`.proot-meta-file.<name>.meta` next
/// to it), which would otherwise apply to the file written there from outside; without a record
/// its own mode counts.
fn remove_record(file: &Path) {
    if let (Some(dir), Some(name)) = (file.parent(), file.file_name()) {
        let record = dir.join(format!(".proot-meta-file.{}.meta", name.to_string_lossy()));
        let _ = fs::remove_file(record);
    }
}

/// Make `dir` and its missing parents, open to every user: the app's umask would keep them to
/// itself.
fn make_dirs(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        make_dirs(parent)?;
    }
    match fs::create_dir(dir) {
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
        Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o755)),
    }
}
