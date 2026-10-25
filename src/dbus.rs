//! Minimal D-Bus client: SASL EXTERNAL auth, unix-fd negotiation, and the
//! little-endian wire format (marshalling/unmarshalling of all basic and
//! container types). Spoken directly over the session bus Unix socket.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::sys;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Path(String),
    Sig(String),
    /// Index into the message's unix-fd array.
    Fd(u32),
    /// Element signature + elements (the signature is needed for empty arrays).
    Array(String, Vec<Value>),
    Struct(Vec<Value>),
    DictEntry(Box<Value>, Box<Value>),
    Variant(Box<Value>),
}

impl Value {
    pub fn signature(&self) -> String {
        match self {
            Value::Byte(_) => "y".into(),
            Value::Bool(_) => "b".into(),
            Value::I16(_) => "n".into(),
            Value::U16(_) => "q".into(),
            Value::I32(_) => "i".into(),
            Value::U32(_) => "u".into(),
            Value::I64(_) => "x".into(),
            Value::U64(_) => "t".into(),
            Value::F64(_) => "d".into(),
            Value::Str(_) => "s".into(),
            Value::Path(_) => "o".into(),
            Value::Sig(_) => "g".into(),
            Value::Fd(_) => "h".into(),
            Value::Array(sig, _) => format!("a{sig}"),
            Value::Struct(v) => format!("({})", v.iter().map(|x| x.signature()).collect::<String>()),
            Value::DictEntry(k, v) => format!("{{{}{}}}", k.signature(), v.signature()),
            Value::Variant(_) => "v".into(),
        }
    }

    pub fn variant(v: Value) -> Value {
        Value::Variant(Box::new(v))
    }

    pub fn str(s: &str) -> Value {
        Value::Str(s.to_string())
    }

    /// Builds an `a{sv}` dictionary.
    pub fn dict_sv(entries: Vec<(&str, Value)>) -> Value {
        Value::Array(
            "{sv}".into(),
            entries
                .into_iter()
                .map(|(k, v)| Value::DictEntry(Box::new(Value::str(k)), Box::new(Value::variant(v))))
                .collect(),
        )
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Path(s) | Value::Sig(s) => Some(s),
            Value::Variant(v) => v.as_str(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Byte(x) => Some(x as i64),
            Value::Bool(x) => Some(x as i64),
            Value::I16(x) => Some(x as i64),
            Value::U16(x) => Some(x as i64),
            Value::I32(x) => Some(x as i64),
            Value::U32(x) => Some(x as i64),
            Value::I64(x) => Some(x),
            Value::U64(x) => Some(x as i64),
            Value::F64(x) => Some(x as i64),
            Value::Variant(ref v) => v.as_i64(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(_, v) => Some(v),
            Value::Variant(v) => v.as_array(),
            _ => None,
        }
    }

    /// Looks up `key` in an `a{sv}` / `a{s*}` dictionary.
    pub fn dict_get(&self, key: &str) -> Option<&Value> {
        self.as_array()?.iter().find_map(|e| match e {
            Value::DictEntry(k, v) if k.as_str() == Some(key) => Some(match &**v {
                Value::Variant(inner) => &**inner,
                other => other,
            }),
            _ => None,
        })
    }
}

fn alignment(c: u8) -> usize {
    match c {
        b'y' | b'g' | b'v' => 1,
        b'n' | b'q' => 2,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 4,
    }
}

/// Returns the length of the first complete type in `sig`.
fn type_len(sig: &[u8]) -> io::Result<usize> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad D-Bus signature");
    match sig.first().ok_or_else(bad)? {
        b'a' => Ok(1 + type_len(&sig[1..])?),
        open @ (b'(' | b'{') => {
            let close = if *open == b'(' { b')' } else { b'}' };
            let mut i = 1;
            while *sig.get(i).ok_or_else(bad)? != close {
                i += type_len(&sig[i..])?;
            }
            Ok(i + 1)
        }
        _ => Ok(1),
    }
}

