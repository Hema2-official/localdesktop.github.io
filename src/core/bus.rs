//! A message bus with nobody on it but the app, for what Linux programs ask the system bus: the
//! app's own services, so far UPower with Android's battery (`core::upower`). The guest has no
//! system bus (no systemd starts one), and a dbus-daemon for it would be one more process for
//! Android's phantom process killer, so the app answers on `/run/dbus/system_bus_socket` itself
//! (`android::guest::system_bus`).
//!
//! To its clients it is a system bus with the default policy: it authenticates them (EXTERNAL),
//! names them, answers `org.freedesktop.DBus`, hands the app's services their method calls and
//! sends the services' signals to whoever has a match rule for them. Clients may not own names or
//! call each other, so nothing goes from one client to another, and a call for any other service
//! gets the error a system bus without that service gives.
//! https://dbus.freedesktop.org/doc/dbus-specification.html#message-bus

use super::dbus::{self, Message, Value, Writer};
use std::collections::BTreeMap;

/// The bus's own name, and where it answers.
pub const NAME: &str = "org.freedesktop.DBus";
pub const PATH: &str = "/org/freedesktop/DBus";
/// The unique name of the app's services. Clients are named from `:1.1` on.
pub const SERVICES: &str = ":1.0";

pub const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
pub const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
pub const PEER: &str = "org.freedesktop.DBus.Peer";

/// The longest authentication line a client may send.
const MAX_LINE: usize = 16 << 10;
/// What the bus keeps for a client that doesn't read; past it the client is disconnected.
const MAX_OUTPUT: usize = 8 << 20;
const MAX_RULES: usize = 512;

/// The errors the bus and its services answer with.
pub mod errors {
    pub const FAILED: &str = "org.freedesktop.DBus.Error.Failed";
    pub const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
    pub const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
    pub const ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
    pub const NOT_SUPPORTED: &str = "org.freedesktop.DBus.Error.NotSupported";
    pub const LIMITS_EXCEEDED: &str = "org.freedesktop.DBus.Error.LimitsExceeded";
    pub const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
    pub const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
    pub const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
    pub const UNKNOWN_INTERFACE: &str = "org.freedesktop.DBus.Error.UnknownInterface";
    pub const UNKNOWN_PROPERTY: &str = "org.freedesktop.DBus.Error.UnknownProperty";
    pub const PROPERTY_READ_ONLY: &str = "org.freedesktop.DBus.Error.PropertyReadOnly";
    pub const MATCH_RULE_INVALID: &str = "org.freedesktop.DBus.Error.MatchRuleInvalid";
    pub const MATCH_RULE_NOT_FOUND: &str = "org.freedesktop.DBus.Error.MatchRuleNotFound";
}

/// What a method call gets back: a body and its signature, or an error's name and text.
pub type Reply = Result<(String, Vec<u8>), (&'static str, String)>;

/// A reply whose body `write` makes.
pub fn reply(signature: &str, write: impl FnOnce(&mut Writer)) -> Reply {
    let mut body = Writer::new();
    write(&mut body);
    Ok((signature.into(), body.into_bytes()))
}

pub fn fail(name: &'static str, text: impl Into<String>) -> Reply {
    Err((name, text.into()))
}

/// For `?` on a call's arguments.
pub fn invalid_args(_: dbus::Malformed) -> (&'static str, String) {
    (errors::INVALID_ARGS, "Invalid arguments".into())
}

/// One of the app's services on the bus.
pub trait Service {
    /// The well-known names it has.
    fn names(&self) -> &'static [&'static str];
    /// Answer a method call meant for it.
    fn call(&mut self, call: &Message) -> Reply;
}

/// A signal of a service's, for whoever has a match rule for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub path: String,
    pub interface: String,
    pub member: String,
    pub signature: String,
    pub body: Vec<u8>,
    /// Its leading string arguments, for rules that match on them (`arg0='…'`).
    pub args: Vec<String>,
}

/// Where a connection is: the byte that starts it, the authentication lines, then messages.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum Stage {
    #[default]
    Nul,
    Auth,
    /// `AUTH EXTERNAL` without an identity: the client sends it in a `DATA` line.
    Data,
    Authenticated,
    Messages,
}

#[derive(Debug, Default)]
struct Connection {
    stage: Stage,
    input: Vec<u8>,
    output: Vec<u8>,
    /// Its unique name, once it has said hello.
    name: Option<String>,
    rules: Vec<(String, Rule)>,
    uid: u32,
    pid: u32,
    /// It didn't read what it was sent, and goes.
    broken: bool,
}

pub struct Bus<S> {
    /// The server's id, which clients get after authenticating.
    guid: String,
    /// The app's process, which is the bus and its services.
    pid: u32,
    service: S,
    serial: u32,
    named: u32,
    connections: BTreeMap<u32, Connection>,
}

impl<S: Service> Bus<S> {
    /// `guid`: 32 hex digits, another one for every bus.
    pub fn new(guid: String, pid: u32, service: S) -> Self {
        Self {
            guid,
            pid,
            service,
            serial: 0,
            named: 0,
            connections: BTreeMap::new(),
        }
    }

