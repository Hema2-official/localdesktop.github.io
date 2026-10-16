//! The files setup writes into the Linux system, kept under `src/guest/` as they land there:
//! `rootfs/` for the system's, `home/` for the user's, `session/` for parts of the session
//! launchers. Some carry `@NAME@` placeholders that setup fills in (`fill`).

macro_rules! guest_file {
    ($path:literal) => {
        include_str!(concat!("../guest/", $path))
    };
}

pub const TZ_FROM_LOCALTIME: &str = guest_file!("rootfs/etc/profile.d/localdesktop-tz.sh");
pub const GPU_HELPER: &str = guest_file!("rootfs/usr/local/bin/gpu");
pub const FIREFOX_AUTOCONFIG: &str = guest_file!("rootfs/usr/lib/firefox/defaults/pref/autoconfig.js");
pub const FIREFOX_CONFIG: &str = guest_file!("rootfs/usr/lib/firefox/localdesktop.cfg");
pub const BWRAP: &str = guest_file!("rootfs/usr/local/bin/bwrap");
pub const CHROMIUM: &str = guest_file!("rootfs/usr/local/bin/chromium");
pub const ONBOARD: &str = guest_file!("rootfs/usr/local/bin/onboard");
pub const NO_SANDBOX_ENTRIES: &str = guest_file!("rootfs/usr/local/bin/localdesktop-no-sandbox-entries");
/// `@SERVICE@`, `@SPOOL@`.
pub const OPEN_HELPER: &str = guest_file!("rootfs/usr/local/bin/localdesktop-open");
/// `@PIPE@`, `@WAYLAND_DISPLAY@`.
pub const LOCALDESKTOP_COMMAND: &str = guest_file!("rootfs/usr/local/bin/localdesktop");
pub const INSTALL_ENTRY: &str = guest_file!("rootfs/usr/share/applications/localdesktop-install.desktop");
pub const SETTINGS_ENTRY: &str = guest_file!("rootfs/usr/share/applications/localdesktop-settings.desktop");
pub const SHARE_SERVICE_MENU: &str = guest_file!("rootfs/usr/share/kio/servicemenus/localdesktop-share.desktop");
pub const SHARE_SEND_TO: &str = guest_file!("rootfs/usr/share/Thunar/sendto/localdesktop-share.desktop");
pub const BUNDLE_TYPE: &str = guest_file!("rootfs/usr/share/mime/packages/localdesktop.xml");

/// `@PIPEWIRE_GUEST_RUNTIME_DIR@`, `@PULSE_GUEST_SERVER@`, `@TZ_FROM_LOCALTIME@`.
pub const SESSION_ENVIRONMENT: &str = guest_file!("session/environment.sh");
pub const TURNIP_CHECK: &str = guest_file!("session/turnip-check.sh");
/// `@ZINK_EGL_LIB@`.
pub const ZINK: &str = guest_file!("session/zink.sh");
/// `@DOCS_HOME_URL@`.
pub const ONLINE_DOCS_ENTRY: &str = guest_file!("home/Desktop/localdesktop-online-docs.desktop");

/// `@ENV@`, `@PLASMA_XDG_DIR@`, `@TURNIP_CHECK@`, `@ZINK@`.
pub const STARTPLASMA: &str = guest_file!("rootfs/usr/local/bin/startplasma-localdesktop");
pub const PLASMA_KEYBOARD: &str = guest_file!("rootfs/usr/share/applications/localdesktop-keyboard.desktop");
pub const PLASMA_UNPIN_MISSING_APPS: &str = guest_file!(
    "rootfs/usr/share/plasma/shells/org.kde.plasma.desktop/contents/updates/localdesktop_unpin_missing_apps.js"
);
pub const PLASMA_KWINRC: &str = guest_file!("rootfs/etc/localdesktop/plasma/kwinrc");
pub const PLASMA_KDEGLOBALS: &str = guest_file!("rootfs/etc/localdesktop/plasma/kdeglobals");
pub const PLASMA_KSMSERVERRC: &str = guest_file!("rootfs/etc/localdesktop/plasma/ksmserverrc");
pub const PLASMA_KSCREENLOCKERRC: &str = guest_file!("rootfs/etc/localdesktop/plasma/kscreenlockerrc");
pub const PLASMA_KWALLETRC: &str = guest_file!("rootfs/etc/localdesktop/plasma/kwalletrc");
pub const PLASMA_BALOOFILERC: &str = guest_file!("rootfs/etc/localdesktop/plasma/baloofilerc");
pub const PLASMA_KDED6RC: &str = guest_file!("rootfs/etc/localdesktop/plasma/kded6rc");

