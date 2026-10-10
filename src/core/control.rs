//! What the `localdesktop` command in the guest asks of the app (`guest::control`): one line per
//! command on a pipe in the rootfs. Every program in the guest can write there, so the commands
//! only do what is harmless, or what Android asks the user about itself (installing an app).

use std::path::{Component, Path, PathBuf};

/// The pipe, in the guest.
pub const PIPE: &str = "/run/localdesktop/control";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A web or mail link, for Android's apps.
    OpenUrl(String),
    /// An app (`.apk`), or a bundle of them (`.xapk`, `.apks`), to install on Android: its path in
    /// the guest.
    Install(String),
    /// Android's settings for the app.
    OpenSettings,
    /// The app's own terminal.
    OpenTerminal,
    RestartDesktop,
}

/// The command a line stands for, `None` for anything else.
pub fn parse(line: &str) -> Option<Command> {
    if line.chars().any(char::is_control) {
        return None;
    }
    let (verb, argument) = match line.split_once(' ') {
        Some((verb, argument)) => (verb, Some(argument)),
        None => (line, None),
    };
    match (verb, argument) {
        ("open-url", Some(url)) if is_link(url) => Some(Command::OpenUrl(url.into())),
        ("install", Some(path)) if path.starts_with('/') => Some(Command::Install(path.into())),
        ("open-settings", None) => Some(Command::OpenSettings),
        ("open-terminal", None) => Some(Command::OpenTerminal),
        ("restart-desktop", None) => Some(Command::RestartDesktop),
        _ => None,
    }
}

/// Links for a browser or a mail app: nothing that names an app or a file.
fn is_link(url: &str) -> bool {
    let scheme = url.split_once(':').map(|it| it.0.to_ascii_lowercase());
    matches!(scheme.as_deref(), Some("http" | "https" | "mailto"))
}

/// Where the app reads a file the guest names: in the rootfs, or in Android's shared storage,
/// which proot binds at `/android` and `/root/Android` when the app may read it. `None` for what
/// the app can't read that way.
pub fn host_path(guest: &str, rootfs: &str, shared_storage: bool) -> Option<PathBuf> {
    let path = Path::new(guest);
    if !path.is_absolute() || path.components().any(|it| it == Component::ParentDir) {
        return None;
    }
    for bind in ["/android", "/root/Android"] {
        if let Ok(rest) = path.strip_prefix(bind) {
            return shared_storage.then(|| Path::new("/sdcard").join(rest));
        }
    }
    if ["/proc", "/sys", "/dev"]
        .iter()
        .any(|it| path.starts_with(it))
    {
        return None;
    }
    Some(Path::new(rootfs).join(path.strip_prefix("/").ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_take_only_its_commands() {
        assert_eq!(
            parse("open-url https://example.org/a b"),
            Some(Command::OpenUrl("https://example.org/a b".into()))
        );
        assert_eq!(
            parse("open-url mailto:me@example.org"),
            Some(Command::OpenUrl("mailto:me@example.org".into()))
        );
        assert_eq!(parse("open-url intent://scan/#Intent;end"), None);
        assert_eq!(parse("open-url file:///etc/passwd"), None);
        assert_eq!(
            parse("install /home/tester/Downloads/My App.apk"),
            Some(Command::Install("/home/tester/Downloads/My App.apk".into()))
        );
        assert_eq!(parse("install Downloads/app.apk"), None);
        assert_eq!(parse("open-settings"), Some(Command::OpenSettings));
        assert_eq!(parse("open-terminal"), Some(Command::OpenTerminal));
        assert_eq!(parse("restart-desktop"), Some(Command::RestartDesktop));
        assert_eq!(parse("restart-desktop now"), None);
        assert_eq!(parse("open-url https://example.org/\u{7}"), None);
        assert_eq!(parse("rm -rf /"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn should_find_the_file_where_the_app_can_read_it() {
        let rootfs = "/data/data/app.polarbear/files/arch";
        assert_eq!(
            host_path("/home/tester/Downloads/app.apk", rootfs, false),
            Some(PathBuf::from(format!(
                "{rootfs}/home/tester/Downloads/app.apk"
            )))
        );
        assert_eq!(
            host_path("/android/Download/app.apk", rootfs, true),
            Some(PathBuf::from("/sdcard/Download/app.apk"))
        );
        assert_eq!(
            host_path("/root/Android/Download/app.apk", rootfs, true),
            Some(PathBuf::from("/sdcard/Download/app.apk"))
        );
        assert_eq!(host_path("/android/Download/app.apk", rootfs, false), None);
        // A folder that only starts like the bind is the rootfs's.
        assert_eq!(
            host_path("/androidx/app.apk", rootfs, true),
            Some(PathBuf::from(format!("{rootfs}/androidx/app.apk")))
        );
        assert_eq!(
            host_path("/home/tester/../../etc/shadow", rootfs, true),
            None
        );
        assert_eq!(host_path("/proc/self/root/app.apk", rootfs, true), None);
        assert_eq!(host_path("app.apk", rootfs, true), None);
    }
}
