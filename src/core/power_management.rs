//! What Plasma's battery widget asks PowerDevil, answered by the app (`guest::power`). The preset
//! leaves PowerDevil out (Android manages power, and PowerDevil's display power-off blanked the
//! screen), but Plasma's system tray shows the widget only while PowerDevil's service is on the
//! session bus.
//!
//! The battery itself comes from UPower (`core::upower`); here are the remaining time and the
//! inhibitions. Blocking sleep, with the widget's switch or by any program through
//! `org.freedesktop.PowerManagement.Inhibit`, keeps Android's screen on: a phone left alone sleeps
//! when its screen goes off.

use super::bus::{self, errors, fail, invalid_args, reply, Reply, Service, Signal};
use super::dbus::{Message, Writer};
use super::upower::AndroidBattery;
use std::collections::BTreeMap;

const SOLID: &str = "org.kde.Solid.PowerManagement";
const POLICY_AGENT: &str = "org.kde.Solid.PowerManagement.PolicyAgent";
const BUTTONS: &str = "org.kde.Solid.PowerManagement.Actions.HandleButtonEvents";
const FDO: &str = "org.freedesktop.PowerManagement";
const INHIBIT: &str = "org.freedesktop.PowerManagement.Inhibit";

const SOLID_PATH: &str = "/org/kde/Solid/PowerManagement";
const POLICY_AGENT_PATH: &str = "/org/kde/Solid/PowerManagement/PolicyAgent";
const BUTTONS_PATH: &str = "/org/kde/Solid/PowerManagement/Actions/HandleButtonEvents";
const FDO_PATH: &str = "/org/freedesktop/PowerManagement";
const INHIBIT_PATH: &str = "/org/freedesktop/PowerManagement/Inhibit";

/// The objects and the interface of each, as PowerDevil has them.
const OBJECTS: [(&str, &str); 5] = [
    (SOLID_PATH, SOLID),
    (POLICY_AGENT_PATH, POLICY_AGENT),
    (BUTTONS_PATH, BUTTONS),
    (FDO_PATH, FDO),
    (INHIBIT_PATH, INHIBIT),
];

/// What a program asked for when it blocked sleep.
#[derive(Debug, Clone, PartialEq)]
struct Inhibition {
    /// Its connection: the inhibition ends with it.
    owner: String,
    app: String,
    reason: String,
}

#[derive(Debug, Default)]
pub struct PowerManagement {
    /// How long until full while charging, until empty while discharging, in ms; 0 for "don't
    /// know".
    remaining_ms: u64,
    inhibitions: BTreeMap<u32, Inhibition>,
    cookie: u32,
    /// What the calls since the last look have to tell everybody.
    signals: Vec<Signal>,
}

impl PowerManagement {
    pub fn new() -> Self {
        Self::default()
    }

    /// Android's battery changed: what to tell the desktop.
    pub fn battery(&mut self, battery: &AndroidBattery) -> Vec<Signal> {
        let remaining = if battery.charging() {
            battery.time_to_full_ms
        } else if battery.discharging() {
            battery.time_to_empty_ms
        } else {
            None
        };
        let remaining = remaining.filter(|it| *it > 0).unwrap_or(0) as u64;
        if remaining == self.remaining_ms {
            return Vec::new();
        }
        self.remaining_ms = remaining;
        [
            "batteryRemainingTimeChanged",
            "smoothedBatteryRemainingTimeChanged",
        ]
        .into_iter()
        .map(|member| {
            signal(SOLID_PATH, SOLID, member, "t", |body| {
                body.u64(remaining);
            })
        })
        .collect()
    }

    /// Whether something on the desktop blocks sleep.
    pub fn inhibited(&self) -> bool {
        !self.inhibitions.is_empty()
    }

    /// The connection `owner` has left the bus, and its inhibitions with it.
    pub fn gone(&mut self, owner: &str) {
        let before = self.inhibited();
        self.inhibitions.retain(|_, it| it.owner != owner);
        self.inhibition_changed(before);
    }

    /// What the calls since the last look have to tell everybody.
    pub fn signals(&mut self) -> Vec<Signal> {
        std::mem::take(&mut self.signals)
    }

    /// The programs that block sleep, and why.
    pub fn holders(&self) -> Vec<(&str, &str)> {
        self.inhibitions
            .values()
            .map(|it| (it.app.as_str(), it.reason.as_str()))
            .collect()
    }

    fn inhibit(&mut self, owner: &str, app: String, reason: String) -> u32 {
        let before = self.inhibited();
        self.cookie = self.cookie.wrapping_add(1).max(1);
        let inhibition = Inhibition {
            owner: owner.into(),
            app,
            reason,
        };
        self.inhibitions.insert(self.cookie, inhibition);
        self.inhibition_changed(before);
        self.cookie
    }

