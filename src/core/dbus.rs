//! Just enough of D-Bus's wire format for the app to talk to the desktop session's bus and to
//! answer as a bus itself (`core::bus`): messages of every kind with plain arguments, variants
//! and dictionaries of them, and reading the header and leading plain arguments of what comes in.
//! A D-Bus library would bring its own threads and executor for these few messages.
//! https://dbus.freedesktop.org/doc/dbus-specification.html#message-protocol

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;

/// The caller won't read a reply, so the bus needn't send one.
pub const NO_REPLY_EXPECTED: u8 = 0x1;

/// The largest message the app reads. The specification allows 128 MiB; a notification's image
/// is far smaller than this.
pub const MAX_MESSAGE: usize = 16 << 20;

/// What doesn't follow the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

/// A message's header: its type and the header fields the app looks at.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub flags: u8,
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub signature: String,
}

/// A message read from the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub body: Vec<u8>,
    big_endian: bool,
}

impl Message {
    /// The body's arguments, to read in the order of the header's signature.
    pub fn arguments(&self) -> Reader<'_> {
        Reader {
            buf: &self.body,
            pos: 0,
            big_endian: self.big_endian,
        }
    }
}

/// A value the app sends in a variant or a dictionary.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    U32(u32),
    I32(i32),
    U64(u64),
    I64(i64),
    F64(f64),
    Str(String),
    Path(String),
}

impl Value {
    pub fn signature(&self) -> &'static str {
        match self {
            Self::Bool(_) => "b",
            Self::U32(_) => "u",
            Self::I32(_) => "i",
            Self::U64(_) => "t",
            Self::I64(_) => "x",
            Self::F64(_) => "d",
            Self::Str(_) => "s",
            Self::Path(_) => "o",
        }
    }
}

/// Writes a body (or a header), little-endian. Alignment is relative to the start, which is
/// right for a body: the body itself starts 8-aligned.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn align(&mut self, to: usize) {
        while self.buf.len() % to != 0 {
            self.buf.push(0);
        }
    }

    pub fn byte(&mut self, value: u8) -> &mut Self {
        self.buf.push(value);
        self
    }

    pub fn boolean(&mut self, value: bool) -> &mut Self {
        self.u32(value as u32)
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.align(4);
        self.buf.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.u32(value as u32)
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.align(8);
        self.buf.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn i64(&mut self, value: i64) -> &mut Self {
        self.u64(value as u64)
    }

    pub fn f64(&mut self, value: f64) -> &mut Self {
        self.u64(value.to_bits())
    }

    /// A string or an object path.
    pub fn string(&mut self, value: &str) -> &mut Self {
        self.u32(value.len() as u32);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
        self
    }

    pub fn signature(&mut self, value: &str) -> &mut Self {
        self.buf.push(value.len() as u8);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
        self
    }

    /// An array whose elements `write` adds, of a type aligned to `alignment` (8 for structs
    /// and dictionary entries). The padding before the first element is there even without one.
    pub fn array(&mut self, alignment: usize, write: impl FnOnce(&mut Self)) -> &mut Self {
        self.u32(0);
        let length_at = self.buf.len() - 4;
        self.align(alignment);
        let start = self.buf.len();
        write(self);
        let length = (self.buf.len() - start) as u32;
        self.buf[length_at..length_at + 4].copy_from_slice(&length.to_le_bytes());
        self
    }

    /// An array of strings (`as`) or of object paths (`ao`).
    pub fn strings(&mut self, values: &[&str]) -> &mut Self {
        self.array(4, |writer| {
            for value in values {
                writer.string(value);
            }
        })
    }

    pub fn value(&mut self, value: &Value) -> &mut Self {
        match value {
            Value::Bool(it) => self.boolean(*it),
            Value::U32(it) => self.u32(*it),
            Value::I32(it) => self.i32(*it),
            Value::U64(it) => self.u64(*it),
            Value::I64(it) => self.i64(*it),
            Value::F64(it) => self.f64(*it),
            Value::Str(it) | Value::Path(it) => self.string(it),
        }
    }

    pub fn variant(&mut self, value: &Value) -> &mut Self {
        self.signature(value.signature());
        self.value(value)
    }

    /// A dictionary of variants (`a{sv}`).
    pub fn dict<K: AsRef<str>>(&mut self, entries: &[(K, Value)]) -> &mut Self {
        self.array(8, |writer| {
            for (key, value) in entries {
                writer.align(8);
                writer.string(key.as_ref());
                writer.variant(value);
            }
        })
    }

    /// A header field: a `(yv)` struct with a string, object path or signature in the variant.
    fn field(&mut self, code: u8, kind: char, value: &str) {
        self.align(8);
        self.byte(code);
        self.signature(&kind.to_string());
        match kind {
            'g' => self.signature(value),
            _ => self.string(value),
        };
    }
}