fn split_types(sig: &str) -> io::Result<Vec<String>> {
    let b = sig.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let n = type_len(&b[i..])?;
        out.push(sig[i..i + n].to_string());
        i += n;
    }
    Ok(out)
}

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn pad(&mut self, a: usize) {
        while self.buf.len() % a != 0 {
            self.buf.push(0);
        }
    }

    fn u32(&mut self, v: u32) {
        self.pad(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    fn sig(&mut self, s: &str) {
        self.buf.push(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::Byte(x) => self.buf.push(*x),
            Value::Bool(x) => self.u32(*x as u32),
            Value::I16(x) => {
                self.pad(2);
                self.buf.extend_from_slice(&x.to_le_bytes())
            }
            Value::U16(x) => {
                self.pad(2);
                self.buf.extend_from_slice(&x.to_le_bytes())
            }
            Value::I32(x) => self.u32(*x as u32),
            Value::U32(x) | Value::Fd(x) => self.u32(*x),
            Value::I64(x) => {
                self.pad(8);
                self.buf.extend_from_slice(&x.to_le_bytes())
            }
            Value::U64(x) => {
                self.pad(8);
                self.buf.extend_from_slice(&x.to_le_bytes())
            }
            Value::F64(x) => {
                self.pad(8);
                self.buf.extend_from_slice(&x.to_le_bytes())
            }
            Value::Str(s) | Value::Path(s) => self.string(s),
            Value::Sig(s) => self.sig(s),
            Value::Array(esig, items) => {
                self.u32(0);
                let len_pos = self.buf.len() - 4;
                self.pad(alignment(esig.as_bytes()[0]));
                let start = self.buf.len();
                for it in items {
                    self.value(it);
                }
                let len = (self.buf.len() - start) as u32;
                self.buf[len_pos..len_pos + 4].copy_from_slice(&len.to_le_bytes());
            }
            Value::Struct(fields) => {
                self.pad(8);
                for f in fields {
                    self.value(f);
                }
            }
            Value::DictEntry(k, val) => {
                self.pad(8);
                self.value(k);
                self.value(val);
            }
            Value::Variant(inner) => {
                self.sig(&inner.signature());
                self.value(inner);
            }
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn err() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, "truncated D-Bus message")
    }

    fn align(&mut self, a: usize) {
        self.pos = (self.pos + a - 1) & !(a - 1);
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos + n).ok_or_else(Self::err)?;
        self.pos += n;
        Ok(s)
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.align(4);
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn string(&mut self) -> io::Result<String> {
        let n = self.u32()? as usize;
        let s = String::from_utf8_lossy(self.take(n)?).into_owned();
        self.take(1)?;
        Ok(s)
    }

    fn sig(&mut self) -> io::Result<String> {
        let n = self.take(1)?[0] as usize;
        let s = String::from_utf8_lossy(self.take(n)?).into_owned();
        self.take(1)?;
        Ok(s)
    }

    fn value(&mut self, sig: &str) -> io::Result<Value> {
        let b = sig.as_bytes();
        Ok(match b[0] {
            b'y' => Value::Byte(self.take(1)?[0]),
            b'b' => Value::Bool(self.u32()? != 0),
            b'n' => {
                self.align(2);
                Value::I16(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
            }
            b'q' => {
                self.align(2);
                Value::U16(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
            }
            b'i' => Value::I32(self.u32()? as i32),
            b'u' => Value::U32(self.u32()?),
            b'h' => Value::Fd(self.u32()?),
            b'x' | b't' | b'd' => {
                self.align(8);
                let raw: [u8; 8] = self.take(8)?.try_into().unwrap();
                match b[0] {
                    b'x' => Value::I64(i64::from_le_bytes(raw)),
                    b't' => Value::U64(u64::from_le_bytes(raw)),
                    _ => Value::F64(f64::from_le_bytes(raw)),
                }
            }
            b's' => Value::Str(self.string()?),
            b'o' => Value::Path(self.string()?),
            b'g' => Value::Sig(self.sig()?),
            b'v' => {
                let s = self.sig()?;
                Value::Variant(Box::new(self.value(&s)?))
            }
            b'a' => {
                let esig = &sig[1..1 + type_len(&b[1..])?];
                let len = self.u32()? as usize;
                self.align(alignment(esig.as_bytes()[0]));
                let end = self.pos + len;
                if end > self.buf.len() {
                    return Err(Self::err());
                }
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.value(esig)?);
                }
                Value::Array(esig.to_string(), items)
            }
            b'(' => {
                self.align(8);
                let inner = &sig[1..type_len(b)? - 1];
                let mut fields = Vec::new();
                for t in split_types(inner)? {
                    fields.push(self.value(&t)?);
                }
                Value::Struct(fields)
            }
            b'{' => {
                self.align(8);
                let inner = split_types(&sig[1..type_len(b)? - 1])?;
                if inner.len() != 2 {
                    return Err(Self::err());
                }
                let k = self.value(&inner[0])?;
                let v = self.value(&inner[1])?;
                Value::DictEntry(Box::new(k), Box::new(v))
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown D-Bus type")),
        })
    }
}

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;
pub const FLAG_NO_REPLY: u8 = 1;

#[derive(Clone, Debug, Default)]
pub struct Message {
    pub msg_type: u8,
    pub flags: u8,
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub body: Vec<Value>,
    pub unix_fds: u32,
}

impl Message {
    pub fn method_call(dest: &str, path: &str, iface: &str, member: &str, body: Vec<Value>) -> Message {
        Message {
            msg_type: METHOD_CALL,
            path: Some(path.into()),
            interface: Some(iface.into()),
            member: Some(member.into()),
            destination: Some(dest.into()),
            body,
            ..Default::default()
        }
    }

    pub fn signal(path: &str, iface: &str, member: &str, body: Vec<Value>) -> Message {
        Message {
            msg_type: SIGNAL,
            path: Some(path.into()),
            interface: Some(iface.into()),
            member: Some(member.into()),
            body,
            ..Default::default()
        }
    }

    pub fn reply(&self, body: Vec<Value>) -> Message {
        Message {
            msg_type: METHOD_RETURN,
            reply_serial: Some(self.serial),
            destination: self.sender.clone(),
            body,
            ..Default::default()
        }
    }

    pub fn error(&self, name: &str, text: &str) -> Message {
        Message {
            msg_type: ERROR,
            reply_serial: Some(self.serial),
            destination: self.sender.clone(),
            error_name: Some(name.into()),
            body: vec![Value::str(text)],
            ..Default::default()
        }
    }

    pub fn signature(&self) -> String {
        self.body.iter().map(|v| v.signature()).collect()
    }

    pub fn is(&self, iface: &str, member: &str) -> bool {
        self.interface.as_deref() == Some(iface) && self.member.as_deref() == Some(member)
    }

    /// Human-readable error text for an ERROR message.
    pub fn error_text(&self) -> String {
        format!(
            "{}: {}",
            self.error_name.as_deref().unwrap_or("?"),
            self.body.first().and_then(|v| v.as_str()).unwrap_or("")
        )
    }

    fn encode(&self, serial: u32) -> Vec<u8> {
        let mut fields = Vec::new();
        let mut field = |code: u8, v: Value| {
            fields.push(Value::Struct(vec![Value::Byte(code), Value::variant(v)]));
        };
        if let Some(p) = &self.path {
            field(1, Value::Path(p.clone()));
        }
        if let Some(i) = &self.interface {
            field(2, Value::Str(i.clone()));
        }
        if let Some(m) = &self.member {
            field(3, Value::Str(m.clone()));
        }
        if let Some(e) = &self.error_name {
            field(4, Value::Str(e.clone()));
        }
        if let Some(r) = self.reply_serial {
            field(5, Value::U32(r));
        }
        if let Some(d) = &self.destination {
            field(6, Value::Str(d.clone()));
        }
        let sig = self.signature();
        if !sig.is_empty() {
            field(8, Value::Sig(sig));
        }
        if self.unix_fds > 0 {
            field(9, Value::U32(self.unix_fds));
        }
        let mut body = Writer { buf: Vec::new() };
        for v in &self.body {
            body.value(v);
        }
        let mut w = Writer { buf: vec![b'l', self.msg_type, self.flags, 1] };
        w.u32(body.buf.len() as u32);
        w.u32(serial);
        w.value(&Value::Array("(yv)".into(), fields));
        w.pad(8);
        w.buf.extend_from_slice(&body.buf);
        w.buf
    }

    fn decode(header: &[u8], body: &[u8]) -> io::Result<Message> {
        let mut r = Reader { buf: header, pos: 0 };
        let fixed = r.take(4)?;
        let mut m = Message { msg_type: fixed[1], flags: fixed[2], ..Default::default() };
        r.u32()?;
        m.serial = r.u32()?;
        let fields = r.value("a(yv)")?;
        let mut sig = String::new();
        for f in fields.as_array().unwrap_or(&[]) {
            if let Value::Struct(kv) = f {
                let (Value::Byte(code), Value::Variant(v)) = (&kv[0], &kv[1]) else { continue };
                match code {
                    1 => m.path = v.as_str().map(Into::into),
                    2 => m.interface = v.as_str().map(Into::into),
                    3 => m.member = v.as_str().map(Into::into),
                    4 => m.error_name = v.as_str().map(Into::into),
                    5 => m.reply_serial = v.as_i64().map(|x| x as u32),
                    6 => m.destination = v.as_str().map(Into::into),
                    7 => m.sender = v.as_str().map(Into::into),
                    8 => sig = v.as_str().unwrap_or("").into(),
                    9 => m.unix_fds = v.as_i64().unwrap_or(0) as u32,
                    _ => {}
                }
            }
        }
        let mut br = Reader { buf: body, pos: 0 };
        for t in split_types(&sig)? {
            m.body.push(br.value(&t)?);
        }
        Ok(m)
    }
}

/// Cloneable sending half of a bus connection.
#[derive(Clone)]
pub struct Sender {
    inner: Arc<(Mutex<UnixStream>, AtomicU32)>,
}

impl Sender {
    /// Sends a message (with optional fds) and returns the serial used.
    pub fn send_with_fds(&self, msg: &Message, fds: &[RawFd]) -> io::Result<u32> {
        let serial = self.inner.1.fetch_add(1, Ordering::SeqCst);
        let mut m = msg.clone();
        m.unix_fds = fds.len() as u32;
        let bytes = m.encode(serial);
        let stream = self.inner.0.lock().unwrap();
        sys::send_all_with_fds(stream.as_raw_fd(), &bytes, fds)?;
        Ok(serial)
    }

    pub fn send(&self, msg: &Message) -> io::Result<u32> {
        self.send_with_fds(msg, &[])
    }
}

pub struct Connection {
    reader: UnixStream,
    pub sender: Sender,
    pub unique_name: String,
    /// Messages received while waiting for a specific reply.
    backlog: std::collections::VecDeque<Message>,
}

fn session_bus_stream() -> io::Result<UnixStream> {
    let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().unwrap_or_default();
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no usable DBUS_SESSION_BUS_ADDRESS");
    let mut candidates: Vec<String> = addr.split(';').map(str::to_string).collect();
    if addr.is_empty() {
        if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
            candidates.push(format!("unix:path={rt}/bus"));
        }
    }
    for entry in candidates {
        let Some(params) = entry.strip_prefix("unix:") else { continue };
        for kv in params.split(',') {
            let res = if let Some(p) = kv.strip_prefix("path=") {
                UnixStream::connect(unescape(p))
            } else if let Some(p) = kv.strip_prefix("abstract=") {
                use std::os::linux::net::SocketAddrExt;
                std::os::unix::net::SocketAddr::from_abstract_name(unescape(p).as_bytes())
                    .and_then(|a| UnixStream::connect_addr(&a))
            } else {
                continue;
            };
            match res {
                Ok(s) => return Ok(s),
                Err(e) => last_err = e,
            }
        }
    }
    Err(last_err)
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_line(s: &mut UnixStream) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if s.read(&mut byte)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "bus closed during auth"));
        }
        line.push(byte[0]);
    }
    line.truncate(line.len() - 2);
    Ok(String::from_utf8_lossy(&line).into_owned())
}