    fn release(&mut self, cookie: u32) {
        let before = self.inhibited();
        self.inhibitions.remove(&cookie);
        self.inhibition_changed(before);
    }

    fn inhibition_changed(&mut self, before: bool) {
        let now = self.inhibited();
        if now == before {
            return;
        }
        for path in [FDO_PATH, INHIBIT_PATH] {
            self.signals
                .push(signal(path, INHIBIT, "HasInhibitChanged", "b", |body| {
                    body.boolean(now);
                }));
        }
    }

    fn introspect(path: &str) -> Option<String> {
        let own = OBJECTS
            .iter()
            .find(|(it, _)| *it == path)
            .map(|(_, it)| *it);
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        let mut children: Vec<&str> = OBJECTS
            .iter()
            .filter_map(|(it, _)| it.strip_prefix(prefix.as_str()))
            .filter_map(|rest| rest.split('/').next())
            .collect();
        children.dedup();
        if own.is_none() && children.is_empty() {
            return None;
        }
        let mut xml = String::from(
            "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\"\n\
             \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n<node>\n",
        );
        if let Some(interface) = own {
            xml += &format!("  <interface name=\"{interface}\"/>\n");
        }
        for child in children {
            xml += &format!("  <node name=\"{child}\"/>\n");
        }
        xml += "</node>\n";
        Some(xml)
    }
}

impl Service for PowerManagement {
    /// The one Plasma's tray watches for last, once the others answer.
    fn names(&self) -> &'static [&'static str] {
        &[FDO, INHIBIT, POLICY_AGENT, SOLID]
    }

    fn call(&mut self, call: &Message) -> Reply {
        let header = &call.header;
        let path = header.path.clone().unwrap_or_default();
        let member = header.member.clone().unwrap_or_default();
        let owner = header.sender.clone().unwrap_or_default();
        let own = OBJECTS
            .iter()
            .find(|(it, _)| *it == path)
            .map(|(_, it)| *it);
        let interface = header.interface.clone();
        if interface.as_deref() == Some(bus::INTROSPECTABLE) && member == "Introspect" {
            return match Self::introspect(&path) {
                Some(xml) => reply("s", |body| {
                    body.string(&xml);
                }),
                None => fail(
                    errors::UNKNOWN_OBJECT,
                    format!("No such object path '{path}'"),
                ),
            };
        }
        if interface.as_deref() == Some(bus::PEER) && member == "Ping" {
            return reply("", |_| {});
        }
        let Some(own) = own else {
            return fail(
                errors::UNKNOWN_OBJECT,
                format!("No such object path '{path}'"),
            );
        };
        let mut args = call.arguments();
        let no = |_: &mut Writer| {};
        match (
            path.as_str(),
            interface.as_deref().unwrap_or(own),
            member.as_str(),
        ) {
            (SOLID_PATH, SOLID, "batteryRemainingTime" | "smoothedBatteryRemainingTime") => {
                let remaining = self.remaining_ms;
                reply("t", |body| {
                    body.u64(remaining);
                })
            }
            // PowerDevil's own default: no charge limit.
            (SOLID_PATH, SOLID, "chargeStopThreshold") => reply("i", |body| {
                body.i32(100);
            }),
            (SOLID_PATH, SOLID, "chargeStartThreshold") => reply("i", |body| {
                body.i32(0);
            }),
            (
                SOLID_PATH,
                SOLID,
                "isLidPresent" | "isLidClosed" | "isActionSupported" | "hasDualGpu",
            )
            | (BUTTONS_PATH, BUTTONS, "triggersLidAction")
            | (FDO_PATH, FDO, "GetPowerSaveStatus" | "CanSuspend" | "CanHibernate")
            | (FDO_PATH, FDO, "CanHybridSuspend" | "CanSuspendThenHibernate") => {
                reply("b", |body| {
                    body.boolean(false);
                })
            }
            (FDO_PATH | INHIBIT_PATH, INHIBIT, "HasInhibit")
            | (POLICY_AGENT_PATH, POLICY_AGENT, "HasInhibition") => {
                let inhibited = self.inhibited();
                reply("b", |body| {
                    body.boolean(inhibited);
                })
            }
            (FDO_PATH | INHIBIT_PATH, INHIBIT, "Inhibit") => {
                let app = args.string().map_err(invalid_args)?;
                let reason = args.string().map_err(invalid_args)?;
                let cookie = self.inhibit(&owner, app, reason);
                reply("u", |body| {
                    body.u32(cookie);
                })
            }
            (POLICY_AGENT_PATH, POLICY_AGENT, "AddInhibition") => {
                args.u32().map_err(invalid_args)?;
                let app = args.string().map_err(invalid_args)?;
                let reason = args.string().map_err(invalid_args)?;
                let cookie = self.inhibit(&owner, app, reason);
                reply("u", |body| {
                    body.u32(cookie);
                })
            }
            (FDO_PATH | INHIBIT_PATH, INHIBIT, "UnInhibit")
            | (POLICY_AGENT_PATH, POLICY_AGENT, "ReleaseInhibition") => {
                let cookie = args.u32().map_err(invalid_args)?;
                self.release(cookie);
                reply("", no)
            }
            // Who blocks what isn't listed or managed here.
            (POLICY_AGENT_PATH, POLICY_AGENT, "ListInhibitions") => reply("a(ss)", |body| {
                body.array(8, |_| {});
            }),
            (POLICY_AGENT_PATH, POLICY_AGENT, "SetInhibitionAllowed") => reply("", no),
            (POLICY_AGENT_PATH, bus::PROPERTIES, "Get") => {
                let interface = args.string().map_err(invalid_args)?;
                let name = args.string().map_err(invalid_args)?;
                if interface != POLICY_AGENT || name != "RequestedInhibitions" {
                    return fail(
                        errors::UNKNOWN_PROPERTY,
                        format!("No such property '{name}'"),
                    );
                }
                reply("v", |body| {
                    body.signature("a(ssssu)").array(8, |_| {});
                })
            }
            (POLICY_AGENT_PATH, bus::PROPERTIES, "GetAll") => {
                let interface = args.string().map_err(invalid_args)?;
                reply("a{sv}", |body| {
                    body.array(8, |dict| {
                        if interface == POLICY_AGENT {
                            dict.string("RequestedInhibitions")
                                .signature("a(ssssu)")
                                .array(8, |_| {});
                        }
                    });
                })
            }
            (_, bus::PROPERTIES, "GetAll") => reply("a{sv}", |body| {
                body.array(8, |_| {});
            }),
            (FDO_PATH, FDO, "Suspend" | "Hibernate") => fail(
                errors::NOT_SUPPORTED,
                "Android decides when the phone sleeps",
            ),
            (_, interface, member) => fail(
                errors::UNKNOWN_METHOD,
                format!("Unknown method '{member}' or interface '{interface}'."),
            ),
        }
    }
}