    pub fn service(&mut self) -> &mut S {
        &mut self.service
    }

    /// A client connected, as `id`, from the process `pid`.
    pub fn accept(&mut self, id: u32, pid: u32) {
        let connection = Connection {
            pid,
            ..Connection::default()
        };
        self.connections.insert(id, connection);
    }

    /// What `id` sent. False if it broke the protocol, and has to be disconnected.
    pub fn received(&mut self, id: u32, bytes: &[u8]) -> bool {
        let Some(connection) = self.connections.get_mut(&id) else {
            return false;
        };
        connection.input.extend_from_slice(bytes);
        loop {
            let Some(connection) = self.connections.get_mut(&id) else {
                return false;
            };
            match connection.stage {
                Stage::Nul => {
                    let Some(&first) = connection.input.first() else {
                        return true;
                    };
                    if first != 0 {
                        return false;
                    }
                    connection.input.remove(0);
                    connection.stage = Stage::Auth;
                }
                Stage::Messages => return self.messages(id),
                _ => {
                    let Some(end) = connection.input.windows(2).position(|it| it == b"\r\n") else {
                        return connection.input.len() <= MAX_LINE;
                    };
                    let line: Vec<u8> = connection.input.drain(..end + 2).collect();
                    let line = String::from_utf8_lossy(&line[..end]);
                    match authenticate(connection, &line, &self.guid) {
                        Ok(Some(answer)) => connection.output.extend_from_slice(answer.as_bytes()),
                        Ok(None) => {}
                        Err(()) => return false,
                    }
                }
            }
        }
    }

    /// What there is to send to `id`: the caller removes what it wrote. `None` if `id` has to be
    /// disconnected.
    pub fn output(&mut self, id: u32) -> Option<&mut Vec<u8>> {
        self.connections
            .get_mut(&id)
            .filter(|it| !it.broken)
            .map(|it| &mut it.output)
    }

    /// Whether there is something to send to `id`, or it has to be disconnected.
    pub fn has_output(&self, id: u32) -> bool {
        self.connections
            .get(&id)
            .is_some_and(|it| it.broken || !it.output.is_empty())
    }

    /// `id` has gone.
    pub fn closed(&mut self, id: u32) {
        if let Some(name) = self.connections.remove(&id).and_then(|it| it.name) {
            self.name_owner_changed(&name, &name, "");
        }
    }