impl Connection {
    pub fn session() -> io::Result<Connection> {
        use std::os::unix::fs::MetadataExt;
        let mut stream = session_bus_stream()?;
        let uid = std::fs::metadata("/proc/self")?.uid();
        let hex: String = uid.to_string().bytes().map(|b| format!("{b:02x}")).collect();
        stream.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())?;
        let resp = read_line(&mut stream)?;
        if !resp.starts_with("OK ") {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("D-Bus auth failed: {resp}")));
        }
        stream.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
        let resp = read_line(&mut stream)?;
        if resp != "AGREE_UNIX_FD" {
            return Err(io::Error::new(io::ErrorKind::Unsupported, format!("bus refused fd passing: {resp}")));
        }
        stream.write_all(b"BEGIN\r\n")?;
        let sender = Sender { inner: Arc::new((Mutex::new(stream.try_clone()?), AtomicU32::new(1))) };
        let mut conn = Connection { reader: stream, sender, unique_name: String::new(), backlog: Default::default() };
        let reply = conn.call(&Message::method_call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            vec![],
        ))?;
        conn.unique_name = reply.body.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
        Ok(conn)
    }

    fn read_message(&mut self) -> io::Result<Message> {
        let mut fixed = [0u8; 16];
        self.reader.read_exact(&mut fixed)?;
        if fixed[0] != b'l' {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "big-endian D-Bus messages unsupported"));
        }
        let body_len = u32::from_le_bytes(fixed[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(fixed[12..16].try_into().unwrap()) as usize;
        let header_len = (16 + fields_len + 7) & !7;
        let mut header = vec![0u8; header_len];
        header[..16].copy_from_slice(&fixed);
        self.reader.read_exact(&mut header[16..])?;
        let mut body = vec![0u8; body_len];
        self.reader.read_exact(&mut body)?;
        Message::decode(&header, &body)
    }

    /// Next incoming message (blocking).
    pub fn next(&mut self) -> io::Result<Message> {
        if let Some(m) = self.backlog.pop_front() {
            return Ok(m);
        }
        self.read_message()
    }

    /// Sends a call with fds and blocks until its reply; errors become `Err`.
    pub fn call_with_fds(&mut self, msg: &Message, fds: &[RawFd]) -> io::Result<Message> {
        let serial = self.sender.send_with_fds(msg, fds)?;
        self.wait_reply(serial)
    }

    /// Blocks until the reply to `serial` arrives; errors become `Err`.
    pub fn wait_reply(&mut self, serial: u32) -> io::Result<Message> {
        loop {
            let m = self.read_message()?;
            if m.reply_serial == Some(serial) && (m.msg_type == METHOD_RETURN || m.msg_type == ERROR) {
                if m.msg_type == ERROR {
                    return Err(io::Error::new(io::ErrorKind::Other, m.error_text()));
                }
                return Ok(m);
            }
            self.backlog.push_back(m);
        }
    }

    pub fn call(&mut self, msg: &Message) -> io::Result<Message> {
        self.call_with_fds(msg, &[])
    }

    pub fn bus_call(&mut self, member: &str, body: Vec<Value>) -> io::Result<Message> {
        self.call(&Message::method_call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            member,
            body,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_complex_body() {
        let body = vec![
            Value::U32(7),
            Value::Struct(vec![
                Value::I32(0),
                Value::dict_sv(vec![("label", Value::str("Full")), ("enabled", Value::Bool(true))]),
                Value::Array("v".into(), vec![Value::variant(Value::Struct(vec![Value::I32(1)]))]),
            ]),
            Value::Array("(iiay)".into(), vec![Value::Struct(vec![
                Value::I32(1),
                Value::I32(1),
                Value::Array("y".into(), vec![Value::Byte(1), Value::Byte(2), Value::Byte(3), Value::Byte(4)]),
            ])]),
            Value::U64(1 << 40),
            Value::Array("s".into(), vec![]),
        ];
        let m = Message::signal("/a", "b.c", "D", body.clone());
        let bytes = m.encode(5);
        let fields_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let hl = (16 + fields_len + 7) & !7;
        let d = Message::decode(&bytes[..hl], &bytes[hl..]).unwrap();
        assert_eq!(d.body, body);
        assert_eq!(d.serial, 5);
        assert_eq!(d.member.as_deref(), Some("D"));
        assert_eq!(d.signature(), "u(ia{sv}av)a(iiay)tas");
    }

    #[test]
    fn dict_lookup() {
        let d = Value::dict_sv(vec![("width", Value::U32(1920)), ("type", Value::str("raw"))]);
        assert_eq!(d.dict_get("width").and_then(|v| v.as_i64()), Some(1920));
        assert_eq!(d.dict_get("type").and_then(|v| v.as_str()), Some("raw"));
        assert!(d.dict_get("nope").is_none());
    }
}
