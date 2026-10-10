use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Inside the data directory of the package this build is for, see `build.rs`.
#[cfg(not(test))]
pub const ARCH_FS_ROOT: &str = concat!("/data/data/", env!("LOCALDESKTOP_PACKAGE"), "/files/arch");
#[cfg(test)]
pub const ARCH_FS_ROOT: &str = "/data/local/tmp/arch";

pub const ARCH_FS_ARCHIVE: &str = "https://github.com/termux/proot-distro/releases/download/v4.29.0/archlinux-aarch64-pd-v4.29.0.tar.xz";

/// Project homepage, also the online documentation entry point.
pub const DOCS_HOME_URL: &str = "https://localdesktop.github.io/";

/// Download URL for the offline User Manual PDF matching the running version.
/// The release asset is dot-free/hyphenated (GitHub turns spaces into dots).
pub fn user_manual_url() -> String {
    format!(
        "https://github.com/localdesktop/localdesktop.github.io/releases/download/v{VERSION}/Local-Desktop-v{VERSION}-User-Manual.pdf"
    )
}

pub const WAYLAND_SOCKET_NAME: &str = "wayland-0";

pub const MAX_PANEL_LOG_ENTRIES: usize = 100;

#[cfg(official_package)]
pub const SENTRY_DSN: &str = "https://d8af27f864ade027ff81ecadea91b02e@o4509548388417536.ingest.de.sentry.io/4509548392480848";
/// Builds under another package name (forks, side-by-side dev builds) don't report to the
/// maintainers' Sentry project; an empty DSN leaves the Sentry client disabled.
#[cfg(not(official_package))]
pub const SENTRY_DSN: &str = "";

/// PipeWire runtime path as seen from inside the proot guest.
pub const PIPEWIRE_GUEST_RUNTIME_DIR: &str = "/tmp";

/// PipeWire-Pulse socket as seen from inside the proot guest.
pub const PULSE_GUEST_SERVER: &str = "unix:/tmp/pulse/native";

/// Make sure the config keys are all lowercase, and config values are single-line. Use \n for multi-line config values if needed
/// If a key exists multiple time, the first entry is applied
/// If a `try_` config exsists multiple time, the last entry is applied
/// But in general, it is **invalid** to have duplicated config keys inside a TOML file
pub const CONFIG_FILE: &str = "/etc/localdesktop/localdesktop.toml";

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct LocalConfig {
    #[serde(default)]
    pub user: UserConfig,

    #[serde(default)]
    pub desktop: DesktopConfig,

    #[serde(default)]
    pub ssh: SshConfig,

    #[serde(default)]
    pub graphics: GraphicsConfig,

    #[serde(default)]
    pub performance: PerformanceConfig,

    #[serde(default)]
    pub clipboard: ClipboardConfig,

    #[serde(default)]
    pub screen: ScreenConfig,

    #[serde(default)]
    pub notifications: NotificationsConfig,

    #[serde(default)]
    pub battery: BatteryConfig,

    /// What happens if we don't assign this `#[serde(default)]` attribute?
    /// The answer: If the user omits the `[command]` group, the WHOLE config fails to parse
    /// => The default `[user]` group is applied (with `username=root`) even if the `[user]` settings are completely valid.
    /// => So make sure that every config group has a `#[serde(default)]` attribute to avoid invalid sections breaking unrelated parts of the config.
    #[serde(default)]
    pub command: CommandConfig,

    /// Mistakes found in the config file, for the user; the config itself makes the best of them.
    #[serde(skip)]
    pub problems: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserConfig {
    pub username: String,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            username: "root".to_string(),
        }
    }
}

/// What `useradd` takes, which is also what the config has to say to be safe in a shell command:
/// lowercase letters, digits, `_` and `-`, starting with a letter or `_`, at most 32 of them.
pub fn valid_username(username: &str) -> bool {
    let mut chars = username.chars();
    chars.next().is_some_and(|it| it.is_ascii_lowercase() || it == '_')
        && chars.all(|it| it.is_ascii_lowercase() || it.is_ascii_digit() || it == '_' || it == '-')
        && username.len() <= 32
}

