//! Android's battery as UPower describes batteries, for the desktop's battery widget: Plasma's
//! (through Solid), Xfce's power manager, GNOME's and Chromium's battery API all ask UPower on the
//! system bus. The bus is `core::bus`; Android's side is `android::battery`.
//! https://upower.freedesktop.org/docs/
//!
//! There are three devices, as UPower has them on a laptop: the battery, the charger ("line
//! power") and the display device that sums the batteries up for icons.

use super::bus::{self, errors, fail, invalid_args, reply, Reply, Service, Signal};
use super::dbus::{Message, Value, Writer};

pub const NAME: &str = "org.freedesktop.UPower";
const PATH: &str = "/org/freedesktop/UPower";
const DEVICES: &str = "/org/freedesktop/UPower/devices";
const DEVICE: &str = "org.freedesktop.UPower.Device";
const BATTERY: &str = "/org/freedesktop/UPower/devices/battery_BAT0";
const LINE_POWER: &str = "/org/freedesktop/UPower/devices/line_power_AC";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
/// The UPower whose interfaces these are.
const VERSION: &str = "1.91.5";

/// UPower's device types, states, technologies and warning levels.
const KIND_LINE_POWER: u32 = 1;
const KIND_BATTERY: u32 = 2;
const STATE_UNKNOWN: u32 = 0;
const STATE_CHARGING: u32 = 1;
const STATE_DISCHARGING: u32 = 2;
const STATE_FULLY_CHARGED: u32 = 4;
const STATE_PENDING_CHARGE: u32 = 5;
const LEVEL_NONE: u32 = 1;
const LEVEL_LOW: u32 = 3;
const LEVEL_CRITICAL: u32 = 4;
const LEVEL_ACTION: u32 = 5;

/// What Android says about its battery: `ACTION_BATTERY_CHANGED`'s extras and `BatteryManager`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AndroidBattery {
    pub present: bool,
    pub level: i64,
    pub scale: i64,
    /// `BATTERY_STATUS_*`: 2 charging, 3 discharging, 4 not charging, 5 full.
    pub status: i64,
    /// `BATTERY_PLUGGED_*` bits, 0 on battery.
    pub plugged: i64,
    pub voltage_mv: i64,
    /// Tenths of a degree Celsius.
    pub temperature: i64,
    pub technology: String,
    /// What is left, in µAh, where Android tells.
    pub charge_uah: Option<i64>,
    /// What flows in or out, in µA, either sign.
    pub current_ua: Option<i64>,
    /// How long until full, while charging (Android 9 and later).
    pub time_to_full_ms: Option<i64>,
    /// How long until empty, as Android predicts it while discharging (Android 12 and later).
    pub time_to_empty_ms: Option<i64>,
    pub cycles: Option<i64>,
    /// What the battery held new, in µAh, from the phone's power profile.
    pub design_uah: Option<i64>,
}

impl AndroidBattery {
    pub fn percentage(&self) -> f64 {
        if self.scale <= 0 {
            return 0.0;
        }
        (self.level as f64 * 100.0 / self.scale as f64).clamp(0.0, 100.0)
    }

    pub fn charging(&self) -> bool {
        self.state() == STATE_CHARGING
    }

    pub fn discharging(&self) -> bool {
        self.state() == STATE_DISCHARGING
    }

    pub fn state(&self) -> u32 {
        match self.status {
            2 => STATE_CHARGING,
            3 => STATE_DISCHARGING,
            // Plugged in and holding: a charge limit (Samsung's "Protect battery" at 85 %).
            4 if self.plugged != 0 => STATE_PENDING_CHARGE,
            4 => STATE_DISCHARGING,
            5 => STATE_FULLY_CHARGED,
            _ => STATE_UNKNOWN,
        }
    }

    fn voltage(&self) -> f64 {
        self.voltage_mv as f64 / 1000.0
    }

    /// `charge` µAh in Wh, at the battery's voltage now.
    fn energy_of(&self, charge: f64) -> f64 {
        charge / 1e6 * self.voltage()
    }

