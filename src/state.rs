//! State shared between the tray, capture and display threads.

use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, Mutex};

use crate::update::UpdateNote;

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
    /// Mirror window is mapped. Cleared by Q while that window is focused, or by
    /// "Keine Spiegelung"; a tray mode sets it again. The window is created on the
    /// virtual desktop that is current at that moment.
    pub showing: bool,
    /// Full and Zoom fit the whole source inside the target and leave the rest black.
    /// When false, the image is stretched and aspect ratio is ignored.
    pub keep_aspect: bool,
    /// Destroy and recreate the mirror window so it lands on the virtual desktop
    /// that is current when the flag is set.
    pub repin: bool,
    /// Tray asked for the Über dialog. The display thread clears this after
    /// starting that process. Closing the dialog does not stop the mirror.
    pub show_about: bool,
    /// Outcome of the GitHub release check, shown on the tray menu.
    pub update: UpdateNote,
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
        self.showing && (self.selecting || !matches!(self.mode, Mode::Black))
    }

    /// Left click on the tray icon. Off removes the mirror the same way as
    /// Keine Spiegelung. On maps it again with the mode already stored, so a
    /// zoom region comes back instead of asking for a new one.
    pub fn toggle_mirror(&mut self) {
        self.showing = !self.showing;
        self.selecting = false;
    }
}

pub struct Shared {
    pub state: Mutex<State>,
    pub cond: Condvar,
    wake_w: File,
}

impl Shared {
    /// Returns the shared state and the read end of the display thread's wake-up pipe.
    /// `showing` is false when the process was started with `--no-mirror`.
    pub fn new(showing: bool) -> std::io::Result<(Arc<Shared>, OwnedFd)> {
        let (r, w) = crate::sys::pipe(true)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                mode: Mode::Full,
                selecting: false,
                showing,
                keep_aspect: true,
                repin: false,
                show_about: false,
                update: UpdateNote::Idle,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(mode: Mode, showing: bool, selecting: bool) -> State {
        State {
            mode,
            selecting,
            showing,
            keep_aspect: true,
            repin: false,
            show_about: false,
            update: UpdateNote::Idle,
            quit: false,
            source: None,
            want_frame: true,
            frame: None,
            capture_error: None,
        }
    }

    #[test]
    fn left_click_restores_full_screen() {
        let mut st = state(Mode::Full, true, false);
        st.toggle_mirror();
        assert!(!st.showing);
        assert_eq!(st.mode, Mode::Full);
        st.toggle_mirror();
        assert!(st.showing);
        assert_eq!(st.mode, Mode::Full);
        assert!(!st.selecting);
    }

    #[test]
    fn left_click_restores_the_zoom_region() {
        let region = RegionF { x: 0.1, y: 0.2, w: 0.3, h: 0.4 };
        let mut st = state(Mode::Zoom(region), true, false);
        st.toggle_mirror();
        assert!(!st.showing);
        st.toggle_mirror();
        assert!(st.showing);
        assert_eq!(st.mode, Mode::Zoom(region));
    }

    #[test]
    fn left_click_cancels_an_in_progress_selection_and_keeps_the_region() {
        let region = RegionF { x: 0.0, y: 0.0, w: 0.5, h: 0.5 };
        let mut st = state(Mode::Zoom(region), true, true);
        st.toggle_mirror();
        assert!(!st.showing);
        assert!(!st.selecting);
        assert_eq!(st.mode, Mode::Zoom(region));
        st.toggle_mirror();
        assert!(st.showing);
        assert!(!st.selecting);
        assert_eq!(st.mode, Mode::Zoom(region));
    }

    #[test]
    fn left_click_from_no_mirror_starts_the_stored_mode() {
        let mut st = state(Mode::Black, false, false);
        st.toggle_mirror();
        assert!(st.showing);
        assert_eq!(st.mode, Mode::Black);
    }
}