    /// Send a service's signal to whoever has a rule for it.
    pub fn emit(&mut self, signal: &Signal) {
        self.deliver(SERVICES, None, signal);
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    fn send(&mut self, id: u32, bytes: &[u8]) {
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        if connection.output.len() + bytes.len() > MAX_OUTPUT {
            connection.broken = true;
        } else if !connection.broken {
            connection.output.extend_from_slice(bytes);
        }
    }

    fn messages(&mut self, id: u32) -> bool {
        loop {
            let Some(connection) = self.connections.get_mut(&id) else {
                return false;
            };
            let length = match dbus::message_len(&connection.input) {
                Ok(Some(length)) if connection.input.len() >= length => length,
                Ok(_) => return true,
                Err(_) => return false,
            };
            let Ok(message) = dbus::parse(&connection.input[..length]) else {
                return false;
            };
            connection.input.drain(..length);
            if !self.dispatch(id, message) {
                return false;
            }
        }
    }

    fn dispatch(&mut self, id: u32, mut message: Message) -> bool {
        let Some(name) = self.connections.get(&id).and_then(|it| it.name.clone()) else {
            // A client says hello before anything else.
            return self.hello(id, &message);
        };
        // The bus says who sent what it passes on, and replies to that.
        message.header.sender = Some(name);
        if message.header.kind != dbus::METHOD_CALL {
            // Returns, errors and a client's own signals: nobody here takes them.
            return true;
        }
        let destination = message.header.destination.clone();
        let (sender, reply) = match destination.as_deref() {
            None | Some(NAME) => (NAME, self.bus_call(id, &message)),
            Some(it) if it == SERVICES || self.service.names().contains(&it) => {
                (SERVICES, self.service.call(&message))
            }
            Some(it) if self.owner(it).is_some() => (
                NAME,
                fail(
                    errors::ACCESS_DENIED,
                    format!("Clients of this bus may not call each other ({it})"),
                ),
            ),
            Some(it) => (
                NAME,
                fail(
                    errors::SERVICE_UNKNOWN,
                    format!("The name {it} was not provided by any .service files"),
                ),
            ),
        };
        if message.header.flags & dbus::NO_REPLY_EXPECTED == 0 {
            let serial = self.next_serial();
            let bytes = match reply {
                Ok((signature, body)) => {
                    dbus::method_return(serial, &message.header, sender, &signature, &body)
                }
                Err((name, text)) => dbus::error(serial, &message.header, sender, name, &text),
            };
            self.send(id, &bytes);
        }
        true
    }

    fn hello(&mut self, id: u32, message: &Message) -> bool {
        let header = &message.header;
        let hello = header.kind == dbus::METHOD_CALL
            && header.destination.as_deref() == Some(NAME)
            && matches!(header.interface.as_deref(), None | Some(NAME))
            && header.member.as_deref() == Some("Hello");
        if !hello {
            return false;
        }
        self.named += 1;
        let name = format!(":1.{}", self.named);
        if let Some(connection) = self.connections.get_mut(&id) {
            connection.name = Some(name.clone());
        }
        let mut call = header.clone();
        call.sender = Some(name.clone());
        let serial = self.next_serial();
        let mut body = Writer::new();
        body.string(&name);
        self.send(
            id,
            &dbus::method_return(serial, &call, NAME, "s", &body.into_bytes()),
        );
        let acquired = bus_signal("NameAcquired", "s", vec![name.clone()]);
        self.deliver(NAME, Some(&name), &acquired);
        self.name_owner_changed(&name, "", &name);
        true
    }

    fn name_owner_changed(&mut self, name: &str, old: &str, new: &str) {
        let signal = bus_signal(
            "NameOwnerChanged",
            "sss",
            vec![name.into(), old.into(), new.into()],
        );
        self.deliver(NAME, None, &signal);
    }

    /// Send `signal` from `sender` to `destination`, or without one to whoever has a rule for it.
    fn deliver(&mut self, sender: &str, destination: Option<&str>, signal: &Signal) {
        let serial = self.next_serial();
        let bytes = dbus::signal(
            serial,
            sender,
            destination,
            &signal.path,
            &signal.interface,
            &signal.member,
            &signal.signature,
            &signal.body,
        );
        let owner = |name: &str| self.owner(name);
        let recipients: Vec<u32> = self
            .connections
            .iter()
            .filter(|(_, connection)| {
                let Some(name) = &connection.name else {
                    return false;
                };
                match destination {
                    Some(destination) => name == destination,
                    None => connection
                        .rules
                        .iter()
                        .any(|(_, rule)| rule.matches(sender, signal, &owner)),
                }
            })
            .map(|(id, _)| *id)
            .collect();
        for id in recipients {
            self.send(id, &bytes);
        }
    }

    /// The unique name that has `name`.
    fn owner(&self, name: &str) -> Option<String> {
        if name == NAME {
            return Some(NAME.into());
        }
        if name == SERVICES || self.service.names().contains(&name) {
            return Some(SERVICES.into());
        }
        let client = self
            .connections
            .values()
            .any(|it| it.name.as_deref() == Some(name));
        client.then(|| name.into())
    }

    /// The user and process of the connection that has `name`.
    fn credentials(&self, name: &str) -> Option<(u32, u32)> {
        let owner = self.owner(name)?;
        if !owner.starts_with(':') || owner == SERVICES {
            return Some((0, self.pid));
        }
        self.connections
            .values()
            .find(|it| it.name.as_deref() == Some(owner.as_str()))
            .map(|it| (it.uid, it.pid))
    }

    fn no_owner(name: &str) -> Reply {
        fail(
            errors::NAME_HAS_NO_OWNER,
            format!("Could not get owner of name '{name}': no such name"),
        )
    }

    /// `org.freedesktop.DBus` and the standard interfaces at the bus.
    fn bus_call(&mut self, id: u32, call: &Message) -> Reply {
        let interface = call.header.interface.clone().unwrap_or_else(|| NAME.into());
        let member = call.header.member.clone().unwrap_or_default();
        let mut args = call.arguments();
        match (interface.as_str(), member.as_str()) {
            (NAME, "Hello") => fail(errors::FAILED, "Already handled an Hello message"),
            (NAME, "AddMatch") => {
                let text = args.string().map_err(invalid_args)?;
                let Some(rule) = Rule::parse(&text) else {
                    return fail(
                        errors::MATCH_RULE_INVALID,
                        format!("Invalid match rule: {text}"),
                    );
                };
                let connection = self.connections.get_mut(&id).expect("the caller is there");
                if connection.rules.len() >= MAX_RULES {
                    return fail(errors::LIMITS_EXCEEDED, "Too many match rules");
                }
                connection.rules.push((text, rule));
                reply("", |_| {})
            }
            (NAME, "RemoveMatch") => {
                let text = args.string().map_err(invalid_args)?;
                let connection = self.connections.get_mut(&id).expect("the caller is there");
                match connection.rules.iter().position(|(it, _)| *it == text) {
                    Some(index) => {
                        connection.rules.remove(index);
                        reply("", |_| {})
                    }
                    None => fail(
                        errors::MATCH_RULE_NOT_FOUND,
                        "The given match rule wasn't found",
                    ),
                }
            }
            (NAME, "RequestName") => {
                let name = args.string().map_err(invalid_args)?;
                fail(
                    errors::ACCESS_DENIED,
                    format!("Connection is not allowed to own the service \"{name}\" on this bus"),
                )
            }
            (NAME, "ReleaseName") => {
                let name = args.string().map_err(invalid_args)?;
                // Nobody gets names here: the name has no owner, or another one.
                let answer = if self.owner(&name).is_some() { 3 } else { 2 };
                reply("u", |body| {
                    body.u32(answer);
                })
            }
            (NAME, "ListNames") => {
                let mut names = vec![NAME, SERVICES];
                names.extend(self.service.names());
                let clients: Vec<String> = self
                    .connections
                    .values()
                    .filter_map(|it| it.name.clone())
                    .collect();
                names.extend(clients.iter().map(String::as_str));
                reply("as", |body| {
                    body.strings(&names);
                })
            }
            (NAME, "ListActivatableNames") => reply("as", |body| {
                body.strings(&[NAME]);
            }),
            (NAME, "NameHasOwner") => {
                let name = args.string().map_err(invalid_args)?;
                let owned = self.owner(&name).is_some();
                reply("b", |body| {
                    body.boolean(owned);
                })
            }
            (NAME, "GetNameOwner") => {
                let name = args.string().map_err(invalid_args)?;
                match self.owner(&name) {
                    Some(owner) => reply("s", |body| {
                        body.string(&owner);
                    }),
                    None => Self::no_owner(&name),
                }
            }
            (NAME, "ListQueuedOwners") => {
                let name = args.string().map_err(invalid_args)?;
                match self.owner(&name) {
                    Some(owner) => reply("as", |body| {
                        body.strings(&[&owner]);
                    }),
                    None => Self::no_owner(&name),
                }
            }
            (NAME, "StartServiceByName") => {
                let name = args.string().map_err(invalid_args)?;
                if self.owner(&name).is_some() {
                    // DBUS_START_REPLY_ALREADY_RUNNING
                    reply("u", |body| {
                        body.u32(2);
                    })
                } else {
                    fail(
                        errors::SERVICE_UNKNOWN,
                        format!("The name {name} was not provided by any .service files"),
                    )
                }
            }
            (NAME, "GetConnectionUnixUser" | "GetConnectionUnixProcessID") => {
                let name = args.string().map_err(invalid_args)?;
                let Some((uid, pid)) = self.credentials(&name) else {
                    return Self::no_owner(&name);
                };
                let value = if member == "GetConnectionUnixUser" {
                    uid
                } else {
                    pid
                };
                reply("u", |body| {
                    body.u32(value);
                })
            }
            (NAME, "GetConnectionCredentials") => {
                let name = args.string().map_err(invalid_args)?;
                let Some((uid, pid)) = self.credentials(&name) else {
                    return Self::no_owner(&name);
                };
                reply("a{sv}", |body| {
                    body.dict(&[
                        ("UnixUserID", Value::U32(uid)),
                        ("ProcessID", Value::U32(pid)),
                    ]);
                })
            }
            (NAME, "GetId") | (PEER, "GetMachineId") => {
                let guid = self.guid.clone();
                reply("s", |body| {
                    body.string(&guid);
                })
            }
            (NAME, "ReloadConfig") | (PEER, "Ping") => reply("", |_| {}),
            (NAME, "UpdateActivationEnvironment")
            | ("org.freedesktop.DBus.Monitoring", "BecomeMonitor") => {
                fail(errors::ACCESS_DENIED, "Not allowed on this bus")
            }
            (INTROSPECTABLE, "Introspect") => reply("s", |body| {
                body.string(BUS_INTROSPECTION);
            }),
            (interface, member) => fail(
                errors::UNKNOWN_METHOD,
                format!("Unknown method '{member}' or interface '{interface}'."),
            ),
        }
    }
}

fn bus_signal(member: &str, signature: &str, args: Vec<String>) -> Signal {
    let mut body = Writer::new();
    for arg in &args {
        body.string(arg);
    }
    Signal {
        path: PATH.into(),
        interface: NAME.into(),
        member: member.into(),
        signature: signature.into(),
        body: body.into_bytes(),
        args,
    }
}

/// One line of a client's authentication, and the answer to it; `Err` breaks the connection.
/// The client says who it is with EXTERNAL; the bus believes it, since every program in the guest
/// is the app's own process anyway.
fn authenticate(connection: &mut Connection, line: &str, guid: &str) -> Result<Option<String>, ()> {
    let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
    let accept = |connection: &mut Connection, identity: &str| {
        connection.uid = uid(identity).unwrap_or(0);
        connection.stage = Stage::Authenticated;
        Ok(Some(format!("OK {guid}\r\n")))
    };
    match (connection.stage, command) {
        (Stage::Auth, "AUTH") => match argument.split_once(' ') {
            Some(("EXTERNAL", identity)) => accept(connection, identity),
            None if argument == "EXTERNAL" => {
                connection.stage = Stage::Data;
                Ok(Some("DATA\r\n".into()))
            }
            _ => Ok(Some("REJECTED EXTERNAL\r\n".into())),
        },
        (Stage::Data, "DATA") => accept(connection, argument),
        (Stage::Authenticated, "NEGOTIATE_UNIX_FD") => {
            Ok(Some("ERROR no file descriptors on this bus\r\n".into()))
        }
        (Stage::Authenticated, "BEGIN") => {
            connection.stage = Stage::Messages;
            Ok(None)
        }
        (_, "CANCEL" | "ERROR") => {
            connection.stage = Stage::Auth;
            Ok(Some("REJECTED EXTERNAL\r\n".into()))
        }
        (_, "BEGIN") => Err(()),
        _ => Ok(Some("ERROR\r\n".into())),
    }
}

/// The user id in an EXTERNAL identity: its decimal digits, hex-encoded.
fn uid(identity: &str) -> Option<u32> {
    let digits = (0..identity.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(identity.get(at..at + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(digits).ok()?.parse().ok()
}

/// What a client listens for (`AddMatch`).
#[derive(Debug, Default, Clone, PartialEq)]
struct Rule {
    kind: Option<String>,
    sender: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    path: Option<String>,
    path_namespace: Option<String>,
    destination: Option<String>,
    args: Vec<(usize, String)>,
    arg_paths: Vec<(usize, String)>,
    arg0_namespace: Option<String>,
}

impl Rule {
    fn parse(text: &str) -> Option<Self> {
        let mut rule = Self::default();
        for (key, value) in pairs(text)? {
            match key.as_str() {
                "type" => rule.kind = Some(value),
                "sender" => rule.sender = Some(value),
                "interface" => rule.interface = Some(value),
                "member" => rule.member = Some(value),
                "path" => rule.path = Some(value),
                "path_namespace" => rule.path_namespace = Some(value),
                "destination" => rule.destination = Some(value),
                "arg0namespace" => rule.arg0_namespace = Some(value),
                // Monitors see everything; here nobody sees what isn't sent to them.
                "eavesdrop" => {}
                key => {
                    let index = key.strip_prefix("arg")?;
                    let (index, path) = match index.strip_suffix("path") {
                        Some(index) => (index, true),
                        None => (index, false),
                    };
                    let index: usize = index.parse().ok().filter(|it| *it < 64)?;
                    if path {
                        rule.arg_paths.push((index, value));
                    } else {
                        rule.args.push((index, value));
                    }
                }
            }
        }
        Some(rule)
    }

    /// Whether a broadcast `signal` from the unique name `sender` is one the rule asks for.
    fn matches(
        &self,
        sender: &str,
        signal: &Signal,
        owner: &dyn Fn(&str) -> Option<String>,
    ) -> bool {
        let same = |want: &Option<String>, have: &str| want.as_deref().is_none_or(|it| it == have);
        if self.kind.as_deref().is_some_and(|it| it != "signal")
            || self.destination.is_some()
            || !same(&self.interface, &signal.interface)
            || !same(&self.member, &signal.member)
            || !same(&self.path, &signal.path)
        {
            return false;
        }
        if let Some(want) = &self.sender {
            if owner(want).as_deref() != Some(sender) {
                return false;
            }
        }
        if let Some(namespace) = &self.path_namespace {
            let inside = namespace == "/"
                || signal.path == *namespace
                || signal.path.starts_with(&format!("{namespace}/"));
            if !inside {
                return false;
            }
        }
        if self
            .args
            .iter()
            .any(|(index, want)| signal.args.get(*index) != Some(want))
        {
            return false;
        }
        let related = |have: &String, want: &String| {
            have == want
                || (want.ends_with('/') && have.starts_with(want.as_str()))
                || (have.ends_with('/') && want.starts_with(have.as_str()))
        };
        if self.arg_paths.iter().any(|(index, want)| {
            !signal
                .args
                .get(*index)
                .is_some_and(|have| related(have, want))
        }) {
            return false;
        }
        if let Some(namespace) = &self.arg0_namespace {
            let inside = signal
                .args
                .first()
                .is_some_and(|it| it == namespace || it.starts_with(&format!("{namespace}.")));
            if !inside {
                return false;
            }
        }
        true
    }
}

/// A rule's `key='value'` pairs. Inside quotes everything is as it is; outside them `\'` is an
/// apostrophe and a comma ends the value.
fn pairs(text: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let mut value = String::new();
        let mut quoted = false;
        let mut chars = after.char_indices().peekable();
        let mut end = after.len();
        while let Some((at, c)) = chars.next() {
            match (c, quoted) {
                ('\'', _) => quoted = !quoted,
                ('\\', false) if chars.peek().is_some_and(|(_, next)| *next == '\'') => {
                    chars.next();
                    value.push('\'');
                }
                (',', false) => {
                    end = at + 1;
                    break;
                }
                (c, _) => value.push(c),
            }
        }
        if quoted {
            return None;
        }
        pairs.push((key.trim().to_string(), value));
        rest = after[end..].trim_start();
    }
    Some(pairs)
}

const BUS_INTROSPECTION: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.DBus">
    <method name="Hello"><arg direction="out" type="s"/></method>
    <method name="RequestName"><arg direction="in" type="s"/><arg direction="in" type="u"/><arg direction="out" type="u"/></method>
    <method name="ReleaseName"><arg direction="in" type="s"/><arg direction="out" type="u"/></method>
    <method name="StartServiceByName"><arg direction="in" type="s"/><arg direction="in" type="u"/><arg direction="out" type="u"/></method>
    <method name="NameHasOwner"><arg direction="in" type="s"/><arg direction="out" type="b"/></method>
    <method name="ListNames"><arg direction="out" type="as"/></method>
    <method name="ListActivatableNames"><arg direction="out" type="as"/></method>
    <method name="AddMatch"><arg direction="in" type="s"/></method>
    <method name="RemoveMatch"><arg direction="in" type="s"/></method>
    <method name="GetNameOwner"><arg direction="in" type="s"/><arg direction="out" type="s"/></method>
    <method name="ListQueuedOwners"><arg direction="in" type="s"/><arg direction="out" type="as"/></method>
    <method name="GetConnectionUnixUser"><arg direction="in" type="s"/><arg direction="out" type="u"/></method>
    <method name="GetConnectionUnixProcessID"><arg direction="in" type="s"/><arg direction="out" type="u"/></method>
    <method name="GetConnectionCredentials"><arg direction="in" type="s"/><arg direction="out" type="a{sv}"/></method>
    <method name="GetId"><arg direction="out" type="s"/></method>
    <signal name="NameOwnerChanged"><arg type="s"/><arg type="s"/><arg type="s"/></signal>
    <signal name="NameLost"><arg type="s"/></signal>
    <signal name="NameAcquired"><arg type="s"/></signal>
  </interface>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect"><arg direction="out" type="s"/></method>
  </interface>
  <interface name="org.freedesktop.DBus.Peer">
    <method name="GetMachineId"><arg direction="out" type="s"/></method>
    <method name="Ping"/>
  </interface>
</node>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const GUID: &str = "0123456789abcdef0123456789abcdef";

    /// A service that answers every call with its member's name.
    struct Echo;

    impl Service for Echo {
        fn names(&self) -> &'static [&'static str] {
            &["org.example.Echo"]
        }

        fn call(&mut self, call: &Message) -> Reply {
            let member = call.header.member.clone().unwrap_or_default();
            reply("s", |body| {
                body.string(&member);
            })
        }
    }

    fn bus() -> Bus<Echo> {
        Bus::new(GUID.into(), 4242, Echo)
    }

    /// What the bus has for `id`, taken.
    fn output(bus: &mut Bus<Echo>, id: u32) -> Vec<u8> {
        std::mem::take(bus.output(id).expect("still connected"))
    }

    fn messages(mut bytes: &[u8]) -> Vec<Message> {
        let mut messages = Vec::new();
        while let Some(length) = dbus::message_len(bytes).unwrap() {
            messages.push(dbus::parse(&bytes[..length]).unwrap());
            bytes = &bytes[length..];
        }
        messages
    }

    fn call(
        serial: u32,
        destination: &str,
        interface: &str,
        member: &str,
        args: &[&str],
    ) -> Vec<u8> {
        let mut body = Writer::new();
        for arg in args {
            body.string(arg);
        }
        let signature = "s".repeat(args.len());
        dbus::method_call(
            serial,
            destination,
            "/",
            interface,
            member,
            &signature,
            &body.into_bytes(),
            0,
        )
    }

    /// A client as libdbus and GDBus connect: EXTERNAL with the uid, no file descriptors, hello.
    fn connect(bus: &mut Bus<Echo>, id: u32) -> String {
        bus.accept(id, 100 + id);
        assert!(bus.received(id, b"\0AUTH EXTERNAL 31303031\r\n"));
        assert_eq!(output(bus, id), format!("OK {GUID}\r\n").as_bytes());
        let mut bytes = b"NEGOTIATE_UNIX_FD\r\nBEGIN\r\n".to_vec();
        bytes.extend(call(1, NAME, NAME, "Hello", &[]));
        assert!(bus.received(id, &bytes));
        let out = output(bus, id);
        assert!(out.starts_with(b"ERROR"));
        let lines_end = out.windows(2).position(|it| it == b"\r\n").unwrap() + 2;
        let replies = messages(&out[lines_end..]);
        let name = replies[0].arguments().string().unwrap();
        assert_eq!(replies[0].header.reply_serial, Some(1));
        // NameAcquired, to it alone.
        assert_eq!(replies[1].header.member.as_deref(), Some("NameAcquired"));
        assert_eq!(
            replies[1].header.destination.as_deref(),
            Some(name.as_str())
        );
        name
    }

    fn ask(bus: &mut Bus<Echo>, id: u32, bytes: Vec<u8>) -> Message {
        assert!(bus.received(id, &bytes));
        let mut replies = messages(&output(bus, id));
        assert_eq!(replies.len(), 1, "one reply");
        replies.remove(0)
    }

    fn error_name(message: &Message) -> Option<&str> {
        (message.header.kind == dbus::ERROR).then_some(message.header.error_name.as_deref()?)
    }

    fn strings(message: &Message, count: usize) -> Vec<String> {
        let mut args = message.arguments();
        (0..count).map(|_| args.string().unwrap()).collect()
    }

    #[test]
    fn should_let_clients_in_as_dbus_daemon_does() {
        let mut bus = bus();
        assert_eq!(connect(&mut bus, 1), ":1.1");
        assert_eq!(connect(&mut bus, 2), ":1.2");
        // The user it said it is, the process its socket says.
        let reply = ask(
            &mut bus,
            1,
            call(2, NAME, NAME, "GetConnectionUnixUser", &[":1.2"]),
        );
        assert_eq!(reply.arguments().u32().unwrap(), 1001);
        let reply = ask(
            &mut bus,
            1,
            call(3, NAME, NAME, "GetConnectionUnixProcessID", &[":1.2"]),
        );
        assert_eq!(reply.arguments().u32().unwrap(), 102);

        // sd-bus sends its identity in a DATA line.
        bus.accept(3, 103);
        assert!(bus.received(3, b"\0AUTH EXTERNAL\r\nDATA\r\n"));
        assert_eq!(
            output(&mut bus, 3),
            format!("DATA\r\nOK {GUID}\r\n").as_bytes()
        );

        // Other mechanisms are refused, and so is anything but hello first, or no NUL byte.
        bus.accept(4, 104);
        assert!(bus.received(4, b"\0AUTH ANONYMOUS\r\n"));
        assert_eq!(output(&mut bus, 4), b"REJECTED EXTERNAL\r\n");
        assert!(bus.received(4, b"AUTH EXTERNAL 30\r\nBEGIN\r\n"));
        assert!(!bus.received(4, &call(1, NAME, NAME, "ListNames", &[])));
        bus.accept(5, 105);
        assert!(!bus.received(5, b"AUTH EXTERNAL 30\r\n"));
        bus.accept(6, 106);
        assert!(!bus.received(6, b"\0BEGIN\r\n"));
    }

    #[test]
    fn should_answer_for_the_names_there_are_and_only_them() {
        let mut bus = bus();
        connect(&mut bus, 1);
        let mut owner = |name: &str| ask(&mut bus, 1, call(9, NAME, NAME, "GetNameOwner", &[name]));
        assert_eq!(strings(&owner("org.example.Echo"), 1), [SERVICES]);
        assert_eq!(strings(&owner(NAME), 1), [NAME]);
        assert_eq!(
            error_name(&owner("org.freedesktop.login1")),
            Some(errors::NAME_HAS_NO_OWNER)
        );

        let names = ask(&mut bus, 1, call(10, NAME, NAME, "ListNames", &[]));
        let mut args = names.arguments();
        args.u32().unwrap();
        let listed: Vec<String> = (0..4).map(|_| args.string().unwrap()).collect();
        assert_eq!(listed, [NAME, SERVICES, "org.example.Echo", ":1.1"]);

        let echo = call(11, NAME, NAME, "StartServiceByName", &["org.example.Echo"]);
        assert_eq!(ask(&mut bus, 1, echo).arguments().u32().unwrap(), 2);
        let login1 = call(
            12,
            NAME,
            NAME,
            "StartServiceByName",
            &["org.freedesktop.login1"],
        );
        assert_eq!(
            error_name(&ask(&mut bus, 1, login1)),
            Some(errors::SERVICE_UNKNOWN)
        );
        // Nobody may own a name, as the system bus's default policy has it.
        let mut body = Writer::new();
        body.string("org.example.Mine").u32(0);
        let request = dbus::method_call(
            13,
            NAME,
            PATH,
            NAME,
            "RequestName",
            "su",
            &body.into_bytes(),
            0,
        );
        assert_eq!(
            error_name(&ask(&mut bus, 1, request)),
            Some(errors::ACCESS_DENIED)
        );
        let unknown = ask(&mut bus, 1, call(14, NAME, NAME, "Frobnicate", &[]));
        assert_eq!(error_name(&unknown), Some(errors::UNKNOWN_METHOD));
    }

    #[test]
    fn should_hand_the_services_their_calls_and_refuse_the_rest() {
        let mut bus = bus();
        connect(&mut bus, 1);
        connect(&mut bus, 2);
        let echoed = ask(
            &mut bus,
            1,
            call(5, "org.example.Echo", "org.example.Echo", "Hi", &[]),
        );
        assert_eq!(echoed.header.kind, dbus::METHOD_RETURN);
        assert_eq!(echoed.header.sender.as_deref(), Some(SERVICES));
        assert_eq!(echoed.header.destination.as_deref(), Some(":1.1"));
        assert_eq!(echoed.header.reply_serial, Some(5));
        assert_eq!(strings(&echoed, 1), ["Hi"]);
        // The services' unique name works too.
        let echoed = ask(
            &mut bus,
            1,
            call(6, SERVICES, "org.example.Echo", "Again", &[]),
        );
        assert_eq!(strings(&echoed, 1), ["Again"]);

        let suspend = call(
            7,
            "org.freedesktop.login1",
            "org.freedesktop.login1.Manager",
            "CanSuspend",
            &[],
        );
        let login1 = ask(&mut bus, 1, suspend);
        assert_eq!(error_name(&login1), Some(errors::SERVICE_UNKNOWN));
        assert_eq!(login1.header.sender.as_deref(), Some(NAME));
        // Clients don't reach each other.
        let other = ask(
            &mut bus,
            1,
            call(8, ":1.2", "org.example.Mine", "Hello", &[]),
        );
        assert_eq!(error_name(&other), Some(errors::ACCESS_DENIED));
        assert!(output(&mut bus, 2).is_empty());

        // No reply when none is wanted.
        let mut quiet = call(9, "org.example.Echo", "org.example.Echo", "Hush", &[]);
        quiet[2] = dbus::NO_REPLY_EXPECTED;
        assert!(bus.received(1, &quiet));
        assert!(output(&mut bus, 1).is_empty());
    }

    fn changed(path: &str, interface: &str) -> Signal {
        let mut body = Writer::new();
        body.string(interface);
        Signal {
            path: path.into(),
            interface: PROPERTIES.into(),
            member: "PropertiesChanged".into(),
            signature: "s".into(),
            body: body.into_bytes(),
            args: vec![interface.into()],
        }
    }

    #[test]
    fn should_send_signals_to_whoever_has_a_rule_for_them() {
        let mut bus = bus();
        connect(&mut bus, 1);
        connect(&mut bus, 2);
        // As Solid asks for a battery's changes.
        let rule = "type='signal',sender='org.example.Echo',path='/devices/battery',\
                    interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',\
                    arg0='org.freedesktop.UPower.Device'";
        let added = ask(&mut bus, 1, call(4, NAME, NAME, "AddMatch", &[rule]));
        assert_eq!(added.header.kind, dbus::METHOD_RETURN);
        ask(
            &mut bus,
            2,
            call(
                4,
                NAME,
                NAME,
                "AddMatch",
                &["type='signal',path_namespace='/devices'"],
            ),
        );

        bus.emit(&changed(
            "/devices/battery",
            "org.freedesktop.UPower.Device",
        ));
        let got = messages(&output(&mut bus, 1));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].header.kind, dbus::SIGNAL);
        assert_eq!(got[0].header.sender.as_deref(), Some(SERVICES));
        assert_eq!(got[0].header.path.as_deref(), Some("/devices/battery"));
        assert_eq!(messages(&output(&mut bus, 2)).len(), 1);