/// The config a fresh install starts with: the desktop chosen on the setup page and the user
/// named there, unless that's root or empty (or not a valid name, which the page doesn't send).
pub fn first_config(preset: DesktopPreset, username: &str) -> String {
    let preset = match preset {
        DesktopPreset::Xfce => "xfce",
        DesktopPreset::Plasma => "plasma",
    };
    let mut config = format!(
        "# Local Desktop's config: {DOCS_HOME_URL}docs/user/configurations\n\
         [desktop]\n\
         # \"xfce\" or \"plasma\". Changing it installs the other desktop on the next start.\n\
         preset = \"{preset}\"\n"
    );
    if username != "root" && valid_username(username) {
        config.push_str(&format!(
            "\n[user]\n\
             # Who the desktop runs as, with sudo. A new name is made on the next start.\n\
             username = \"{username}\"\n"
        ));
    }
    config
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopPreset {
    Xfce,
    Plasma,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct DesktopConfig {
    /// `xfce` (the default) or `plasma`. Kept as a string so that an unknown value falls back to
    /// Xfce instead of invalidating the whole config.
    #[serde(default)]
    pub preset: String,
}

impl DesktopConfig {
    pub fn preset(&self) -> DesktopPreset {
        match self.preset.trim() {
            "plasma" => DesktopPreset::Plasma,
            _ => DesktopPreset::Xfce,
        }
    }
}

/// An OpenSSH server for the session user, in its own proot so it outlives a broken desktop.
/// It only starts when someone can log in: a key here or in the user's `~/.ssh/authorized_keys`,
/// or `password_login`.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct SshConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Android apps can't listen below 1024.
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub password_login: bool,
    /// Public keys to add to the user's `~/.ssh/authorized_keys`, separated by `\n`.
    #[serde(default)]
    pub authorized_keys: String,
}

fn default_true() -> bool {
    true
}

/// GPU drivers. Arch's Mesa only drives GPUs through `/dev/dri`, which Android apps don't get;
/// on Qualcomm phones (with `/dev/kgsl-3d0`) Local Desktop installs a Mesa build that talks to
/// the Adreno through KGSL instead.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct GraphicsConfig {
    /// Install and keep Mesa for Adreno (https://github.com/lfdevs/mesa-for-android-container).
    /// `false` puts Arch's own Mesa back.
    #[serde(default = "default_true")]
    pub adreno_drivers: bool,
}

impl Default for GraphicsConfig {
    fn default() -> Self {
        Self {
            adreno_drivers: true,
        }
    }
}

/// How Android schedules the Linux programs. Under proot a program spends much of its time
/// stopped while proot handles its system calls, so the scheduler sees it as less busy than it is
/// and runs it on slower cores at a lower clock, where every system call costs more. A floor on
/// its utilization corrects that; it only costs energy while programs are running. Unless it's
/// `off`, proot also raises its own floor to the maximum while it handles system calls in quick
/// succession, since that's what the programs wait for.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PerformanceConfig {
    /// `off`, `balanced` (the default) or `max`. Kept as a string so that an unknown value falls
    /// back to `balanced` instead of invalidating the section.
    #[serde(default = "default_cpu_boost")]
    pub cpu_boost: String,
    /// Load a realpath(3) that asks proot for the whole answer at once into every program
    /// (through /etc/ld.so.preload), instead of glibc's, which stops for each part of the path.
    #[serde(default = "default_true")]
    pub fast_realpath: bool,
}

fn default_cpu_boost() -> String {
    "balanced".to_string()
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            cpu_boost: default_cpu_boost(),
            fast_realpath: true,
        }
    }
}

/// One clipboard for Android and the desktop: what is copied on one side can be pasted on the
/// other. Linux programs can then read what was copied on Android while Local Desktop is in
/// front, like programs on any desktop can.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ClipboardConfig {
    #[serde(default = "default_true")]
    pub sync: bool,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self { sync: true }
    }
}

/// Local Desktop keeps the screen on while it's in front, so that a video or a long build isn't
/// cut short by Android's timeout.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ScreenConfig {
    /// Let the screen turn off after Android's timeout when nobody has used the desktop for that
    /// long and nothing in it (a video player) asks to stay awake, as the desktop's compositor
    /// tells; `false` keeps it on for as long as the app is in front.
    #[serde(default = "default_true")]
    pub sleep_when_idle: bool,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            sleep_when_idle: true,
        }
    }
}

