//! What the settings page (`assets/settings.html`, `android::settings`) shows of the config file,
//! and how it writes a change: the line for that key, the rest of the file as the user wrote it.

use crate::core::config::{self, LocalConfig};
use serde_json::{Map, Value};

/// When a change takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    /// When the desktop restarts, which reads the config again.
    Restart,
    /// The next time the app starts: setup has work to do first (a user to make, a desktop or
    /// drivers to install, an SSH server to set up).
    NextStart,
}

impl Applies {
    pub fn name(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::NextStart => "next-start",
        }
    }
}

/// What a setting holds, and so which values it takes.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Switch,
    /// One of these.
    Choice(&'static [&'static str]),
    UserName,
    /// One an app may listen on.
    Port,
    /// Lines, one string in the file with `\n` between them.
    Lines,
}

pub struct Setting {
    pub section: &'static str,
    pub key: &'static str,
    pub kind: Kind,
    pub applies: Applies,
}

const fn setting(
    section: &'static str,
    key: &'static str,
    kind: Kind,
    applies: Applies,
) -> Setting {
    Setting {
        section,
        key,
        kind,
        applies,
    }
}

/// The settings the page shows; the config's commands it leaves to the file.
pub const SETTINGS: &[Setting] = &[
    setting(
        "desktop",
        "preset",
        Kind::Choice(&["plasma", "xfce"]),
        Applies::NextStart,
    ),
    setting("user", "username", Kind::UserName, Applies::NextStart),
    setting("clipboard", "sync", Kind::Switch, Applies::Restart),
    setting("notifications", "forward", Kind::Switch, Applies::Restart),
    setting("battery", "share", Kind::Switch, Applies::Restart),
    setting("screen", "sleep_when_idle", Kind::Switch, Applies::Restart),
    setting(
        "performance",
        "cpu_boost",
        Kind::Choice(&["off", "balanced", "max"]),
        Applies::Restart,
    ),
    setting(
        "performance",
        "fast_realpath",
        Kind::Switch,
        Applies::NextStart,
    ),
    setting(
        "graphics",
        "adreno_drivers",
        Kind::Switch,
        Applies::NextStart,
    ),
    setting("ssh", "enabled", Kind::Switch, Applies::NextStart),
    setting("ssh", "port", Kind::Port, Applies::NextStart),
    setting("ssh", "password_login", Kind::Switch, Applies::NextStart),
    setting("ssh", "authorized_keys", Kind::Lines, Applies::NextStart),
];

fn find(section: &str, key: &str) -> Option<&'static Setting> {
    SETTINGS
        .iter()
        .find(|it| it.section == section && it.key == key)
}

/// A setting's value in a config, as the page gets it.
fn value_of(config: &LocalConfig, setting: &Setting) -> Value {
    let all = serde_json::to_value(config).unwrap_or_default();
    let value = all[setting.section][setting.key].clone();
    match setting.kind {
        // An empty or unknown value means the default, which is what the page shows.
        Kind::Choice(choices) => match value.as_str().map(str::trim) {
            Some(it) if choices.contains(&it) => Value::from(it),
            _ => Value::from(default_choice(setting)),
        },
        _ => value,
    }
}

fn default_choice(setting: &Setting) -> &'static str {
    match (setting.section, setting.key) {
        ("desktop", "preset") => "xfce",
        ("performance", "cpu_boost") => "balanced",
        _ => "",
    }
}

/// The settings' values in a config, as `{"section.key": value}`.
pub fn values(config: &LocalConfig) -> Value {
    let values: Map<String, Value> = SETTINGS
        .iter()
        .map(|it| (format!("{}.{}", it.section, it.key), value_of(config, it)))
        .collect();
    Value::Object(values)
}

/// The settings whose value in the file differs from what the running app goes by, as
/// `section.key`, with when they take effect.
pub fn pending(running: &LocalConfig, file: &LocalConfig) -> Vec<(String, Applies)> {
    SETTINGS
        .iter()
        .filter(|it| value_of(running, it) != value_of(file, it))
        .map(|it| (format!("{}.{}", it.section, it.key), it.applies))
        .collect()
}

