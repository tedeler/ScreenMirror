//! State shared between the tray, capture and display threads.

use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, Mutex};

/// A region of the source screen as fractions (0..1) of its width/height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegionF {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Full,
    Zoom(RegionF),
    Black,
}

impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Full => "Full",
            Mode::Zoom(_) => "Zoom",
            Mode::Black => "Black",
        }
    }
}

/// A captured screen image, 32-bit pixels in wl_shm XRGB8888/ARGB8888 byte order (B,G,R,X).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
    pub seq: u64,
}

pub struct State {
    pub mode: Mode,
    /// Region selection overlay requested or active.
    pub selecting: bool,
    pub quit: bool,
    /// Output name (e.g. "eDP-1") to capture.
    pub source: Option<String>,
    /// Display side is ready for another frame.
    pub want_frame: bool,
    pub frame: Option<Arc<Frame>>,
    pub capture_error: Option<String>,
}

impl State {
    pub fn needs_frames(&self) -> bool {
        self.selecting || !matches!(self.mode, Mode::Black)
    }
}

pub struct Shared {
    pub state: Mutex<State>,
    pub cond: Condvar,
    wake_w: File,
}

impl Shared {
    /// Returns the shared state and the read end of the display thread's wake-up pipe.
    pub fn new() -> std::io::Result<(Arc<Shared>, OwnedFd)> {
        let (r, w) = crate::sys::pipe(true)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                mode: Mode::Full,
                selecting: false,
                quit: false,
                source: None,
                want_frame: true,
                frame: None,
                capture_error: None,
            }),
            cond: Condvar::new(),
            wake_w: File::from(w),
        });
        Ok((shared, r))
    }

    /// Wakes the display thread and any waiting worker thread.
    pub fn wake(&self) {
        let _ = (&self.wake_w).write(&[1]);
        self.cond.notify_all();
    }

    pub fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
        self.wake();
    }
}