/// What a message's header says, for writing one.
#[derive(Debug, Default)]
pub struct Fields<'a> {
    pub path: Option<&'a str>,
    pub interface: Option<&'a str>,
    pub member: Option<&'a str>,
    pub error_name: Option<&'a str>,
    pub reply_serial: Option<u32>,
    pub destination: Option<&'a str>,
    pub sender: Option<&'a str>,
    /// What `body` holds, empty for nothing.
    pub signature: &'a str,
}

/// A message of any kind, as the bytes to send.
pub fn message(kind: u8, flags: u8, serial: u32, fields: &Fields, body: &[u8]) -> Vec<u8> {
    let mut header = Writer::new();
    header
        .byte(b'l')
        .byte(kind)
        .byte(flags)
        .byte(1)
        .u32(body.len() as u32)
        .u32(serial)
        .u32(0);
    let start = header.buf.len();
    let strings = [
        (1, 'o', fields.path),
        (2, 's', fields.interface),
        (3, 's', fields.member),
        (4, 's', fields.error_name),
        (6, 's', fields.destination),
        (7, 's', fields.sender),
    ];
    for (code, kind, value) in strings {
        if let Some(value) = value {
            header.field(code, kind, value);
        }
    }
    if let Some(reply_serial) = fields.reply_serial {
        header.align(8);
        header.byte(5).signature("u").u32(reply_serial);
    }
    if !fields.signature.is_empty() {
        header.field(8, 'g', fields.signature);
    }
    let length = (header.buf.len() - start) as u32;
    header.buf[12..16].copy_from_slice(&length.to_le_bytes());
    header.align(8);
    header.buf.extend_from_slice(body);
    header.buf
}

/// A method call, as the bytes to send. `signature` describes `body` (empty for none).
pub fn method_call(
    serial: u32,
    destination: &str,
    path: &str,
    interface: &str,
    member: &str,
    signature: &str,
    body: &[u8],
    flags: u8,
) -> Vec<u8> {
    let fields = Fields {
        path: Some(path),
        interface: (!interface.is_empty()).then_some(interface),
        member: Some(member),
        destination: Some(destination),
        signature,
        ..Fields::default()
    };
    message(METHOD_CALL, flags, serial, &fields, body)
}

/// The reply to `call`, from `sender`.
pub fn method_return(
    serial: u32,
    call: &Header,
    sender: &str,
    signature: &str,
    body: &[u8],
) -> Vec<u8> {
    let fields = Fields {
        reply_serial: Some(call.serial),
        destination: call.sender.as_deref(),
        sender: Some(sender),
        signature,
        ..Fields::default()
    };
    message(METHOD_RETURN, NO_REPLY_EXPECTED, serial, &fields, body)
}

/// The error `name` with `text`, in reply to `call`, from `sender`.
pub fn error(serial: u32, call: &Header, sender: &str, name: &str, text: &str) -> Vec<u8> {
    let mut body = Writer::new();
    body.string(text);
    let fields = Fields {
        error_name: Some(name),
        reply_serial: Some(call.serial),
        destination: call.sender.as_deref(),
        sender: Some(sender),
        signature: "s",
        ..Fields::default()
    };
    message(
        ERROR,
        NO_REPLY_EXPECTED,
        serial,
        &fields,
        &body.into_bytes(),
    )
}

/// A signal from `sender`, to everybody who listens or to `destination` alone.
pub fn signal(
    serial: u32,
    sender: &str,
    destination: Option<&str>,
    path: &str,
    interface: &str,
    member: &str,
    signature: &str,
    body: &[u8],
) -> Vec<u8> {
    let fields = Fields {
        path: Some(path),
        interface: Some(interface),
        member: Some(member),
        destination,
        sender: Some(sender),
        signature,
        ..Fields::default()
    };
    message(SIGNAL, NO_REPLY_EXPECTED, serial, &fields, body)
}