/// The desktop's notifications on Android while Local Desktop isn't in front (a build that
/// finished while the phone was in a pocket). Back in front, they come off Android: the desktop
/// keeps them in its own history.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct NotificationsConfig {
    #[serde(default = "default_true")]
    pub forward: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { forward: true }
    }
}

/// Android's battery on the desktop: its battery widget shows the phone's charge, which Android's
/// status bar doesn't while Local Desktop fills the screen. The app answers as UPower on the
/// guest's system bus, where every desktop's battery widget looks.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct BatteryConfig {
    #[serde(default = "default_true")]
    pub share: bool,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self { share: true }
    }
}

impl PerformanceConfig {
    /// The minimum utilization (out of 1024) the scheduler assumes for the Linux programs.
    pub fn utilization_floor(&self) -> u32 {
        match self.cpu_boost.trim() {
            "off" => 0,
            "max" => 1024,
            _ => 512,
        }
    }
}

fn default_ssh_port() -> u16 {
    8022
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: default_ssh_port(),
            password_login: false,
            authorized_keys: String::new(),
        }
    }
}

impl SshConfig {
    /// The configured public keys, one per entry.
    pub fn keys(&self) -> Vec<&str> {
        self.authorized_keys
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect()
    }
}

/// Commands left out or empty come from the desktop preset, see `LocalConfig::with_preset_commands`.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct CommandConfig {
    #[serde(default)]
    pub check: String,
    #[serde(default)]
    pub install: String,
    #[serde(default)]
    pub launch: String,
}

/// `sudo` for the session user (see `setup_user`), in both presets.
fn xfce_check() -> String {
    "pacman -Q noto-fonts && pacman -Q xfce4-session && pacman -Q xfce4-panel && pacman -Q xfce4-settings && pacman -Q xfce4-terminal && pacman -Q thunar && pacman -Q xfdesktop && pacman -Q xfconf && pacman -Q labwc && pacman -Q wlr-randr && pacman -Q xorg-xwayland && pacman -Q xdg-desktop-portal && pacman -Q xdg-desktop-portal-gtk && pacman -Q onboard && pacman -Q firefox && pacman -Q evince && pacman -Q pipewire && pacman -Q pipewire-audio && pacman -Q pipewire-alsa && pacman -Q sudo"
        .to_string()
}

fn xfce_install() -> String {
    "stdbuf -oL pacman -Syu --needed --noconfirm --noprogressbar noto-fonts xfce4 labwc wlr-randr xorg-xwayland xdg-desktop-portal xdg-desktop-portal-gtk onboard firefox evince pipewire pipewire-audio pipewire-alsa sudo"
        .to_string()
}
/// Direct the desktop session to the compositor and the host PipeWire socket.
fn xfce_launch() -> String {
    format!("export PIPEWIRE_RUNTIME_DIR={PIPEWIRE_GUEST_RUNTIME_DIR} PULSE_SERVER={PULSE_GUEST_SERVER}; WAYLAND_DISPLAY=/tmp/wayland-0 XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=XFCE /usr/local/bin/startxfce4-localdesktop 2>&1")
        .to_string()
}

/// Plasma without the parts that need hardware or services Android doesn't give proot (Bluetooth,
/// NetworkManager, disks, printers). KWin nests directly on Local Desktop's compositor.
const PLASMA_PACKAGES: &str = "noto-fonts plasma-desktop plasma-keyboard plasma-pa kscreen konsole dolphin okular xdg-desktop-portal-kde xorg-xwayland firefox pipewire pipewire-audio pipewire-alsa sudo";

fn plasma_check() -> String {
    format!("pacman -Q {PLASMA_PACKAGES}")
}

fn plasma_install() -> String {
    format!("stdbuf -oL pacman -Syu --needed --noconfirm --noprogressbar {PLASMA_PACKAGES}")
}

/// `startplasma-localdesktop` is written by setup, like `startxfce4-localdesktop`.
fn plasma_launch() -> String {
    "/usr/local/bin/startplasma-localdesktop 2>&1".to_string()
}