    /// What is left, in Wh.
    fn energy(&self) -> f64 {
        match self.charge_uah {
            Some(charge) if charge > 0 => self.energy_of(charge as f64),
            _ => 0.0,
        }
    }

    /// What the battery holds when full, in µAh, as the charge left and the level say.
    fn full_charge(&self) -> Option<f64> {
        let charge = self.charge_uah.filter(|it| *it > 0)?;
        let percentage = self.percentage();
        (percentage > 0.0).then(|| charge as f64 * 100.0 / percentage)
    }

    /// In W. Android documents the current in µA, but Samsung's phones give mA (3012 while
    /// charging at 3 A). A phone that runs a desktop draws far more than 10 mA, so less than
    /// 10,000 is taken for mA.
    fn energy_rate(&self) -> f64 {
        let Some(current) = self.current_ua.map(i64::unsigned_abs) else {
            return 0.0;
        };
        let amperes = if current < 10_000 {
            current as f64 / 1e3
        } else {
            current as f64 / 1e6
        };
        amperes * self.voltage()
    }

    fn technology(&self) -> u32 {
        match self.technology.to_ascii_lowercase().as_str() {
            "li-ion" | "lion" => 1,
            "li-poly" | "lipo" => 2,
            "lifepo4" => 3,
            _ => 0,
        }
    }

    /// UPower's levels for a discharging battery: low at 20 %, critical at 5 %, action at 2 %.
    fn warning_level(&self) -> u32 {
        let percentage = self.percentage();
        match () {
            _ if self.state() != STATE_DISCHARGING => LEVEL_NONE,
            _ if percentage <= 2.0 => LEVEL_ACTION,
            _ if percentage <= 5.0 => LEVEL_CRITICAL,
            _ if percentage <= 20.0 => LEVEL_LOW,
            _ => LEVEL_NONE,
        }
    }

    /// UPower's icon for the battery.
    fn icon_name(&self) -> &'static str {
        let state = self.state();
        if !self.present {
            return "battery-missing-symbolic";
        }
        if state == STATE_FULLY_CHARGED {
            return "battery-full-charged-symbolic";
        }
        let charging = matches!(state, STATE_CHARGING | STATE_PENDING_CHARGE);
        match (self.percentage(), charging) {
            (it, true) if it <= 20.0 => "battery-caution-charging-symbolic",
            (it, false) if it <= 20.0 => "battery-caution-symbolic",
            (it, true) if it < 30.0 => "battery-low-charging-symbolic",
            (it, false) if it < 30.0 => "battery-low-symbolic",
            (it, true) if it < 60.0 => "battery-good-charging-symbolic",
            (it, false) if it < 60.0 => "battery-good-symbolic",
            (_, true) => "battery-full-charging-symbolic",
            (_, false) => "battery-full-symbolic",
        }
    }
}