fn read_u32_at(buf: &[u8], at: usize, big_endian: bool) -> u32 {
    let bytes = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
    if big_endian {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    }
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// How long the message at the start of `buf` is, once its first 16 bytes are there.
pub fn message_len(buf: &[u8]) -> Result<Option<usize>, Malformed> {
    if buf.len() < 16 {
        return Ok(None);
    }
    let big_endian = match buf[0] {
        b'l' => false,
        b'B' => true,
        _ => return Err(Malformed("unknown byte order")),
    };
    let body = read_u32_at(buf, 4, big_endian) as usize;
    let fields = read_u32_at(buf, 12, big_endian) as usize;
    let length = 16 + align8(fields) + body;
    if length > MAX_MESSAGE {
        return Err(Malformed("message too long"));
    }
    Ok(Some(length))
}

/// Read one whole message, as `message_len` measured it.
pub fn parse(buf: &[u8]) -> Result<Message, Malformed> {
    let Some(length) = message_len(buf)? else {
        return Err(Malformed("message cut short"));
    };
    if buf.len() < length {
        return Err(Malformed("message cut short"));
    }
    let big_endian = buf[0] == b'B';
    let mut header = Header {
        kind: buf[1],
        flags: buf[2],
        serial: read_u32_at(buf, 8, big_endian),
        ..Header::default()
    };
    let fields_end = 16 + read_u32_at(buf, 12, big_endian) as usize;
    let mut fields = Reader {
        buf: &buf[..fields_end],
        pos: 16,
        big_endian,
    };
    while fields.pos < fields_end {
        fields.align(8)?;
        if fields.pos >= fields_end {
            break;
        }
        let code = fields.byte()?;
        let kind = fields.signature()?;
        match (code, kind.as_str()) {
            (1, "o") => header.path = Some(fields.string()?),
            (2, "s") => header.interface = Some(fields.string()?),
            (3, "s") => header.member = Some(fields.string()?),
            (4, "s") => header.error_name = Some(fields.string()?),
            (5, "u") => header.reply_serial = Some(fields.u32()?),
            (6, "s") => header.destination = Some(fields.string()?),
            (7, "s") => header.sender = Some(fields.string()?),
            (8, "g") => header.signature = fields.signature()?,
            (_, "u") => {
                fields.u32()?;
            }
            (_, "s" | "o") => {
                fields.string()?;
            }
            (_, "g") => {
                fields.signature()?;
            }
            _ => return Err(Malformed("unexpected header field")),
        }
    }
    Ok(Message {
        header,
        body: buf[align8(fields_end)..length].to_vec(),
        big_endian,
    })
}

/// Reads a body's arguments in order.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    big_endian: bool,
}

