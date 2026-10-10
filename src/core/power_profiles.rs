//! Power profiles for the desktop, as power-profiles-daemon has them: power saver, balanced and
//! performance, which stand for `[performance] cpu_boost` `off`, `balanced` and `max`.
//!
//! Plasma's battery widget shows its profile switch only while power-profiles-daemon's service
//! (`org.freedesktop.UPower.PowerProfiles`) is on the system bus, and then switches through
//! PowerDevil (`core::power_management`). Programs that ask the service itself, such as
//! `powerprofilesctl` or GNOME's settings, switch through it here. Either way the app puts the
//! profile in effect and saves it in the config (`android::power_profile`).

use super::bus::{self, errors, fail, invalid_args, reply, Reply, Service, Signal};
use super::dbus::{Message, Value, Writer};

pub const NAME: &str = "org.freedesktop.UPower.PowerProfiles";
pub const PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
/// The power-profiles-daemon whose interface this is.
const VERSION: &str = "0.30";

/// The properties and their types.
const PROPERTIES: [(&str, &str); 7] = [
    ("ActiveProfile", "s"),
    ("PerformanceInhibited", "s"),
    ("PerformanceDegraded", "s"),
    ("Profiles", "aa{sv}"),
    ("Actions", "as"),
    ("ActiveProfileHolds", "aa{sv}"),
    ("Version", "s"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    PowerSaver,
    #[default]
    Balanced,
    Performance,
}

impl Profile {
    pub const ALL: [Self; 3] = [Self::PowerSaver, Self::Balanced, Self::Performance];

    pub fn name(self) -> &'static str {
        match self {
            Self::PowerSaver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|it| it.name() == name)
    }

    /// The `[performance] cpu_boost` it stands for.
    pub fn cpu_boost(self) -> &'static str {
        match self {
            Self::PowerSaver => "off",
            Self::Balanced => "balanced",
            Self::Performance => "max",
        }
    }

    /// The profile `cpu_boost` amounts to: anything else is balanced, as for the boost itself.
    pub fn from_cpu_boost(cpu_boost: &str) -> Self {
        match cpu_boost.trim() {
            "off" => Self::PowerSaver,
            "max" => Self::Performance,
            _ => Self::Balanced,
        }
    }
}

/// power-profiles-daemon's service.
pub struct PowerProfiles {
    active: Profile,
    /// What a program asked for, until the app puts it in effect (`set`).
    requested: Option<Profile>,
}

impl PowerProfiles {
    pub fn new(active: Profile) -> Self {
        Self {
            active,
            requested: None,
        }
    }

    /// The profile a program asked for since the last look.
    pub fn requested(&mut self) -> Option<Profile> {
        self.requested.take()
    }

    /// `profile` is in effect now: what to tell the desktop.
    pub fn set(&mut self, profile: Profile) -> Vec<Signal> {
        if profile == self.active {
            return Vec::new();
        }
        self.active = profile;
        let changed = [("ActiveProfile", Value::Str(profile.name().into()))];
        vec![bus::properties_changed(PATH, NAME, &changed)]
    }

    fn write(&self, property: &str, writer: &mut Writer) {
        match property {
            "ActiveProfile" => {
                writer.string(self.active.name());
            }
            "Profiles" => {
                writer.array(4, |profiles| {
                    for profile in Profile::ALL {
                        profiles.dict(&[
                            ("Profile", Value::Str(profile.name().into())),
                            ("Driver", Value::Str("local-desktop".into())),
                            ("PlatformDriver", Value::Str("local-desktop".into())),
                        ]);
                    }
                });
            }
            "Actions" => {
                writer.strings(&[]);
            }
            "ActiveProfileHolds" => {
                writer.array(4, |_| {});
            }
            "Version" => {
                writer.string(VERSION);
            }
            // Nothing holds performance back here.
            _ => {
                writer.string("");
            }
        }
    }

    /// The object's interfaces, for `Introspect`; `None` for paths that aren't its.
    pub fn introspect(path: &str) -> Option<String> {
        if path != PATH {
            return None;
        }
        let mut xml = String::from(
            "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\"\n\
             \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n<node>\n",
        );
        xml += &format!("  <interface name=\"{NAME}\">\n");
        for (name, signature) in PROPERTIES {
            let access = if name == "ActiveProfile" {
                "readwrite"
            } else {
                "read"
            };
            xml += &format!(
                "    <property name=\"{name}\" type=\"{signature}\" access=\"{access}\"/>\n"
            );
        }
        xml +=
            "    <method name=\"HoldProfile\"><arg name=\"profile\" type=\"s\" direction=\"in\"/>\
                <arg name=\"reason\" type=\"s\" direction=\"in\"/>\
                <arg name=\"application_id\" type=\"s\" direction=\"in\"/>\
                <arg name=\"cookie\" type=\"u\" direction=\"out\"/></method>\n";
        xml += "    <method name=\"ReleaseProfile\"><arg name=\"cookie\" type=\"u\" direction=\"in\"/></method>\n";
        xml += "    <signal name=\"ProfileReleased\"><arg name=\"cookie\" type=\"u\"/></signal>\n";
        xml += "  </interface>\n</node>\n";
        Some(xml)
    }
}