        // Another object or interface: not what client 1 asked for.
        bus.emit(&changed(
            "/devices/line_power",
            "org.freedesktop.UPower.Device",
        ));
        bus.emit(&changed("/devices/battery", "org.freedesktop.UPower"));
        assert!(output(&mut bus, 1).is_empty());
        assert_eq!(messages(&output(&mut bus, 2)).len(), 2);
        bus.emit(&changed("/elsewhere", "org.freedesktop.UPower.Device"));
        assert!(output(&mut bus, 2).is_empty());

        // Removed, it gets nothing more.
        ask(&mut bus, 1, call(5, NAME, NAME, "RemoveMatch", &[rule]));
        bus.emit(&changed(
            "/devices/battery",
            "org.freedesktop.UPower.Device",
        ));
        assert!(output(&mut bus, 1).is_empty());
        let again = ask(&mut bus, 1, call(6, NAME, NAME, "RemoveMatch", &[rule]));
        assert_eq!(error_name(&again), Some(errors::MATCH_RULE_NOT_FOUND));
        let broken = ask(
            &mut bus,
            1,
            call(7, NAME, NAME, "AddMatch", &["member='Unclosed"]),
        );
        assert_eq!(error_name(&broken), Some(errors::MATCH_RULE_INVALID));
    }

    #[test]
    fn should_tell_when_clients_come_and_go() {
        let mut bus = bus();
        connect(&mut bus, 1);
        let rule = "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',\
                    member='NameOwnerChanged'";
        ask(&mut bus, 1, call(2, NAME, NAME, "AddMatch", &[rule]));
        connect(&mut bus, 2);
        let came = messages(&output(&mut bus, 1));
        assert_eq!(strings(&came[0], 3), [":1.2", "", ":1.2"]);
        bus.closed(2);
        let went = messages(&output(&mut bus, 1));
        assert_eq!(strings(&went[0], 3), [":1.2", ":1.2", ""]);
        // Its name went with it.
        let gone = ask(&mut bus, 1, call(3, NAME, NAME, "NameHasOwner", &[":1.2"]));
        assert!(!gone.arguments().boolean().unwrap());
    }

    #[test]
    fn should_read_match_rules_as_the_specification_writes_them() {
        let rule =
            Rule::parse("type='signal', member='It'\\''s',arg2='a,b',arg0path='/a/'").unwrap();
        assert_eq!(rule.kind.as_deref(), Some("signal"));
        assert_eq!(rule.member.as_deref(), Some("It's"));
        assert_eq!(rule.args, [(2, "a,b".to_string())]);
        assert_eq!(rule.arg_paths, [(0, "/a/".to_string())]);
        assert!(Rule::parse("").is_some());
        assert!(Rule::parse("member='open").is_none());
        assert!(Rule::parse("arg64='x'").is_none());
        assert!(Rule::parse("nonsense").is_none());

        let signal = |args: &[&str]| Signal {
            path: "/p".into(),
            interface: "i.f".into(),
            member: "M".into(),
            signature: String::new(),
            body: Vec::new(),
            args: args.iter().map(|it| it.to_string()).collect(),
        };
        let nobody = |_: &str| None;
        let matches = |text: &str, args: &[&str]| {
            Rule::parse(text)
                .unwrap()
                .matches(SERVICES, &signal(args), &nobody)
        };
        assert!(matches("arg0path='/a/'", &["/a/b"]));
        assert!(!matches("arg0path='/a/'", &["/b"]));
        assert!(matches("arg0namespace='org.kde'", &["org.kde.x"]));
        assert!(!matches("arg0namespace='org.kde'", &["org.kdex"]));
        assert!(!matches("type='method_call'", &[]));
        // A sender nobody has.
        assert!(!matches("sender='org.example.Gone'", &[]));
    }
}