type Properties = Vec<(&'static str, Value)>;

/// UPower's service: Android's battery as of the last update.
pub struct UPower {
    vendor: String,
    model: String,
    battery: Option<AndroidBattery>,
    /// When the battery was last updated, in seconds since 1970.
    updated: u64,
    /// What the battery holds when full (µAh), as told at the highest level so far, and that
    /// level: the level is a whole percent, which matters least there.
    full: Option<(f64, f64)>,
}

impl UPower {
    /// `vendor` and `model`: the phone's, which name the battery.
    pub fn new(vendor: &str, model: &str) -> Self {
        Self {
            vendor: vendor.into(),
            model: model.into(),
            battery: None,
            updated: 0,
            full: None,
        }
    }

    /// Android's battery changed (`now` in seconds since 1970). What to tell the desktop.
    pub fn update(&mut self, battery: AndroidBattery, now: u64) -> Vec<Signal> {
        let had = self.battery.is_some();
        let before: Vec<(&str, Properties)> = self
            .objects()
            .into_iter()
            .map(|path| (path, self.properties(path)))
            .collect();
        let percentage = battery.percentage();
        if let Some(full) = battery.full_charge() {
            if self.full.is_none_or(|(level, _)| percentage >= level) {
                self.full = Some((percentage, full));
            }
        }
        self.battery = Some(battery);
        self.updated = now;
        let mut signals = Vec::new();
        if !had {
            for path in [BATTERY, LINE_POWER] {
                signals.push(device_signal("DeviceAdded", path));
            }
        }
        for path in self.objects() {
            let after = self.properties(path);
            let old = before.iter().find(|(it, _)| *it == path).map(|(_, it)| it);
            let changed: Properties = after
                .into_iter()
                .filter(|(name, value)| {
                    // `UpdateTime` alone doesn't make a change worth telling.
                    *name != "UpdateTime"
                        && old
                            .is_none_or(|old| !old.iter().any(|it| it.0 == *name && it.1 == *value))
                })
                .collect();
            if changed.is_empty() || old.is_none() {
                continue;
            }
            let interface = if path == PATH { NAME } else { DEVICE };
            let mut changed = changed;
            if interface == DEVICE {
                changed.push(("UpdateTime", Value::U64(self.updated)));
            }
            signals.push(bus::properties_changed(path, interface, &changed));
        }
        signals
    }

    /// The objects there are, the daemon first.
    fn objects(&self) -> Vec<&'static str> {
        match self.battery {
            Some(_) => vec![PATH, BATTERY, LINE_POWER, DISPLAY_DEVICE],
            None => vec![PATH, DISPLAY_DEVICE],
        }
    }

    fn properties(&self, path: &str) -> Properties {
        let battery = self.battery.clone().unwrap_or_default();
        match path {
            PATH => vec![
                ("DaemonVersion", Value::Str(VERSION.into())),
                (
                    "OnBattery",
                    Value::Bool(battery.present && battery.plugged == 0),
                ),
                ("LidIsClosed", Value::Bool(false)),
                ("LidIsPresent", Value::Bool(false)),
            ],
            BATTERY => self.device(&battery, "BAT0", KIND_BATTERY),
            LINE_POWER => self.device(&battery, "AC", KIND_LINE_POWER),
            _ => self.device(&battery, "", KIND_BATTERY),
        }
    }

    /// A device's properties, all of UPower's.
    fn device(&self, battery: &AndroidBattery, native_path: &str, kind: u32) -> Properties {
        let is_battery = kind == KIND_BATTERY;
        let present = is_battery && battery.present;
        let charging = is_battery && battery.charging();
        let discharging = is_battery && battery.discharging();
        let full = self.full.map(|(_, full)| full);
        let design = battery.design_uah.filter(|it| *it > 0).map(|it| it as f64);
        let named = native_path == "BAT0";
        let text = |it: &str| Value::Str(if named { it.into() } else { String::new() });
        let number = |it: f64| Value::F64(if present { it } else { 0.0 });
        vec![
            ("NativePath", Value::Str(native_path.into())),
            ("Vendor", text(&self.vendor)),
            ("Model", text(&self.model)),
            ("Serial", Value::Str(String::new())),
            ("UpdateTime", Value::U64(self.updated)),
            ("Type", Value::U32(kind)),
            ("PowerSupply", Value::Bool(true)),
            ("HasHistory", Value::Bool(false)),
            ("HasStatistics", Value::Bool(false)),
            ("Online", Value::Bool(!is_battery && battery.plugged != 0)),
            ("Energy", number(battery.energy())),
            ("EnergyEmpty", Value::F64(0.0)),
            (
                "EnergyFull",
                number(full.map_or(0.0, |it| battery.energy_of(it))),
            ),
            (
                "EnergyFullDesign",
                number(design.map_or(0.0, |it| battery.energy_of(it))),
            ),
            ("EnergyRate", number(battery.energy_rate())),
            ("Voltage", number(battery.voltage())),
            (
                "ChargeCycles",
                Value::I32(battery.cycles.filter(|_| present).unwrap_or(-1) as i32),
            ),
            ("Luminosity", Value::F64(0.0)),
            (
                "TimeToEmpty",
                Value::I64(if discharging {
                    battery.time_to_empty_ms.unwrap_or(0).max(0) / 1000
                } else {
                    0
                }),
            ),
            (
                "TimeToFull",
                Value::I64(if charging {
                    battery.time_to_full_ms.unwrap_or(0).max(0) / 1000
                } else {
                    0
                }),
            ),
            ("Percentage", number(battery.percentage())),
            ("Temperature", number(battery.temperature as f64 / 10.0)),
            ("IsPresent", Value::Bool(present)),
            (
                "State",
                Value::U32(if present {
                    battery.state()
                } else {
                    STATE_UNKNOWN
                }),
            ),
            ("IsRechargeable", Value::Bool(is_battery)),
            // The battery's health: what it holds of what it held new, 0 for "don't know" as
            // UPower has it.
            (
                "Capacity",
                number(match (full, design) {
                    (Some(full), Some(design)) => (full * 100.0 / design).min(100.0),
                    _ => 0.0,
                }),
            ),
            (
                "Technology",
                Value::U32(if present { battery.technology() } else { 0 }),
            ),
            (
                "WarningLevel",
                Value::U32(if present {
                    battery.warning_level()
                } else {
                    LEVEL_NONE
                }),
            ),
            // The battery tells a percentage, not coarse levels.
            ("BatteryLevel", Value::U32(LEVEL_NONE)),
            (
                "IconName",
                Value::Str(
                    if is_battery {
                        battery.icon_name()
                    } else {
                        "ac-adapter-symbolic"
                    }
                    .into(),
                ),
            ),
            ("ChargeStartThreshold", Value::U32(0)),
            ("ChargeEndThreshold", Value::U32(100)),
            ("ChargeThresholdEnabled", Value::Bool(false)),
            ("ChargeThresholdSupported", Value::Bool(false)),
            ("ChargeThresholdSettingsSupported", Value::U32(0)),
            ("VoltageMinDesign", Value::F64(0.0)),
            ("VoltageMaxDesign", Value::F64(0.0)),
            ("CapacityLevel", Value::Str(String::new())),
        ]
    }

    fn interface_of(path: &str) -> &'static str {
        if path == PATH {
            NAME
        } else {
            DEVICE
        }
    }

    pub fn introspect(&self, path: &str) -> Option<String> {
        let objects = self.objects();
        let children: &[&str] = match path {
            "/" => &["org"],
            "/org" => &["freedesktop"],
            "/org/freedesktop" => &["UPower"],
            PATH => &["devices"],
            DEVICES if objects.contains(&BATTERY) => {
                &["battery_BAT0", "line_power_AC", "DisplayDevice"]
            }
            DEVICES => &["DisplayDevice"],
            _ if objects.contains(&path) => &[],
            _ => return None,
        };
        let mut xml = String::from(
            "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\"\n\
             \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n<node>\n",
        );
        if objects.contains(&path) {
            let interface = Self::interface_of(path);
            xml += &format!("  <interface name=\"{interface}\">\n");
            xml += if path == PATH {
                "    <method name=\"EnumerateDevices\"><arg name=\"devices\" direction=\"out\" type=\"ao\"/></method>\n\
                 \x20   <method name=\"GetDisplayDevice\"><arg name=\"device\" direction=\"out\" type=\"o\"/></method>\n\
                 \x20   <method name=\"GetCriticalAction\"><arg name=\"action\" direction=\"out\" type=\"s\"/></method>\n\
                 \x20   <signal name=\"DeviceAdded\"><arg name=\"device\" type=\"o\"/></signal>\n\
                 \x20   <signal name=\"DeviceRemoved\"><arg name=\"device\" type=\"o\"/></signal>\n"
            } else {
                "    <method name=\"Refresh\"/>\n"
            };
            for (name, value) in self.properties(path) {
                let signature = value.signature();
                xml += &format!(
                    "    <property name=\"{name}\" type=\"{signature}\" access=\"read\"/>\n"
                );
            }
            xml += "  </interface>\n";
            xml += "  <interface name=\"org.freedesktop.DBus.Properties\">\n\
                    \x20   <method name=\"Get\"><arg direction=\"in\" type=\"s\"/><arg direction=\"in\" type=\"s\"/><arg direction=\"out\" type=\"v\"/></method>\n\
                    \x20   <method name=\"GetAll\"><arg direction=\"in\" type=\"s\"/><arg direction=\"out\" type=\"a{sv}\"/></method>\n\
                    \x20   <signal name=\"PropertiesChanged\"><arg type=\"s\"/><arg type=\"a{sv}\"/><arg type=\"as\"/></signal>\n\
                    \x20 </interface>\n";
        }
        for child in children {
            xml += &format!("  <node name=\"{child}\"/>\n");
        }
        xml += "</node>\n";
        Some(xml)
    }
}