/// A value from the page as it goes in the file, or why it can't.
fn literal(setting: &Setting, value: &Value) -> Result<String, String> {
    let string = |value: &str| Value::String(value.into()).to_string();
    match setting.kind {
        Kind::Switch => value
            .as_bool()
            .map(|it| it.to_string())
            .ok_or_else(|| "Not on or off".into()),
        Kind::Choice(choices) => match value.as_str() {
            Some(it) if choices.contains(&it) => Ok(string(it)),
            _ => Err(format!("Not one of {}", choices.join(", "))),
        },
        Kind::UserName => {
            let name = value.as_str().unwrap_or_default().trim();
            if name == "root" || config::valid_username(name) {
                Ok(string(name))
            } else {
                Err(
                    "Lowercase letters, digits, _ and -, starting with a letter or _ (at most 32)"
                        .into(),
                )
            }
        }
        Kind::Port => match value.as_u64() {
            Some(port @ 1024..=65535) => Ok(port.to_string()),
            _ => Err("A number from 1024 to 65535: Android apps can't listen below 1024".into()),
        },
        Kind::Lines => {
            let lines: Vec<&str> = value
                .as_str()
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|it| !it.is_empty())
                .collect();
            Ok(string(&lines.join("\n")))
        }
    }
}

/// The config file's text with a setting changed, or why the value doesn't do.
pub fn set(content: &str, section: &str, key: &str, value: &Value) -> Result<String, String> {
    let setting = find(section, key).ok_or_else(|| format!("No setting {section}.{key}"))?;
    let literal = literal(setting, value)?;
    Ok(config::with_value(content, section, key, &literal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FILE: &str = "# Local Desktop's config\n\
                        [desktop]\n\
                        preset = \"plasma\" # chosen at setup\n\
                        \n\
                        [user]\n\
                        username = \"tester\"\n";

    #[test]
    fn should_show_the_files_values_and_the_defaults() {
        let shown = values(&config::parse_text(FILE));
        assert_eq!(shown["desktop.preset"], "plasma");
        assert_eq!(shown["user.username"], "tester");
        assert_eq!(shown["clipboard.sync"], true);
        assert_eq!(shown["performance.cpu_boost"], "balanced");
        assert_eq!(shown["ssh.port"], 8022);
        // An empty preset is the default one.
        let shown = values(&config::parse_text("[desktop]\npreset = \"\"\n"));
        assert_eq!(shown["desktop.preset"], "xfce");
    }

    #[test]
    fn should_change_one_line_and_keep_the_rest() {
        let changed = set(FILE, "desktop", "preset", &json!("xfce")).unwrap();
        assert_eq!(changed, FILE.replace("\"plasma\"", "\"xfce\""));
        let changed = set(FILE, "clipboard", "sync", &json!(false)).unwrap();
        assert_eq!(changed, format!("{FILE}\n[clipboard]\nsync = false\n"));
        let keys = json!("ssh-ed25519 AAAA one\n\n  ssh-ed25519 BBBB two  \n");
        let changed = set(FILE, "ssh", "authorized_keys", &keys).unwrap();
        assert!(changed.ends_with(
            "[ssh]\nauthorized_keys = \"ssh-ed25519 AAAA one\\nssh-ed25519 BBBB two\"\n"
        ));
        let read = config::parse_text(&changed);
        assert_eq!(
            read.ssh.keys(),
            ["ssh-ed25519 AAAA one", "ssh-ed25519 BBBB two"]
        );
    }

    #[test]
    fn should_refuse_what_the_app_cant_go_by() {
        assert!(set(FILE, "desktop", "preset", &json!("gnome")).is_err());
        assert!(set(FILE, "user", "username", &json!("Peanut")).is_err());
        assert!(set(FILE, "user", "username", &json!("root")).is_ok());
        assert!(set(FILE, "ssh", "port", &json!(22)).is_err());
        assert!(set(FILE, "ssh", "port", &json!(2222)).is_ok());
        assert!(set(FILE, "clipboard", "sync", &json!("yes")).is_err());
        assert!(set(FILE, "command", "launch", &json!("sh")).is_err());
    }

    #[test]
    fn should_tell_what_waits_for_a_restart() {
        let running = config::parse_text(FILE);
        let file = config::parse_text(&set(FILE, "clipboard", "sync", &json!(false)).unwrap());
        assert_eq!(
            pending(&running, &file),
            [("clipboard.sync".to_string(), Applies::Restart)]
        );
        let file = config::parse_text(&set(FILE, "user", "username", &json!("alice")).unwrap());
        assert_eq!(
            pending(&running, &file),
            [("user.username".to_string(), Applies::NextStart)]
        );
        assert!(pending(&running, &running).is_empty());
    }
}