impl LocalConfig {
    /// Fill the commands the config leaves out from the desktop preset.
    fn with_preset_commands(mut self) -> Self {
        let (check, install, launch) = match self.desktop.preset() {
            DesktopPreset::Xfce => (xfce_check(), xfce_install(), xfce_launch()),
            DesktopPreset::Plasma => (plasma_check(), plasma_install(), plasma_launch()),
        };
        for (value, preset_value) in [
            (&mut self.command.check, check),
            (&mut self.command.install, install),
            (&mut self.command.launch, launch),
        ] {
            if value.trim().is_empty() {
                *value = preset_value;
            }
        }
        self
    }
}

/// This function does 2 major tasks:
/// - Read config from `CONFIG_FILE`, and override configs with their `try_*` versions, and return the configs line by line
/// - Write back to the config file, with `try_*` configs commented out
///
/// **Important**: As each call to this function will comment out the `try_*` config, it is **non-idempotent**.
fn process_config_file(full_config_path: String) -> Vec<String> {
    let mut write_back_lines: Vec<String> = vec![];
    let mut effective_config: Vec<String> = vec![];

    if let Ok(content) = fs::read_to_string(&full_config_path) {
        for line in content.lines() {
            let trimmed = line.trim();

            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                let value = value.trim();

                if key.starts_with("try_") {
                    // Comment out the `try_*` configs
                    write_back_lines.push(format!("# {}", trimmed));

                    // Prefer the `try_*` configs
                    let actual_key = key.trim_start_matches("try_");
                    if let Some(line_index) = effective_config
                        .iter()
                        .position(|line| line.starts_with(&format!("{}=", actual_key)))
                    {
                        // Config exists, overriding
                        effective_config[line_index] = format!("{}={}", actual_key, value);
                    } else {
                        // Config does not exist, appending
                        effective_config.push(format!("{}={}", actual_key, value));
                        // Make sure there are no spaces around = so that the check existing key logic works
                    }
                } else {
                    // Keep the config as is
                    write_back_lines.push(trimmed.to_string());

                    if effective_config
                        .iter()
                        .any(|line| line.starts_with(&format!("{}=", key)))
                    {
                        // If already overridden by try_ version, skip inserting
                    } else {
                        // Config does not exist, appending
                        effective_config.push(format!("{}={}", key, value)); // Make sure there are no spaces around = so that the check existing key logic works
                    }
                }
            } else {
                // Keep the line as is
                write_back_lines.push(trimmed.to_string());
                effective_config.push(trimmed.to_string());
            }
        }

        // Rewrite config with try_* lines commented out
        let _ = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&full_config_path)
            .and_then(|mut file| {
                for line in &write_back_lines {
                    writeln!(file, "{}", line)?;
                }
                Ok(())
            });
    }

    // Convert effective config back to lines
    effective_config
}

pub fn parse_config(full_config_path: String) -> LocalConfig {
    let original = fs::read_to_string(&full_config_path).unwrap_or_default();
    let lines = process_config_file(full_config_path);
    let content = lines.join("\n");
    // A mistake anywhere used to replace the whole config with the defaults (say, the Xfce
    // preset on a Plasma install). Now it only costs its own section, and gets reported.
    let mut config = toml::from_str::<LocalConfig>(&content).unwrap_or_else(|_| lenient(&content));
    config.problems = check(&original);
    config.with_preset_commands()
}

/// The keys each section takes. Anything else is most likely a typo, which serde would
/// silently ignore.
const KNOWN_KEYS: &[(&str, &[&str])] = &[
    ("user", &["username"]),
    ("desktop", &["preset"]),
    ("ssh", &["enabled", "port", "password_login", "authorized_keys"]),
    ("graphics", &["adreno_drivers"]),
    ("performance", &["cpu_boost", "fast_realpath"]),
    ("clipboard", &["sync"]),
    ("screen", &["sleep_when_idle"]),
    ("notifications", &["forward"]),
    ("battery", &["share"]),
    ("command", &["check", "install", "launch"]),
];