/// `@ENV@`, `@GPU@`.
pub const STARTXFCE4: &str = guest_file!("rootfs/usr/local/bin/startxfce4-localdesktop");
/// `@XFT_DPI@`.
pub const XFCE_SESSION_INIT: &str = guest_file!("rootfs/usr/local/bin/localdesktop-xfce-session-init");
/// `@UI_SCALE@`.
pub const WLROOTS_OUTPUT: &str = guest_file!("rootfs/usr/local/bin/localdesktop-wlroots-output");
pub const XFCE_SESSION_XML: &str =
    guest_file!("home/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-session.xml");
/// `@XFT_DPI@`.
pub const XSETTINGS_XML: &str = guest_file!("home/.config/xfce4/xfconf/xfce-perchannel-xml/xsettings.xml");
pub const XFCE_SESSION_INIT_AUTOSTART: &str =
    guest_file!("home/.config/autostart/localdesktop-xfce-session-init.desktop");
pub const XFCE_POWER_MANAGER_AUTOSTART: &str = guest_file!("home/.config/autostart/xfce4-power-manager.desktop");
pub const LABWC_RC: &str = guest_file!("home/.config/xfce4/labwc/rc.xml");
pub const LABWC_AUTOSTART: &str = guest_file!("home/.config/xfce4/labwc/autostart");
pub const LABWC_ENVIRONMENT: &str = guest_file!("home/.config/xfce4/labwc/environment");

/// A guest file with its `@NAME@` placeholders filled in. A placeholder alone on its line stands
/// for whole lines when its value is: the line goes, and the value (ending in a line break, or
/// empty) takes its place.
pub fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut text = template.to_owned();
    for (name, value) in values {
        let token = format!("@{name}@");
        if value.is_empty() || value.ends_with('\n') {
            text = text.replace(&format!("{token}\n"), value);
        }
        text = text.replace(&token, value);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `@NAME@` placeholders of a template, sorted.
    fn placeholders(template: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = template;
        while let Some(start) = rest.find('@') {
            let after = &rest[start + 1..];
            match after.find('@') {
                Some(end)
                    if end > 0
                        && after[..end]
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c == '_') =>
                {
                    found.push(after[..end].to_owned());
                    rest = &after[end + 1..];
                }
                _ => rest = after,
            }
        }
        found.sort();
        found.dedup();
        found
    }

    #[test]
    fn should_have_the_placeholders_setup_fills() {
        for (template, names) in [
            (OPEN_HELPER, &["SERVICE", "SPOOL"][..]),
            (LOCALDESKTOP_COMMAND, &["PIPE", "WAYLAND_DISPLAY"]),
            (
                SESSION_ENVIRONMENT,
                &["PIPEWIRE_GUEST_RUNTIME_DIR", "PULSE_GUEST_SERVER", "TZ_FROM_LOCALTIME"],
            ),
            (ZINK, &["ZINK_EGL_LIB"]),
            (ONLINE_DOCS_ENTRY, &["DOCS_HOME_URL"]),
            (STARTPLASMA, &["ENV", "PLASMA_XDG_DIR", "TURNIP_CHECK", "ZINK"]),
            (STARTXFCE4, &["ENV", "GPU"]),
            (XFCE_SESSION_INIT, &["XFT_DPI"]),
            (WLROOTS_OUTPUT, &["UI_SCALE"]),
            (XSETTINGS_XML, &["XFT_DPI"]),
        ] {
            assert_eq!(placeholders(template), names);
        }
        for plain in [
            TZ_FROM_LOCALTIME,
            GPU_HELPER,
            BWRAP,
            CHROMIUM,
            ONBOARD,
            NO_SANDBOX_ENTRIES,
            TURNIP_CHECK,
            PLASMA_UNPIN_MISSING_APPS,
            LABWC_AUTOSTART,
        ] {
            assert!(placeholders(plain).is_empty(), "{plain}");
        }
    }

    #[test]
    fn should_fill_placeholders_and_whole_lines() {
        assert_eq!(
            fill("#!/bin/sh\n@ENV@\n@GPU@\nexec x \"$@\"\n", &[("ENV", "a=1\nb=2\n"), ("GPU", "")]),
            "#!/bin/sh\na=1\nb=2\nexec x \"$@\"\n"
        );
        assert_eq!(fill("scale=\"@UI_SCALE@\"\n", &[("UI_SCALE", "3")]), "scale=\"3\"\n");
    }
}