impl Reader<'_> {
    fn align(&mut self, to: usize) -> Result<(), Malformed> {
        let aligned = (self.pos + to - 1) / to * to;
        if aligned > self.buf.len() {
            return Err(Malformed("argument cut short"));
        }
        self.pos = aligned;
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&[u8], Malformed> {
        if self.pos + n > self.buf.len() {
            return Err(Malformed("argument cut short"));
        }
        let taken = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(taken)
    }

    pub fn byte(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32, Malformed> {
        self.align(4)?;
        let bytes = self.take(4)?;
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        Ok(if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    }

    pub fn i32(&mut self) -> Result<i32, Malformed> {
        Ok(self.u32()? as i32)
    }

    pub fn boolean(&mut self) -> Result<bool, Malformed> {
        Ok(self.u32()? != 0)
    }

    pub fn u64(&mut self) -> Result<u64, Malformed> {
        self.align(8)?;
        let bytes: [u8; 8] = self.take(8)?.try_into().expect("8 bytes");
        Ok(if self.big_endian {
            u64::from_be_bytes(bytes)
        } else {
            u64::from_le_bytes(bytes)
        })
    }

    pub fn f64(&mut self) -> Result<f64, Malformed> {
        Ok(f64::from_bits(self.u64()?))
    }

    /// A variant of one of `Value`'s types.
    pub fn variant(&mut self) -> Result<Value, Malformed> {
        let signature = self.signature()?;
        Ok(match signature.as_str() {
            "b" => Value::Bool(self.boolean()?),
            "u" => Value::U32(self.u32()?),
            "i" => Value::I32(self.i32()?),
            "t" => Value::U64(self.u64()?),
            "x" => Value::I64(self.u64()? as i64),
            "d" => Value::F64(self.f64()?),
            "s" => Value::Str(self.string()?),
            "o" => Value::Path(self.string()?),
            _ => return Err(Malformed("variant of an unexpected type")),
        })
    }

    /// A dictionary of variants (`a{sv}`).
    pub fn dict(&mut self) -> Result<Vec<(String, Value)>, Malformed> {
        let length = self.u32()? as usize;
        self.align(8)?;
        let end = self.pos + length;
        let mut entries = Vec::new();
        while self.pos < end {
            self.align(8)?;
            let key = self.string()?;
            entries.push((key, self.variant()?));
        }
        Ok(entries)
    }

    /// A string or an object path. Invalid UTF-8 is replaced rather than refused.
    pub fn string(&mut self) -> Result<String, Malformed> {
        let length = self.u32()? as usize;
        let bytes = self.take(length + 1)?;
        Ok(String::from_utf8_lossy(&bytes[..length]).into_owned())
    }

    pub fn signature(&mut self) -> Result<String, Malformed> {
        let length = self.byte()? as usize;
        let bytes = self.take(length + 1)?;
        Ok(String::from_utf8_lossy(&bytes[..length]).into_owned())
    }
}

/// The first line of the client's side of authentication: EXTERNAL, as the user the socket's
/// credentials say, which spares the app from knowing the session user's id in the guest.
pub const AUTH: &[u8] = b"\0AUTH EXTERNAL\r\n";
/// The answer to the server's `DATA`: nothing to add.
pub const AUTH_DATA: &[u8] = b"DATA\r\n";
pub const AUTH_BEGIN: &[u8] = b"BEGIN\r\n";

/// The server's answer to an authentication line.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthReply {
    Ok,
    Data,
    Rejected(String),
}

pub fn auth_reply(line: &str) -> AuthReply {
    let line = line.trim_end();
    if line.starts_with("OK") {
        AuthReply::Ok
    } else if line.starts_with("DATA") {
        AuthReply::Data
    } else {
        AuthReply::Rejected(line.to_string())
    }
}

/// Strip the markup some notification servers take in a body (`<b>`, `<a href=…>`), and decode
/// the entities that come with it, for Android's plain text.
pub fn plain_text(markup: &str) -> String {
    let mut text = String::with_capacity(markup.len());
    let mut in_tag = false;
    for c in markup.chars() {
        match (c, in_tag) {
            ('<', _) => in_tag = true,
            ('>', true) => in_tag = false,
            (_, false) => text.push(c),
            _ => {}
        }
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Hello` as dbus-send and every library sends it, from the specification's example layout.
    #[test]
    fn should_write_a_method_call_like_any_client() {
        let bytes = method_call(
            1,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            "",
            &[],
            0,
        );
        // Fixed part: little-endian, method call, no flags, version 1, empty body, serial 1.
        assert_eq!(&bytes[..12], &[b'l', 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(bytes.len() % 8, 0);
        let message = parse(&bytes).unwrap();
        assert_eq!(message.header.kind, METHOD_CALL);
        assert_eq!(message.header.serial, 1);
        assert_eq!(message.header.path.as_deref(), Some("/org/freedesktop/DBus"));
        assert_eq!(message.header.interface.as_deref(), Some("org.freedesktop.DBus"));
        assert_eq!(message.header.member.as_deref(), Some("Hello"));
        assert_eq!(message.header.destination.as_deref(), Some("org.freedesktop.DBus"));
        assert_eq!(message.header.signature, "");
        assert!(message.body.is_empty());
    }

    #[test]
    fn should_read_back_the_arguments_it_wrote() {
        let mut body = Writer::new();
        body.strings(&["type='method_call'", "member='Notify'"]).u32(0);
        let bytes = method_call(
            7,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.Monitoring",
            "BecomeMonitor",
            "asu",
            &body.into_bytes(),
            NO_REPLY_EXPECTED,
        );
        assert_eq!(message_len(&bytes).unwrap(), Some(bytes.len()));
        let message = parse(&bytes).unwrap();
        assert_eq!(message.header.flags, NO_REPLY_EXPECTED);
        assert_eq!(message.header.signature, "asu");
        let mut arguments = message.arguments();
        // The array's byte length, then its strings.
        assert_eq!(arguments.u32().unwrap(), 4 + 18 + 1 + 1 + 4 + 15 + 1);
        assert_eq!(arguments.string().unwrap(), "type='method_call'");
        assert_eq!(arguments.string().unwrap(), "member='Notify'");
        assert_eq!(arguments.u32().unwrap(), 0);
    }

    /// A `Notify` call as notify-send makes it: `susssasa{sv}i`; the app reads its first five.
    #[test]
    fn should_read_the_start_of_a_notification() {
        let mut body = Writer::new();
        body.string("notify-send")
            .u32(0)
            .string("dialog-information")
            .string("Build finished")
            .string("<b>cargo</b> took 2 &amp; a half minutes")
            .strings(&[])
            .u32(0) // a{sv}: empty
            .i32(-1);
        let bytes = method_call(
            3,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            "susssasa{sv}i",
            &body.into_bytes(),
            0,
        );
        let message = parse(&bytes).unwrap();
        let mut arguments = message.arguments();
        assert_eq!(arguments.string().unwrap(), "notify-send");
        assert_eq!(arguments.u32().unwrap(), 0);
        assert_eq!(arguments.string().unwrap(), "dialog-information");
        assert_eq!(arguments.string().unwrap(), "Build finished");
        let text = arguments.string().unwrap();
        assert_eq!(plain_text(&text), "cargo took 2 & a half minutes");
    }

    #[test]
    fn should_read_big_endian_messages_too() {
        // A method return with REPLY_SERIAL 5, a SENDER and a `u` body, big-endian.
        let mut message = vec![b'B', METHOD_RETURN, 0, 1, 0, 0, 0, 4, 0, 0, 0, 9];
        let mut fields = Vec::new();
        fields.extend_from_slice(&[5, 1, b'u', 0, 0, 0, 0, 5]);
        fields.extend_from_slice(&[7, 1, b's', 0, 0, 0, 0, 5]);
        fields.extend_from_slice(b":1.42\0");
        message.extend_from_slice(&(fields.len() as u32).to_be_bytes());
        message.extend_from_slice(&fields);
        while message.len() % 8 != 0 {
            message.push(0);
        }
        message.extend_from_slice(&42u32.to_be_bytes());
        let parsed = parse(&message).unwrap();
        assert_eq!(parsed.header.kind, METHOD_RETURN);
        assert_eq!(parsed.header.serial, 9);
        assert_eq!(parsed.header.reply_serial, Some(5));
        assert_eq!(parsed.header.sender.as_deref(), Some(":1.42"));
        assert_eq!(parsed.arguments().u32().unwrap(), 42);
    }

    #[test]
    fn should_wait_for_whole_messages_and_refuse_garbage() {
        let bytes = method_call(1, "a.b", "/", "a.b", "C", "", &[], 0);
        assert_eq!(message_len(&bytes[..15]).unwrap(), None);
        assert_eq!(message_len(&bytes[..16]).unwrap(), Some(bytes.len()));
        assert!(parse(&bytes[..bytes.len() - 1]).is_err());
        assert!(message_len(b"xyzw000000000000").is_err());
        let mut huge = bytes.clone();
        huge[4..8].copy_from_slice(&(64u32 << 20).to_le_bytes());
        assert!(message_len(&huge).is_err());
        // An argument that runs past the body.
        let message = parse(&bytes).unwrap();
        assert!(message.arguments().string().is_err());
    }

    #[test]
    fn should_understand_the_authentication_replies() {
        assert_eq!(auth_reply("OK 1234deadbeef\r\n"), AuthReply::Ok);
        assert_eq!(auth_reply("DATA\r\n"), AuthReply::Data);
        assert_eq!(
            auth_reply("REJECTED EXTERNAL\r\n"),
            AuthReply::Rejected("REJECTED EXTERNAL".into())
        );
    }
}