impl Service for UPower {
    fn names(&self) -> &'static [&'static str] {
        &[NAME]
    }

    fn call(&mut self, call: &Message) -> Reply {
        let path = call.header.path.clone().unwrap_or_default();
        let member = call.header.member.clone().unwrap_or_default();
        let interface = call.header.interface.clone();
        if interface.as_deref() == Some(bus::INTROSPECTABLE) && member == "Introspect" {
            return match self.introspect(&path) {
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
        if !self.objects().contains(&path.as_str()) {
            return fail(
                errors::UNKNOWN_OBJECT,
                format!("No such object path '{path}'"),
            );
        }
        let own = Self::interface_of(&path);
        let mut args = call.arguments();
        match (interface.as_deref().unwrap_or(own), member.as_str()) {
            (bus::PROPERTIES, "Get") => {
                let interface = args.string().map_err(invalid_args)?;
                let name = args.string().map_err(invalid_args)?;
                if interface != own {
                    return fail(
                        errors::UNKNOWN_INTERFACE,
                        format!("No such interface '{interface}'"),
                    );
                }
                match self
                    .properties(&path)
                    .into_iter()
                    .find(|(it, _)| *it == name)
                {
                    Some((_, value)) => reply("v", |body| {
                        body.variant(&value);
                    }),
                    None => fail(
                        errors::UNKNOWN_PROPERTY,
                        format!("No such property '{name}'"),
                    ),
                }
            }
            (bus::PROPERTIES, "GetAll") => {
                let interface = args.string().map_err(invalid_args)?;
                let properties = if interface == own {
                    self.properties(&path)
                } else {
                    Vec::new()
                };
                reply("a{sv}", |body| {
                    body.dict(&properties);
                })
            }
            (bus::PROPERTIES, "Set") => {
                fail(errors::PROPERTY_READ_ONLY, "The properties are read-only")
            }
            (NAME, "EnumerateDevices") if path == PATH => {
                let devices: Vec<&str> = self
                    .objects()
                    .into_iter()
                    .filter(|it| *it != PATH && *it != DISPLAY_DEVICE)
                    .collect();
                reply("ao", |body| {
                    body.strings(&devices);
                })
            }
            (NAME, "EnumerateKbdBacklights") if path == PATH => reply("ao", |body| {
                body.strings(&[]);
            }),
            (NAME, "GetDisplayDevice") if path == PATH => reply("o", |body| {
                body.string(DISPLAY_DEVICE);
            }),
            (NAME, "GetCriticalAction") if path == PATH => reply("s", |body| {
                body.string("PowerOff");
            }),
            (DEVICE, "Refresh") if path != PATH => reply("", |_| {}),
            // No history: `HasHistory` and `HasStatistics` say so.
            (DEVICE, "GetHistory") if path != PATH => reply("a(udu)", |body| {
                body.array(8, |_| {});
            }),
            (DEVICE, "GetStatistics") if path != PATH => reply("a(dd)", |body| {
                body.array(8, |_| {});
            }),
            (DEVICE, "EnableChargeThreshold") if path != PATH => {
                fail(errors::NOT_SUPPORTED, "Android keeps the charge limit")
            }
            (interface, member) => fail(
                errors::UNKNOWN_METHOD,
                format!("Unknown method '{member}' or interface '{interface}'."),
            ),
        }
    }
}

fn device_signal(member: &str, device: &str) -> Signal {
    let mut body = Writer::new();
    body.string(device);
    Signal {
        path: PATH.into(),
        interface: NAME.into(),
        member: member.into(),
        signature: "o".into(),
        body: body.into_bytes(),
        args: vec![device.into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dbus::{self, METHOD_RETURN};

    /// The S21 FE at 54 %, charging over USB.
    fn charging() -> AndroidBattery {
        AndroidBattery {
            present: true,
            level: 54,
            scale: 100,
            status: 2,
            plugged: 2,
            voltage_mv: 4000,
            temperature: 298,
            technology: "Li-ion".into(),
            charge_uah: Some(2_430_000),
            current_ua: Some(-1_500_000),
            time_to_full_ms: Some(3_600_000),
            time_to_empty_ms: None,
            cycles: None,
            design_uah: Some(5_000_000),
        }
    }

    fn call(path: &str, interface: &str, member: &str, args: &[&str]) -> Message {
        let mut body = Writer::new();
        for arg in args {
            body.string(arg);
        }
        let mut header = dbus::Header {
            kind: dbus::METHOD_CALL,
            serial: 1,
            path: Some(path.into()),
            interface: Some(interface.into()),
            member: Some(member.into()),
            destination: Some(NAME.into()),
            sender: Some(":1.1".into()),
            signature: "s".repeat(args.len()),
            ..dbus::Header::default()
        };
        let bytes = dbus::method_call(
            1,
            NAME,
            path,
            interface,
            member,
            &header.signature,
            &body.into_bytes(),
            0,
        );
        header.serial = 1;
        let mut message = dbus::parse(&bytes).unwrap();
        message.header.sender = header.sender;
        message
    }

    /// A reply's body as a message, to read its arguments.
    fn answer(reply: Reply) -> Message {
        let (signature, body) = reply.expect("an answer");
        let call = dbus::Header {
            serial: 1,
            sender: Some(":1.1".into()),
            ..dbus::Header::default()
        };
        let message =
            dbus::parse(&dbus::method_return(2, &call, ":1.0", &signature, &body)).unwrap();
        assert_eq!(message.header.kind, METHOD_RETURN);
        message
    }

    fn property(properties: &[(String, Value)], name: &str) -> Value {
        properties
            .iter()
            .find(|(it, _)| it == name)
            .expect(name)
            .1
            .clone()
    }

    #[test]
    fn should_describe_androids_battery_as_upower_does() {
        let battery = charging();
        assert_eq!(battery.percentage(), 54.0);
        assert_eq!(battery.state(), STATE_CHARGING);
        assert!((battery.energy() - 9.72).abs() < 1e-9);
        assert_eq!(battery.full_charge(), Some(4_500_000.0));
        assert!((battery.energy_rate() - 6.0).abs() < 1e-9);
        // The same 1.5 A as Samsung reports it.
        let samsung = AndroidBattery {
            current_ua: Some(1500),
            ..charging()
        };
        assert!((samsung.energy_rate() - 6.0).abs() < 1e-9);
        assert_eq!(battery.technology(), 1);
        assert_eq!(battery.icon_name(), "battery-good-charging-symbolic");
        assert_eq!(battery.warning_level(), LEVEL_NONE);

        // Held at a charge limit while plugged in: not charging, but not draining either.
        let held = AndroidBattery {
            status: 4,
            level: 85,
            ..charging()
        };
        assert_eq!(held.state(), STATE_PENDING_CHARGE);
        let unplugged = AndroidBattery {
            status: 4,
            plugged: 0,
            ..charging()
        };
        assert_eq!(unplugged.state(), STATE_DISCHARGING);
        let full = AndroidBattery {
            status: 5,
            level: 100,
            ..charging()
        };
        assert_eq!(full.state(), STATE_FULLY_CHARGED);
        assert_eq!(full.icon_name(), "battery-full-charged-symbolic");

        let draining = |level| AndroidBattery {
            status: 3,
            plugged: 0,
            level,
            ..charging()
        };
        assert_eq!(draining(19).warning_level(), LEVEL_LOW);
        assert_eq!(draining(19).icon_name(), "battery-caution-symbolic");
        assert_eq!(draining(5).warning_level(), LEVEL_CRITICAL);
        assert_eq!(draining(2).warning_level(), LEVEL_ACTION);
        assert_eq!(draining(80).icon_name(), "battery-full-symbolic");
        // Some phones report on a scale of their own.
        assert_eq!(
            AndroidBattery {
                level: 27,
                scale: 50,
                ..charging()
            }
            .percentage(),
            54.0
        );
        assert_eq!(
            AndroidBattery {
                scale: 0,
                ..charging()
            }
            .percentage(),
            0.0
        );
    }

    #[test]
    fn should_answer_what_solid_asks() {
        let mut upower = UPower::new("samsung", "SM-G990B");
        // Before Android has told anything: no devices, but the display device is there.
        let devices = answer(upower.call(&call(PATH, NAME, "EnumerateDevices", &[])));
        assert_eq!(devices.arguments().u32().unwrap(), 0);

        upower.update(charging(), 1_791_000_000);
        let devices = answer(upower.call(&call(PATH, NAME, "EnumerateDevices", &[])));
        let mut args = devices.arguments();
        args.u32().unwrap();
        assert_eq!(
            [args.string().unwrap(), args.string().unwrap()],
            [BATTERY, LINE_POWER]
        );
        let display = answer(upower.call(&call(PATH, NAME, "GetDisplayDevice", &[])));
        assert_eq!(display.arguments().string().unwrap(), DISPLAY_DEVICE);

        let all = answer(upower.call(&call(BATTERY, bus::PROPERTIES, "GetAll", &[DEVICE])));
        let all = all.arguments().dict().unwrap();
        assert_eq!(all.len(), 38);
        assert_eq!(property(&all, "Type"), Value::U32(KIND_BATTERY));
        assert_eq!(property(&all, "Percentage"), Value::F64(54.0));
        assert_eq!(property(&all, "State"), Value::U32(STATE_CHARGING));
        assert_eq!(property(&all, "IsPresent"), Value::Bool(true));
        assert_eq!(property(&all, "PowerSupply"), Value::Bool(true));
        assert_eq!(property(&all, "TimeToFull"), Value::I64(3600));
        assert_eq!(property(&all, "Model"), Value::Str("SM-G990B".into()));
        assert_eq!(property(&all, "UpdateTime"), Value::U64(1_791_000_000));
        let line = answer(upower.call(&call(LINE_POWER, bus::PROPERTIES, "GetAll", &[DEVICE])));
        let line = line.arguments().dict().unwrap();
        assert_eq!(property(&line, "Type"), Value::U32(KIND_LINE_POWER));
        assert_eq!(property(&line, "Online"), Value::Bool(true));
        let daemon = answer(upower.call(&call(PATH, bus::PROPERTIES, "Get", &[NAME, "OnBattery"])));
        assert_eq!(daemon.arguments().variant().unwrap(), Value::Bool(false));

        let missing = upower.call(&call(BATTERY, bus::PROPERTIES, "Get", &[DEVICE, "Colour"]));
        assert_eq!(missing.unwrap_err().0, errors::UNKNOWN_PROPERTY);
        let nowhere = upower.call(&call(
            "/org/freedesktop/UPower/devices/mouse",
            DEVICE,
            "Refresh",
            &[],
        ));
        assert_eq!(nowhere.unwrap_err().0, errors::UNKNOWN_OBJECT);
        let set = upower.call(&call(
            BATTERY,
            bus::PROPERTIES,
            "Set",
            &[DEVICE, "Percentage"],
        ));
        assert_eq!(set.unwrap_err().0, errors::PROPERTY_READ_ONLY);
        let xml = answer(upower.call(&call(DEVICES, bus::INTROSPECTABLE, "Introspect", &[])));
        assert!(xml
            .arguments()
            .string()
            .unwrap()
            .contains("<node name=\"battery_BAT0\"/>"));
    }

    #[test]
    fn should_tell_the_health_from_where_the_level_says_most() {
        let mut upower = UPower::new("samsung", "SM-G990B");
        let battery = |upower: &UPower, name: &str| {
            let properties = upower.properties(BATTERY);
            properties.into_iter().find(|it| it.0 == name).unwrap().1
        };
        // 2.43 Ah at 54 %: 4.5 Ah when full, of the 5 Ah it held new.
        upower.update(charging(), 100);
        assert_eq!(battery(&upower, "Capacity"), Value::F64(90.0));
        assert_eq!(battery(&upower, "EnergyFull"), Value::F64(18.0));
        assert_eq!(battery(&upower, "EnergyFullDesign"), Value::F64(20.0));
        // A whole percent is more of what's left at 20 %: the estimate from 54 % stays.
        let lower = AndroidBattery {
            level: 20,
            charge_uah: Some(940_000),
            ..charging()
        };
        upower.update(lower, 200);
        assert_eq!(battery(&upower, "Capacity"), Value::F64(90.0));
        // Higher up it counts: 2.76 Ah at 60 % is 4.6 Ah when full.
        let higher = AndroidBattery {
            level: 60,
            charge_uah: Some(2_760_000),
            ..charging()
        };
        upower.update(higher.clone(), 300);
        assert_eq!(battery(&upower, "Capacity"), Value::F64(92.0));
        // More than new is new, and without the capacity when new there is no telling.
        let small = AndroidBattery {
            design_uah: Some(4_000_000),
            ..higher.clone()
        };
        upower.update(small, 400);
        assert_eq!(battery(&upower, "Capacity"), Value::F64(100.0));
        let unknown = AndroidBattery {
            design_uah: None,
            ..higher
        };
        upower.update(unknown, 500);
        assert_eq!(battery(&upower, "Capacity"), Value::F64(0.0));
        assert_eq!(battery(&upower, "TimeToEmpty"), Value::I64(0));

        // Android's prediction while draining.
        let draining = AndroidBattery {
            status: 3,
            plugged: 0,
            time_to_empty_ms: Some(7_200_000),
            ..charging()
        };
        upower.update(draining, 600);
        assert_eq!(battery(&upower, "TimeToEmpty"), Value::I64(7200));
        assert_eq!(battery(&upower, "TimeToFull"), Value::I64(0));
    }

    #[test]
    fn should_tell_what_changed_and_only_that() {
        let mut upower = UPower::new("samsung", "SM-G990B");
        let first = upower.update(charging(), 100);
        let added: Vec<_> = first
            .iter()
            .filter(|it| it.member == "DeviceAdded")
            .collect();
        assert_eq!(added.len(), 2);
        assert_eq!(added[0].args, [BATTERY]);
        // The display device was there before, so it changes. The daemon was and stays not on
        // battery.
        let changed: Vec<&str> = first
            .iter()
            .filter(|it| it.member == "PropertiesChanged")
            .map(|it| it.path.as_str())
            .collect();
        assert_eq!(changed, [DISPLAY_DEVICE]);

        // The same again: nothing to tell, however much time has passed.
        assert!(upower.update(charging(), 200).is_empty());

        // One percent more: the battery and the display device change, the rest doesn't.
        let more = upower.update(
            AndroidBattery {
                level: 55,
                ..charging()
            },
            300,
        );
        let paths: Vec<&str> = more.iter().map(|it| it.path.as_str()).collect();
        assert_eq!(paths, [BATTERY, DISPLAY_DEVICE]);
        let message = dbus::parse(&dbus::signal(
            1,
            ":1.0",
            None,
            &more[0].path,
            &more[0].interface,
            &more[0].member,
            &more[0].signature,
            &more[0].body,
        ))
        .unwrap();
        let mut args = message.arguments();
        assert_eq!(args.string().unwrap(), DEVICE);
        let names: Vec<String> = args.dict().unwrap().into_iter().map(|(it, _)| it).collect();
        assert!(names.contains(&"Percentage".to_string()));
        assert!(names.contains(&"UpdateTime".to_string()));
        assert!(!names.contains(&"State".to_string()));
        assert_eq!(more[0].args, [DEVICE]);

        // Unplugged: the charger, the daemon's OnBattery and the battery's state change.
        let unplugged = AndroidBattery {
            status: 3,
            plugged: 0,
            level: 55,
            ..charging()
        };
        let paths: Vec<String> = upower
            .update(unplugged, 400)
            .into_iter()
            .map(|it| it.path)
            .collect();
        assert_eq!(paths, [PATH, BATTERY, LINE_POWER, DISPLAY_DEVICE]);
    }
}
