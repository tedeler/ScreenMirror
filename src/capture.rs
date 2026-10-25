//! Screen capture through KWin's `org.kde.KWin.ScreenShot2` D-Bus interface.
//!
//! Each frame is one `CaptureScreen` call: we pass the write end of a pipe as a
//! unix fd, KWin renders the output and streams the raw image into the pipe, and
//! the reply carries width/height/stride/format. KWin only accepts the call from
//! processes whose executable is listed in a .desktop file with
//! `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2` (see `install`).

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dbus::{Connection, Message, Value};
use crate::state::{Frame, Shared};
use crate::sys;

pub struct Capturer {
    conn: Connection,
    pub include_cursor: bool,
}

impl Capturer {
    pub fn new() -> io::Result<Capturer> {
        Ok(Capturer { conn: Connection::session()?, include_cursor: true })
    }

    pub fn capture_screen(&mut self, output: &str) -> io::Result<Frame> {
        let (r, w) = sys::pipe(false)?;
        let msg = Message::method_call(
            "org.kde.KWin",
            "/org/kde/KWin/ScreenShot2",
            "org.kde.KWin.ScreenShot2",
            "CaptureScreen",
            vec![
                Value::str(output),
                Value::dict_sv(vec![
                    ("include-cursor", Value::Bool(self.include_cursor)),
                    ("native-resolution", Value::Bool(true)),
                ]),
                Value::Fd(0),
            ],
        );
        let serial = self.conn.sender.send_with_fds(&msg, &[w.as_raw_fd()])?;
        drop(w);
        // KWin writes the pixels from a worker thread and closes its end when
        // done (or immediately on error), so draining first cannot deadlock.
        let mut data = Vec::new();
        File::from(r).read_to_end(&mut data)?;
        let reply = self.conn.wait_reply(serial)?;
        let results = reply.body.first().cloned().unwrap_or(Value::Array("{sv}".into(), vec![]));
        let get = |k: &str| results.dict_get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let kind = results.dict_get("type").and_then(|v| v.as_str()).unwrap_or("raw");
        if kind != "raw" {
            return Err(io::Error::new(io::ErrorKind::Unsupported, format!("unsupported screenshot type {kind}")));
        }
        let (width, height, stride, format) = (get("width") as u32, get("height") as u32, get("stride") as u32, get("format"));
        if width == 0 || height == 0 || (data.len() as u64) < stride as u64 * height as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("short screenshot: {width}x{height} stride {stride}, {} bytes", data.len()),
            ));
        }
        to_xrgb(width, height, stride, format, data)
    }
}

/// Converts a QImage-format buffer into 32-bit B,G,R,X rows.
fn to_xrgb(width: u32, height: u32, stride: u32, format: i64, mut data: Vec<u8>) -> io::Result<Frame> {
    let (w, h, s) = (width as usize, height as usize, stride as usize);
    let frame = |data, stride| Frame { width, height, stride, data, seq: 0 };
    match format {
        // QImage::Format_RGB32, ARGB32, ARGB32_Premultiplied: 0xAARRGGBB words.
        4 | 5 | 6 => Ok(frame(data, stride)),
        // RGBX8888, RGBA8888, RGBA8888_Premultiplied: R,G,B,A bytes.
        16 | 17 | 18 => {
            for row in data.chunks_mut(s).take(h) {
                for px in row[..w * 4].chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
            }
            Ok(frame(data, stride))
        }
        // RGB888 (13) and BGR888 (29): packed 24-bit.
        13 | 29 => {
            let mut out = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let p = &data[y * s + x * 3..y * s + x * 3 + 3];
                    let (r, g, b) = if format == 13 { (p[0], p[1], p[2]) } else { (p[2], p[1], p[0]) };
                    out[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[b, g, r, 255]);
                }
            }
            Ok(frame(out, width * 4))
        }
        // RGBX64, RGBA64, RGBA64_Premultiplied: 16-bit R,G,B,A (HDR / 10-bit outputs).
        25 | 26 | 27 => {
            let mut out = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let p = &data[y * s + x * 8..y * s + x * 8 + 8];
                    let c = |i: usize| p[i * 2 + 1];
                    out[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[c(2), c(1), c(0), 255]);
                }
            }
            Ok(frame(out, width * 4))
        }
        _ => Err(io::Error::new(io::ErrorKind::Unsupported, format!("unsupported QImage format {format}"))),
    }
}

/// Capture loop: grabs a new frame whenever the display side asks for one.
pub fn run(shared: Arc<Shared>, max_fps: u32, include_cursor: bool) {
    let mut capturer = match Capturer::new() {
        Ok(mut c) => {
            c.include_cursor = include_cursor;
            c
        }
        Err(e) => {
            eprintln!("screenmirror: capture: cannot connect to session bus: {e}");
            return;
        }
    };
    let min_interval = Duration::from_secs_f64(1.0 / max_fps.max(1) as f64);
    let mut seq = 0u64;
    let mut last_error: Option<String> = None;
    loop {
        let source = {
            let mut st = shared.state.lock().unwrap();
            while !st.quit && !(st.want_frame && st.source.is_some() && st.needs_frames()) {
                st = shared.cond.wait(st).unwrap();
            }
            if st.quit {
                return;
            }
            st.want_frame = false;
            st.source.clone().unwrap()
        };
        let started = Instant::now();
        match capturer.capture_screen(&source) {
            Ok(mut frame) => {
                seq += 1;
                frame.seq = seq;
                if last_error.take().is_some() {
                    eprintln!("screenmirror: capture recovered");
                }
                shared.update(|st| {
                    st.frame = Some(Arc::new(frame));
                    st.capture_error = None;
                });
            }
            Err(e) => {
                let msg = e.to_string();
                if last_error.as_deref() != Some(msg.as_str()) {
                    eprintln!("screenmirror: capture of {source} failed: {msg}");
                    if msg.contains("NoAuthorized") || msg.contains("not authorized") {
                        eprintln!("screenmirror: run `screenmirror --install` so KWin authorizes this executable");
                    }
                }
                last_error = Some(msg.clone());
                shared.update(|st| {
                    st.capture_error = Some(msg);
                    st.want_frame = true;
                });
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        if let Some(rest) = min_interval.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}