impl Service for PowerProfiles {
    fn names(&self) -> &'static [&'static str] {
        &[NAME]
    }

    fn call(&mut self, call: &Message) -> Reply {
        let header = &call.header;
        let path = header.path.as_deref().unwrap_or_default();
        let member = header.member.as_deref().unwrap_or_default();
        let interface = header.interface.as_deref().unwrap_or(NAME);
        if path != PATH {
            return fail(
                errors::UNKNOWN_OBJECT,
                format!("No such object path '{path}'"),
            );
        }
        let mut args = call.arguments();
        match (interface, member) {
            (bus::INTROSPECTABLE, "Introspect") => {
                let xml = Self::introspect(path).unwrap_or_default();
                reply("s", |body| {
                    body.string(&xml);
                })
            }
            (bus::PEER, "Ping") => reply("", |_| {}),
            (bus::PROPERTIES, "Get") => {
                let interface = args.string().map_err(invalid_args)?;
                let name = args.string().map_err(invalid_args)?;
                let property = PROPERTIES.iter().find(|it| it.0 == name);
                match property.filter(|_| interface == NAME) {
                    Some((name, signature)) => reply("v", |body| {
                        body.signature(signature);
                        self.write(name, body);
                    }),
                    None => fail(
                        errors::UNKNOWN_PROPERTY,
                        format!("No such property '{name}'"),
                    ),
                }
            }
            (bus::PROPERTIES, "GetAll") => {
                let interface = args.string().map_err(invalid_args)?;
                reply("a{sv}", |body| {
                    body.array(8, |dict| {
                        if interface != NAME {
                            return;
                        }
                        for (name, signature) in PROPERTIES {
                            dict.entry(name, signature, |value| self.write(name, value));
                        }
                    });
                })
            }
            (bus::PROPERTIES, "Set") => {
                let _interface = args.string().map_err(invalid_args)?;
                let name = args.string().map_err(invalid_args)?;
                if name != "ActiveProfile" {
                    return fail(
                        errors::PROPERTY_READ_ONLY,
                        format!("Property '{name}' is read-only"),
                    );
                }
                let profile = match args.variant().map_err(invalid_args)? {
                    Value::Str(it) => Profile::from_name(&it),
                    _ => None,
                };
                let Some(profile) = profile else {
                    return fail(errors::INVALID_ARGS, "Invalid profile name");
                };
                self.requested = Some(profile);
                reply("", |_| {})
            }
            (NAME, "HoldProfile") => fail(
                errors::NOT_SUPPORTED,
                "Holding a profile isn't supported; set the profile instead",
            ),
            (NAME, "ReleaseProfile") => fail(errors::INVALID_ARGS, "No such hold"),
            (_, member) => fail(
                errors::UNKNOWN_METHOD,
                format!("Unknown method '{member}' or interface '{interface}'."),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dbus;

    fn call(
        interface: &str,
        member: &str,
        signature: &str,
        write: impl FnOnce(&mut Writer),
    ) -> Message {
        let mut body = Writer::new();
        write(&mut body);
        let bytes = dbus::method_call(
            1,
            NAME,
            PATH,
            interface,
            member,
            signature,
            &body.into_bytes(),
            0,
        );
        let mut message = dbus::parse(&bytes).unwrap();
        message.header.sender = Some(":1.4".into());
        message
    }

    #[test]
    fn should_stand_for_the_cpu_boost() {
        for profile in Profile::ALL {
            assert_eq!(Profile::from_cpu_boost(profile.cpu_boost()), profile);
            assert_eq!(Profile::from_name(profile.name()), Some(profile));
        }
        assert_eq!(Profile::from_cpu_boost(" max "), Profile::Performance);
        assert_eq!(Profile::from_cpu_boost("turbo"), Profile::Balanced);
        assert_eq!(Profile::from_name("turbo"), None);
    }

    #[test]
    fn should_tell_and_take_the_active_profile() {
        let mut profiles = PowerProfiles::new(Profile::Balanced);
        let get = call(bus::PROPERTIES, "Get", "ss", |body| {
            body.string(NAME).string("ActiveProfile");
        });
        let (signature, body) = profiles.call(&get).unwrap();
        assert_eq!(signature, "v");
        assert_eq!(&body[..2], b"\x01s");
        assert!(body.ends_with(b"balanced\0"));

        let all = call(bus::PROPERTIES, "GetAll", "s", |body| {
            body.string(NAME);
        });
        let (_, body) = profiles.call(&all).unwrap();
        let text = String::from_utf8_lossy(&body);
        for (name, _) in PROPERTIES {
            assert!(text.contains(name), "{name}");
        }
        assert!(text.contains("power-saver") && text.contains("performance"));

        // A program asks; the app puts it in effect, then everybody hears of it.
        let set = |name: &str| {
            call(bus::PROPERTIES, "Set", "ssv", |body| {
                body.string(NAME)
                    .string("ActiveProfile")
                    .variant(&Value::Str(name.into()));
            })
        };
        assert_eq!(profiles.call(&set("power-saver")).unwrap().0, "");
        assert_eq!(profiles.requested(), Some(Profile::PowerSaver));
        assert_eq!(profiles.requested(), None);
        let signals = profiles.set(Profile::PowerSaver);
        assert_eq!(signals[0].member, "PropertiesChanged");
        assert_eq!(signals[0].args, [NAME]);
        assert!(profiles.set(Profile::PowerSaver).is_empty());

        assert_eq!(
            profiles.call(&set("turbo")).unwrap_err().0,
            errors::INVALID_ARGS
        );
        let version = call(bus::PROPERTIES, "Set", "ssv", |body| {
            body.string(NAME)
                .string("Version")
                .variant(&Value::Str("1".into()));
        });
        assert_eq!(
            profiles.call(&version).unwrap_err().0,
            errors::PROPERTY_READ_ONLY
        );
        let hold = call(NAME, "HoldProfile", "sss", |body| {
            body.string("performance")
                .string("game")
                .string("org.example");
        });
        assert_eq!(profiles.call(&hold).unwrap_err().0, errors::NOT_SUPPORTED);
    }
}