/// Each section on its own, so that a bad value costs only its own section.
fn lenient(content: &str) -> LocalConfig {
    fn section<T: serde::de::DeserializeOwned + Default>(table: &toml::Table, name: &str) -> T {
        table
            .get(name)
            .cloned()
            .and_then(|value| value.try_into().ok())
            .unwrap_or_default()
    }
    let Ok(table) = content.parse::<toml::Table>() else {
        return LocalConfig::default();
    };
    LocalConfig {
        user: section(&table, "user"),
        desktop: section(&table, "desktop"),
        ssh: section(&table, "ssh"),
        graphics: section(&table, "graphics"),
        performance: section(&table, "performance"),
        clipboard: section(&table, "clipboard"),
        screen: section(&table, "screen"),
        notifications: section(&table, "notifications"),
        battery: section(&table, "battery"),
        command: section(&table, "command"),
        problems: Vec::new(),
    }
}

/// What's wrong with the config file as the user wrote it, with its line numbers.
fn check(original: &str) -> Vec<String> {
    let table = match original.parse::<toml::Table>() {
        Ok(table) => table,
        Err(error) => return vec![describe(original, &error)],
    };
    let mut problems = Vec::new();
    if let Err(error) = toml::from_str::<LocalConfig>(original) {
        problems.push(describe(original, &error));
    }
    for (name, value) in &table {
        let Some((_, keys)) = KNOWN_KEYS.iter().find(|(section, _)| section == name) else {
            problems.push(format!("Unknown section or key `{name}`"));
            continue;
        };
        let Some(section) = value.as_table() else {
            continue;
        };
        for key in section.keys() {
            if !keys.contains(&key.strip_prefix("try_").unwrap_or(key)) {
                problems.push(format!("Unknown key `{key}` in [{name}]"));
            }
        }
    }
    let preset = table.get("desktop").and_then(|it| it.get("preset"));
    if let Some(preset) = preset.and_then(|it| it.as_str()) {
        if !matches!(preset.trim(), "" | "xfce" | "plasma") {
            problems.push(format!("Unknown desktop preset \"{preset}\", using xfce"));
        }
    }
    problems
}

fn describe(text: &str, error: &toml::de::Error) -> String {
    let message = error.message().trim();
    match error.span() {
        Some(span) => {
            let line = text[..span.start.min(text.len())].matches('\n').count() + 1;
            format!("Line {line}: {message}")
        }
        None => message.to_string(),
    }
}

/// The config `content` with `key = value` (`value` in TOML) in `[section]`: the key's first line
/// there replaced, with any comment after the old value kept; or the key added under the section's
/// header; or the section added at the end. The rest stays as it was.
pub fn with_value(content: &str, section: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(content.len() + key.len() + value.len() + 16);
    let mut current = None;
    let mut header_end = None;
    let mut replaced = false;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(name) = table_name(trimmed) {
            current = Some(name);
            out.push_str(line);
            if name == section && header_end.is_none() {
                header_end = Some(out.len());
            }
            continue;
        }
        let ours = !replaced && current == Some(section);
        if let Some((_, old)) = trimmed
            .split_once('=')
            .filter(|it| ours && it.0.trim() == key)
        {
            let ending = &line[line.trim_end_matches(['\r', '\n']).len()..];
            out.push_str(&format!("{key} = {value}{}{ending}", comment_after(old)));
            replaced = true;
            continue;
        }
        out.push_str(line);
    }
    if replaced {
        return out;
    }
    let line = format!("{key} = {value}\n");
    match header_end {
        Some(at) if out[..at].ends_with('\n') => out.insert_str(at, &line),
        // The header is the last line, without a line break.
        Some(_) => out.push_str(&format!("\n{line}")),
        None => {
            if !out.is_empty() {
                out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
            }
            out.push_str(&format!("[{section}]\n{line}"));
        }
    }
    out
}

