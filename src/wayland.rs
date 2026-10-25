//! Minimal Wayland wire-protocol client: object ids, request encoding,
//! event decoding and fd passing over the compositor's Unix socket.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::sys;

pub struct Event {
    pub obj: u32,
    pub opcode: u16,
    pub args: Vec<u8>,
}

pub struct Args<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Args<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Args { data, pos: 0 }
    }

    pub fn u32(&mut self) -> u32 {
        let v = self
            .data
            .get(self.pos..self.pos + 4)
            .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
            .unwrap_or(0);
        self.pos += 4;
        v
    }

    pub fn i32(&mut self) -> i32 {
        self.u32() as i32
    }

    pub fn fixed(&mut self) -> f64 {
        self.i32() as f64 / 256.0
    }

    pub fn array(&mut self) -> Vec<u8> {
        let len = self.u32() as usize;
        let v = self.data.get(self.pos..self.pos + len).unwrap_or(&[]).to_vec();
        self.pos += (len + 3) & !3;
        v
    }

    pub fn string(&mut self) -> String {
        let mut v = self.array();
        if v.last() == Some(&0) {
            v.pop();
        }
        String::from_utf8_lossy(&v).into_owned()
    }
}

pub struct Req {
    obj: u32,
    opcode: u16,
    buf: Vec<u8>,
    fds: Vec<RawFd>,
}

impl Req {
    pub fn new(obj: u32, opcode: u16) -> Req {
        Req { obj, opcode, buf: Vec::with_capacity(32), fds: Vec::new() }
    }

    pub fn u32(mut self, v: u32) -> Req {
        self.buf.extend_from_slice(&v.to_ne_bytes());
        self
    }

    pub fn i32(self, v: i32) -> Req {
        self.u32(v as u32)
    }

    pub fn fixed(self, v: f64) -> Req {
        self.i32((v * 256.0).round() as i32)
    }

    pub fn string(mut self, s: &str) -> Req {
        self.buf.extend_from_slice(&(s.len() as u32 + 1).to_ne_bytes());
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
        self
    }

    pub fn fd(mut self, fd: RawFd) -> Req {
        self.fds.push(fd);
        self
    }
}

pub struct Conn {
    sock: UnixStream,
    rbuf: Vec<u8>,
    fds: VecDeque<OwnedFd>,
    next_id: u32,
}

impl Conn {
    pub fn connect() -> io::Result<Conn> {
        let sock = if let Ok(fd) = std::env::var("WAYLAND_SOCKET") {
            let fd: RawFd = fd
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad WAYLAND_SOCKET"))?;
            std::env::remove_var("WAYLAND_SOCKET");
            unsafe { UnixStream::from_raw_fd(fd) }
        } else {
            let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
            let path = if display.starts_with('/') {
                display
            } else {
                let rt = std::env::var("XDG_RUNTIME_DIR")
                    .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR not set"))?;
                format!("{rt}/{display}")
            };
            UnixStream::connect(&path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?
        };
        Ok(Conn { sock, rbuf: Vec::new(), fds: VecDeque::new(), next_id: 2 })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn new_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn send(&mut self, r: Req) -> io::Result<()> {
        let size = 8 + r.buf.len() as u32;
        let mut msg = Vec::with_capacity(size as usize);
        msg.extend_from_slice(&r.obj.to_ne_bytes());
        msg.extend_from_slice(&((size << 16) | r.opcode as u32).to_ne_bytes());
        msg.extend_from_slice(&r.buf);
        sys::send_all_with_fds(self.sock.as_raw_fd(), &msg, &r.fds)
    }

    /// Blocks until at least some data arrives, then returns all complete events.
    pub fn read_events(&mut self) -> io::Result<Vec<Event>> {
        let mut buf = vec![0u8; 65536];
        let mut fds = Vec::new();
        let n = sys::recv_with_fds(self.sock.as_raw_fd(), &mut buf, &mut fds)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "compositor closed the connection"));
        }
        self.fds.extend(fds);
        self.rbuf.extend_from_slice(&buf[..n]);
        let mut events = Vec::new();
        let mut off = 0;
        while self.rbuf.len() - off >= 8 {
            let obj = u32::from_ne_bytes(self.rbuf[off..off + 4].try_into().unwrap());
            let w = u32::from_ne_bytes(self.rbuf[off + 4..off + 8].try_into().unwrap());
            let size = (w >> 16) as usize;
            if size < 8 || self.rbuf.len() - off < size {
                break;
            }
            events.push(Event { obj, opcode: (w & 0xffff) as u16, args: self.rbuf[off + 8..off + size].to_vec() });
            off += size;
        }
        self.rbuf.drain(..off);
        Ok(events)
    }

    /// Pops the next fd received from the compositor (fds arrive in event order).
    pub fn take_fd(&mut self) -> Option<OwnedFd> {
        self.fds.pop_front()
    }
}