fn signal(
    path: &str,
    interface: &str,
    member: &str,
    signature: &str,
    write: impl FnOnce(&mut Writer),
) -> Signal {
    let mut body = Writer::new();
    write(&mut body);
    Signal {
        path: path.into(),
        interface: interface.into(),
        member: member.into(),
        signature: signature.into(),
        body: body.into_bytes(),
        args: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dbus::{self, METHOD_CALL};

    fn charging(time_to_full_ms: Option<i64>) -> AndroidBattery {
        AndroidBattery {
            present: true,
            level: 40,
            scale: 100,
            status: 2,
            plugged: 1,
            time_to_full_ms,
            ..AndroidBattery::default()
        }
    }

    /// A call from `sender` with string and number arguments, as the session bus delivers it.
    fn call(
        sender: &str,
        path: &str,
        interface: &str,
        member: &str,
        write: impl FnOnce(&mut Writer),
        signature: &str,
    ) -> Message {
        let mut body = Writer::new();
        write(&mut body);
        let bytes = dbus::method_call(
            1,
            SOLID,
            path,
            interface,
            member,
            signature,
            &body.into_bytes(),
            0,
        );
        let mut message = dbus::parse(&bytes).unwrap();
        assert_eq!(message.header.kind, METHOD_CALL);
        message.header.sender = Some(sender.into());
        message
    }

    fn inhibit(power: &mut PowerManagement, sender: &str) -> u32 {
        let message = call(
            sender,
            INHIBIT_PATH,
            INHIBIT,
            "Inhibit",
            |body| {
                body.string("org.kde.plasmashell").string(
                    "The battery applet has enabled suppression of sleep and screen locking",
                );
            },
            "ss",
        );
        let (signature, body) = power.call(&message).unwrap();
        assert_eq!(signature, "u");
        u32::from_le_bytes(body[..4].try_into().unwrap())
    }

    fn has_inhibit_changed(signals: &[Signal]) -> Vec<(&str, bool)> {
        signals
            .iter()
            .filter(|it| it.member == "HasInhibitChanged")
            .map(|it| (it.path.as_str(), it.body[0] == 1))
            .collect()
    }

    #[test]
    fn should_tell_the_time_until_full_while_charging() {
        let mut power = PowerManagement::new();
        let signals = power.battery(&charging(Some(3_600_000)));
        let members: Vec<&str> = signals.iter().map(|it| it.member.as_str()).collect();
        assert_eq!(
            members,
            [
                "batteryRemainingTimeChanged",
                "smoothedBatteryRemainingTimeChanged"
            ]
        );
        assert_eq!(signals[0].body, 3_600_000u64.to_le_bytes());
        let asked = call(
            ":1.5",
            SOLID_PATH,
            SOLID,
            "batteryRemainingTime",
            |_| {},
            "",
        );
        assert_eq!(power.call(&asked).unwrap().1, 3_600_000u64.to_le_bytes());
        // Nothing new, nothing to tell.
        assert!(power.battery(&charging(Some(3_600_000))).is_empty());
        // Draining: as long as Android predicts, if it does.
        let draining = AndroidBattery {
            status: 3,
            plugged: 0,
            time_to_empty_ms: Some(18_000_000),
            ..charging(Some(3_600_000))
        };
        assert_eq!(
            power.battery(&draining)[0].body,
            18_000_000u64.to_le_bytes()
        );
        let unknown = AndroidBattery {
            time_to_empty_ms: None,
            ..draining
        };
        assert_eq!(power.battery(&unknown)[0].body, 0u64.to_le_bytes());
        let threshold = call(":1.5", SOLID_PATH, SOLID, "chargeStopThreshold", |_| {}, "");
        assert_eq!(
            power.call(&threshold).unwrap(),
            ("i".to_string(), 100i32.to_le_bytes().to_vec())
        );
    }

    #[test]
    fn should_block_sleep_while_anybody_asks_for_it() {
        let mut power = PowerManagement::new();
        assert!(!power.inhibited());
        // The widget's switch.
        let widget = inhibit(&mut power, ":1.5");
        assert!(power.inhibited());
        assert_eq!(
            has_inhibit_changed(&power.signals()),
            [(FDO_PATH, true), (INHIBIT_PATH, true)]
        );
        assert_eq!(power.holders()[0].0, "org.kde.plasmashell");
        // A video player too: still blocked, nothing new to tell.
        inhibit(&mut power, ":1.9");
        assert!(power.signals().is_empty());
        let off = call(
            ":1.5",
            INHIBIT_PATH,
            INHIBIT,
            "UnInhibit",
            |body| {
                body.u32(widget);
            },
            "u",
        );
        assert_eq!(power.call(&off).unwrap().0, "");
        assert!(power.inhibited());
        assert!(power.signals().is_empty());
        // The player quits without saying so.
        power.gone(":1.9");
        assert!(!power.inhibited());
        assert_eq!(
            has_inhibit_changed(&power.signals()),
            [(FDO_PATH, false), (INHIBIT_PATH, false)]
        );
        let has = call(":1.5", FDO_PATH, INHIBIT, "HasInhibit", |_| {}, "");
        assert_eq!(power.call(&has).unwrap().1, 0u32.to_le_bytes());
    }

    #[test]
    fn should_answer_the_rest_of_what_the_widget_asks() {
        let mut power = PowerManagement::new();
        let lid = call(":1.5", SOLID_PATH, SOLID, "isLidPresent", |_| {}, "");
        assert_eq!(power.call(&lid).unwrap().1, 0u32.to_le_bytes());
        // The requested inhibitions: a variant of an empty a(ssssu).
        let get = call(
            ":1.5",
            POLICY_AGENT_PATH,
            bus::PROPERTIES,
            "Get",
            |body| {
                body.string(POLICY_AGENT).string("RequestedInhibitions");
            },
            "ss",
        );
        let (signature, body) = power.call(&get).unwrap();
        assert_eq!(signature, "v");
        assert_eq!(&body[..10], b"\x08a(ssssu)\0");
        assert_eq!(&body[12..16], &[0, 0, 0, 0]);

        let suspend = call(":1.5", FDO_PATH, FDO, "Suspend", |_| {}, "");
        assert_eq!(power.call(&suspend).unwrap_err().0, errors::NOT_SUPPORTED);
        let nowhere = call(
            ":1.5",
            "/org/kde/Solid/Elsewhere",
            SOLID,
            "isLidPresent",
            |_| {},
            "",
        );
        assert_eq!(power.call(&nowhere).unwrap_err().0, errors::UNKNOWN_OBJECT);
        let tree = call(
            ":1.5",
            "/org/kde/Solid/PowerManagement",
            bus::INTROSPECTABLE,
            "Introspect",
            |_| {},
            "",
        );
        let (_, body) = power.call(&tree).unwrap();
        let xml = String::from_utf8_lossy(&body);
        assert!(xml.contains("<interface name=\"org.kde.Solid.PowerManagement\"/>"));
        assert!(
            xml.contains("<node name=\"PolicyAgent\"/>")
                && xml.contains("<node name=\"Actions\"/>")
        );
    }
}