/// The table a `[name]` header line starts.
fn table_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[').filter(|it| !it.starts_with('['))?;
    let (name, after) = rest.split_once(']')?;
    let after = after.trim();
    (after.is_empty() || after.starts_with('#')).then(|| name.trim())
}

/// The comment after a value (`"max"  # fast`), as " # fast", or nothing.
fn comment_after(value: &str) -> String {
    let value = value.trim();
    let end = match value.strip_prefix('"') {
        Some(rest) => rest.find('"').map_or(value.len(), |at| at + 2),
        None => value.find('#').unwrap_or(value.len()),
    };
    let rest = value[end..].trim();
    if rest.starts_with('#') {
        format!(" {rest}")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn with_config_file(content: &str, f: impl Fn(String)) -> () {
        let dir = tempdir().unwrap();
        let base_dir = dir.path().to_str().unwrap();
        let path = format!("{}/etc/localdesktop", base_dir);
        fs::create_dir_all(&path).unwrap();
        let file_path = format!("{}/localdesktop.toml", path);
        fs::write(&file_path, content).unwrap();
        f(file_path)
    }

    #[test]
    fn should_handle_configs_without_try() {
        with_config_file(
            r#"
                [user]
                username = "alice"

                [command]
                check = "check-cmd"
                install = "install-cmd"
                launch = "launch-cmd"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.user.username, "alice");
                assert_eq!(config.command.check, "check-cmd");
                assert_eq!(config.command.install, "install-cmd");
                assert_eq!(config.command.launch, "launch-cmd");
            },
        );
    }

    #[test]
    fn should_handle_configs_with_try() {
        with_config_file(
            r#"
                [user]
                username = "root"
                try_username = "testuser"

                [command]
                check = "check-cmd"
                try_check = "try-check"
                install = "install-cmd"
                launch = "launch-cmd"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.user.username, "testuser");
                assert_eq!(config.command.check, "try-check");
                assert_eq!(config.command.install, "install-cmd")
            },
        );
    }

    #[test]
    fn should_default_to_the_xfce_preset() {
        with_config_file(
            r#"
                [user]
                username = "alice"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.desktop.preset(), DesktopPreset::Xfce);
                assert_eq!(config.command.launch, xfce_launch());
                assert_eq!(config.command.install, xfce_install());
            },
        );
    }

    #[test]
    fn should_fill_commands_from_the_plasma_preset() {
        with_config_file(
            r#"
                [desktop]
                preset = "plasma"

                [command]
                launch = "launch-cmd"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.desktop.preset(), DesktopPreset::Plasma);
                assert_eq!(config.command.check, plasma_check());
                assert_eq!(config.command.install, plasma_install());
                assert_eq!(config.command.launch, "launch-cmd");
            },
        );
    }

    #[test]
    fn should_try_a_preset_once() {
        with_config_file(
            r#"
                [desktop]
                preset = "xfce"
                try_preset = "plasma"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path.clone());
                assert_eq!(config.desktop.preset(), DesktopPreset::Plasma);
                let config = parse_config(full_config_path);
                assert_eq!(config.desktop.preset(), DesktopPreset::Xfce);
            },
        );
    }

    #[test]
    fn should_fall_back_to_xfce_for_unknown_presets() {
        with_config_file(
            r#"
                [user]
                username = "alice"

                [desktop]
                preset = "gnome"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.user.username, "alice");
                assert_eq!(config.desktop.preset(), DesktopPreset::Xfce);
            },
        );
    }

    #[test]
    fn should_read_ssh_settings() {
        with_config_file(
            r#"
                [ssh]
                authorized_keys = "ssh-rsa AAAA== one\n\nssh-ed25519 BBBB two"
            "#,
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert!(config.ssh.enabled);
                assert_eq!(config.ssh.port, 8022);
                assert!(!config.ssh.password_login);
                assert_eq!(
                    config.ssh.keys(),
                    vec!["ssh-rsa AAAA== one", "ssh-ed25519 BBBB two"]
                );
            },
        );
    }

    #[test]
    fn should_keep_other_sections_when_one_is_wrong() {
        with_config_file(
            "[user]\nusername = \"alice\"\n\n[ssh]\nport = \"8022\"\n",
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.user.username, "alice");
                assert_eq!(config.ssh.port, 8022);
                assert!(
                    config.problems.iter().any(|it| it.starts_with("Line 5:")),
                    "{:?}",
                    config.problems
                );
            },
        );
    }

    #[test]
    fn should_install_adreno_drivers_unless_turned_off() {
        with_config_file("[user]\nusername = \"alice\"\n", |full_config_path| {
            assert!(parse_config(full_config_path).graphics.adreno_drivers);
        });
        with_config_file("[graphics]\nadreno_drivers = false\n", |full_config_path| {
            let config = parse_config(full_config_path);
            assert!(!config.graphics.adreno_drivers);
            assert!(config.problems.is_empty(), "{:?}", config.problems);
        });
    }

    #[test]
    fn should_start_a_fresh_install_with_the_chosen_desktop_and_user() {
        with_config_file(&first_config(DesktopPreset::Plasma, "teddy"), |full_config_path| {
            let config = parse_config(full_config_path);
            assert_eq!(config.desktop.preset(), DesktopPreset::Plasma);
            assert_eq!(config.user.username, "teddy");
            assert!(config.problems.is_empty(), "{:?}", config.problems);
        });
        for username in ["", "root", "Teddy", "te ddy", "teddy'; rm -rf /"] {
            let text = first_config(DesktopPreset::Xfce, username);
            assert!(!text.contains("[user]"), "{username:?}");
            with_config_file(&text, |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(config.desktop.preset(), DesktopPreset::Xfce);
                assert_eq!(config.user.username, "root");
                assert!(config.problems.is_empty(), "{:?}", config.problems);
            });
        }
    }

    #[test]
    fn should_take_only_names_useradd_takes() {
        let longest = "a".repeat(32);
        for name in ["teddy", "_build", "a", "user-2", "x_y", &longest] {
            assert!(valid_username(name), "{name:?}");
        }
        let too_long = "a".repeat(33);
        for name in ["", "Teddy", "2user", "-user", "te ddy", "tédd", "a.b", "a$b", &too_long] {
            assert!(!valid_username(name), "{name:?}");
        }
    }

    #[test]
    fn should_let_the_screen_sleep_when_idle_unless_turned_off() {
        with_config_file("[user]\nusername = \"alice\"\n", |full_config_path| {
            assert!(parse_config(full_config_path).screen.sleep_when_idle);
        });
        with_config_file("[screen]\nsleep_when_idle = false\n", |full_config_path| {
            let config = parse_config(full_config_path);
            assert!(!config.screen.sleep_when_idle);
            assert!(config.problems.is_empty(), "{:?}", config.problems);
        });
    }

    #[test]
    fn should_forward_notifications_unless_turned_off() {
        with_config_file("[user]\nusername = \"alice\"\n", |full_config_path| {
            assert!(parse_config(full_config_path).notifications.forward);
        });
        with_config_file("[notifications]\nforward = false\n", |full_config_path| {
            let config = parse_config(full_config_path);
            assert!(!config.notifications.forward);
            assert!(config.problems.is_empty(), "{:?}", config.problems);
        });
    }

    #[test]
    fn should_share_the_battery_unless_turned_off() {
        with_config_file("[user]\nusername = \"alice\"\n", |full_config_path| {
            assert!(parse_config(full_config_path).battery.share);
        });
        with_config_file("[battery]\nshare = false\n", |full_config_path| {
            let config = parse_config(full_config_path);
            assert!(!config.battery.share);
            assert!(config.problems.is_empty(), "{:?}", config.problems);
        });
    }

    #[test]
    fn should_boost_the_cpu_moderately_unless_told_otherwise() {
        with_config_file("[user]\nusername = \"alice\"\n", |full_config_path| {
            assert_eq!(parse_config(full_config_path).performance.utilization_floor(), 512);
        });
        with_config_file("[performance]\ncpu_boost = \"max\"\n", |full_config_path| {
            let config = parse_config(full_config_path);
            assert_eq!(config.performance.utilization_floor(), 1024);
            assert!(config.problems.is_empty(), "{:?}", config.problems);
        });
        with_config_file("[performance]\ncpu_boost = \"off\"\n", |full_config_path| {
            assert_eq!(parse_config(full_config_path).performance.utilization_floor(), 0);
        });
        with_config_file("[performance]\ncpu_boost = \"turbo\"\n", |full_config_path| {
            assert_eq!(parse_config(full_config_path).performance.utilization_floor(), 512);
        });
    }

    #[test]
    fn should_use_the_fast_realpath_unless_turned_off() {
        with_config_file("[performance]\ncpu_boost = \"max\"\n", |full_config_path| {
            assert!(parse_config(full_config_path).performance.fast_realpath);
        });
        with_config_file("[performance]\nfast_realpath = false\n", |full_config_path| {
            let config = parse_config(full_config_path);
            assert!(!config.performance.fast_realpath);
            assert_eq!(config.performance.utilization_floor(), 512);
            assert!(config.problems.is_empty(), "{:?}", config.problems);
        });
    }

    #[test]
    fn should_share_the_clipboard_unless_turned_off() {
        with_config_file("[user]\nusername = \"alice\"\n", |full_config_path| {
            assert!(parse_config(full_config_path).clipboard.sync);
        });
        with_config_file("[clipboard]\nsync = false\n", |full_config_path| {
            let config = parse_config(full_config_path);
            assert!(!config.clipboard.sync);
            assert!(config.problems.is_empty(), "{:?}", config.problems);
        });
    }

    #[test]
    fn should_report_syntax_errors_and_typos() {
        with_config_file("[user]\nusername = alice\n", |full_config_path| {
            let config = parse_config(full_config_path);
            assert!(config.problems[0].starts_with("Line 2:"), "{:?}", config.problems);
        });
        with_config_file(
            "[user]\nusername = \"alice\"\nusrname = \"bob\"\n[desktop]\npreset = \"gnome\"\n",
            |full_config_path| {
                let config = parse_config(full_config_path);
                assert_eq!(
                    config.problems,
                    vec![
                        "Unknown key `usrname` in [user]".to_string(),
                        "Unknown desktop preset \"gnome\", using xfce".to_string()
                    ]
                );
            },
        );
        with_config_file("[user]\nusername = \"alice\"\ntry_username = \"bob\"\n", |path| {
            assert!(parse_config(path).problems.is_empty());
        });
    }

    #[test]
    fn should_comment_out_try_configs() {
        with_config_file(
            r#"
                username = "root"
                try_username = "commented"

                check = "normal"
                try_check = "try"
            "#,
            |full_config_path| {
                let _ = parse_config(full_config_path.clone()); // This triggers rewriting the config file
                let content = fs::read_to_string(full_config_path).unwrap();

                assert!(
                    content.contains("# try_username = \"commented\""),
                    "❌ `try_username` is not commented out after being applied"
                );
                assert!(
                    content.contains("# try_check = \"try\""),
                    "❌ `try_check` is not commented out after being  applied"
                );
            },
        );
    }

    #[test]
    fn should_set_a_value_where_it_belongs() {
        let config = "[user]\nusername = \"alice\"\n\n[performance]\n# cpu_boost = \"off\"\n\
                      try_cpu_boost = \"off\"\ncpu_boost = \"max\"  # fast\nfast_realpath = true\n";
        // Its line, with its comment; not the commented one, nor the try_ one.
        assert_eq!(
            with_value(config, "performance", "cpu_boost", "\"balanced\""),
            config.replace(
                "cpu_boost = \"max\"  # fast",
                "cpu_boost = \"balanced\" # fast"
            )
        );
        // Under the section's header, where it hasn't the key.
        let without = "[performance]\nfast_realpath = false\n";
        assert_eq!(
            with_value(without, "performance", "cpu_boost", "\"off\""),
            "[performance]\ncpu_boost = \"off\"\nfast_realpath = false\n"
        );
        assert_eq!(
            with_value("[performance]", "performance", "cpu_boost", "\"off\""),
            "[performance]\ncpu_boost = \"off\"\n"
        );
        // A section of its own at the end, and a key of the same name elsewhere left alone.
        let elsewhere = "[user]\nusername = \"alice\"\n[other]\ncpu_boost = 1";
        assert_eq!(
            with_value(elsewhere, "performance", "cpu_boost", "\"max\""),
            format!("{elsewhere}\n\n[performance]\ncpu_boost = \"max\"\n")
        );
        assert_eq!(
            with_value("", "performance", "cpu_boost", "\"max\""),
            "[performance]\ncpu_boost = \"max\"\n"
        );
        // What comes out parses, with the value.
        let written = with_value(without, "performance", "cpu_boost", "\"max\"");
        with_config_file(&written, |full_config_path| {
            assert_eq!(parse_config(full_config_path).performance.cpu_boost, "max");
        });
    }
}
