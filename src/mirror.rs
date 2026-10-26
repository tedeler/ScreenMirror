//! Native Wayland display side.
//!
//! * The mirror is an `xdg_toplevel` fullscreen on the secondary output, so KWin
//!   puts it on the virtual desktop that is current when the window is created
//!   and hides it on every other desktop. Captured frames go into `wl_shm`
//!   buffers. With "Seitenverhältnis behalten" off, `wp_viewporter` stretches the
//!   frame (or, in Zoom, the selected region) to the output size. With it on, the
//!   toplevel stays a black 1×1 buffer stretched to the output, and a centered
//!   `wl_subsurface` shows the whole source at its own aspect ratio, so the unused
//!   area stays black and nothing is cropped. A layer-shell surface would be
//!   forced onto every desktop, which is why the mirror is not one.
//! * The zoom region selector is a layer surface on the source output that
//!   shows a dimmed freeze-frame; drag with the left button to select,
//!   Esc / right click cancels.
//! * Q unmaps the mirror when that window has keyboard focus, and leaves the process up.
//!   It is not a global shortcut, so Q still types everywhere else.
//! * Shift+S swaps the two screens while that same window has keyboard focus.
//!   It is not a global shortcut either.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use crate::state::{Frame, Mode, RegionF, Shared};
use crate::sys::{self, ShmMap};
use crate::tray::TrayHandle;
use crate::wayland::{Args, Conn, Event, Req};

const WL_SHM_FORMAT_XRGB8888: u32 = 1;
const LAYER_OVERLAY: u32 = 3;
const ANCHOR_ALL: u32 = 1 | 2 | 4 | 8;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const KEY_ESC: u32 = 1;
/// Linux evdev code for the letter q. Only handled while the mirror window is focused.
const KEY_Q: u32 = 16;
/// Linux evdev code for the letter s. With Shift, swaps screens while the mirror is focused.
const KEY_S: u32 = 31;
/// XKB Shift (bit 0). Caps Lock is bit 1 and does not count.
const XKB_MOD_SHIFT: u32 = 1;
const CURSOR_CROSSHAIR: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Obj {
    Registry,
    SyncCallback,
    FrameCallback(u32),
    Output(u32),
    Seat,
    Pointer,
    Keyboard,
    Buffer,
    LayerSurface(u32),
    XdgWmBase,
    XdgSurface(u32),
    XdgToplevel(u32),
    OutputOrder,
}

#[derive(Default, Clone)]
pub struct OutputInfo {
    pub global: u32,
    pub id: u32,
    pub name: String,
    pub description: String,
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    pub done: bool,
}

struct Slot {
    buffer: u32,
    map: ShmMap,
    width: u32,
    height: u32,
    stride: u32,
    busy: bool,
}

/// Centered child that shows the fitted frame. The parent surface is the black backdrop.
#[derive(Clone, Copy)]
struct Fit {
    surface: u32,
    subsurface: u32,
    viewport: u32,
}

#[derive(Clone, Copy)]
struct XdgWin {
    surface: u32,
    viewport: u32,
    xdg: u32,
    toplevel: u32,
    configured: bool,
    width: i32,
    height: i32,
    pending_w: i32,
    pending_h: i32,
    frame_pending: bool,
    fit: Option<Fit>,
    /// Buffer currently committed on the toplevel, so a fitted frame never reuses it.
    shown: u32,
}

#[derive(Clone, Copy)]
struct LayerSurf {
    surface: u32,
    viewport: u32,
    layer: u32,
    configured: bool,
    width: i32,
    height: i32,
    frame_pending: bool,
}

struct Selection {
    surf: LayerSurf,
    snapshot: Arc<Frame>,
    dimmed: Vec<u8>,
    start: Option<(f64, f64)>,
    cur: (f64, f64),
    dirty: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Drawn {
    Nothing,
    Black,
    /// Sequence, mode, and whether the frame was fitted (`true`) or stretched (`false`).
    Frame(u64, Mode, bool),
}

#[derive(Default)]
struct Globals {
    compositor: u32,
    shm: u32,
    viewporter: u32,
    subcompositor: u32,
    layer_shell: u32,
    wm_base: u32,
    cursor_shape: u32,
    seat: u32,
    output_order: u32,
}

pub struct App {
    conn: Conn,
    shared: Arc<Shared>,
    tray: Option<TrayHandle>,
    objects: HashMap<u32, Obj>,
    g: Globals,
    pending_globals: Vec<(u32, String, u32)>,
    pub outputs: Vec<OutputInfo>,
    output_order: Vec<String>,
    order_complete: bool,
    outputs_dirty: bool,
    pointer: u32,
    keyboard: u32,
    /// `wl_surface` that currently has keyboard focus, if it is one of ours.
    keyboard_focus: Option<u32>,
    /// Shift from `wl_keyboard.modifiers` (opcode 4). Not Caps Lock.
    shift_down: bool,
    /// When set, source and target are the `choose_outputs` pair reversed.
    /// Not persisted. A new process starts unswapped.
    swapped: bool,
    cursor_device: u32,
    slots: Vec<Slot>,
    black: Option<Slot>,
    mirror: Option<XdgWin>,
    drawn: Drawn,
    selection: Option<Selection>,
    selection_wait_seq: Option<u64>,
    source: Option<String>,
    target: Option<String>,
    /// `lastScreen` of the Plasma toolbar, kept across a failed config read.
    panel_screen: Option<i32>,
    sync_done: bool,
}

fn env_output(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

/// Reverse `source` and `target` when `swapped` and both names exist and differ.
/// Otherwise the pair is unchanged, including a missing name or two equal names.
fn exchange_pair(
    swapped: bool,
    source: Option<String>,
    target: Option<String>,
) -> (Option<String>, Option<String>) {
    match (swapped, &source, &target) {
        (true, Some(s), Some(t)) if s != t => (target, source),
        _ => (source, target),
    }
}

/// Plasma writes this when the desktop shell is `org.kde.plasma.desktop`.
fn panel_config_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("plasma-org.kde.plasma.desktop-appletsrc"))
}

/// Screen index of the Plasma toolbar, from `plasma-org.kde.plasma.desktop-appletsrc`.
///
/// That index is into KWin's output order (`kde_output_order_v1`), which is the
/// same order Plasma uses for `lastScreen`. It is not a connector name and not
/// "whatever is called eDP". Moving the panel writes a new `lastScreen`.
fn toolbar_screen(text: &str) -> Option<i32> {
    struct Cont {
        plugin: String,
        formfactor: i32,
        screen: i32,
        toolbar: bool,
        seen: bool,
    }
    impl Default for Cont {
        fn default() -> Self {
            // -1 means "no lastScreen yet", so a half-read containment is not screen 0.
            Cont { plugin: String::new(), formfactor: 0, screen: -1, toolbar: false, seen: false }
        }
    }
    let mut conts: HashMap<u32, Cont> = HashMap::new();
    let mut section: Option<Section> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(sec) = section_kind(line) {
            section = Some(sec);
            if let Section::Cont(id) = sec {
                conts.entry(id).or_default().seen = true;
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match section {
            Some(Section::Cont(id)) => {
                let c = conts.entry(id).or_default();
                c.seen = true;
                match key {
                    "plugin" => c.plugin = value.to_string(),
                    "formfactor" => c.formfactor = value.parse().unwrap_or(0),
                    "lastScreen" => c.screen = value.parse().unwrap_or(-1),
                    _ => {}
                }
            }
            Some(Section::Applet(id)) if key == "plugin" => {
                if matches!(
                    value,
                    "org.kde.plasma.icontasks" | "org.kde.plasma.kickoff" | "org.kde.plasma.systemtray"
                ) {
                    conts.entry(id).or_default().toolbar = true;
                }
            }
            _ => {}
        }
    }
    let mut panels: Vec<&Cont> = conts
        .values()
        .filter(|c| c.seen && c.screen >= 0 && (c.plugin == "org.kde.panel" || c.formfactor == 2 || c.formfactor == 3))
        .collect();
    if panels.iter().any(|c| c.toolbar) {
        panels.retain(|c| c.toolbar);
    }
    panels.sort_by_key(|c| c.screen);
    panels.first().map(|c| c.screen)
}

#[derive(Clone, Copy)]
enum Section {
    Cont(u32),
    Applet(u32),
}

/// `[Containments][N]` is a containment. `[Containments][N][Applets][M]` is one
/// of its applets. Deeper sections (system-tray internals) are ignored.
fn section_kind(line: &str) -> Option<Section> {
    if !line.starts_with('[') || !line.ends_with(']') {
        return None;
    }
    let parts: Vec<&str> = line[1..line.len() - 1].split("][").collect();
    let id = |s: &str| s.parse().ok();
    match parts.as_slice() {
        ["Containments", id_s] => Some(Section::Cont(id(id_s)?)),
        ["Containments", id_s, "Applets", _] => Some(Section::Applet(id(id_s)?)),
        _ => None,
    }
}

impl App {
    pub fn connect(shared: Arc<Shared>, tray: Option<TrayHandle>) -> io::Result<App> {
        let conn = Conn::connect()?;
        let mut app = App {
            conn,
            shared,
            tray,
            objects: HashMap::new(),
            g: Globals::default(),
            pending_globals: Vec::new(),
            outputs: Vec::new(),
            output_order: Vec::new(),
            order_complete: false,
            outputs_dirty: false,
            pointer: 0,
            keyboard: 0,
            keyboard_focus: None,
            shift_down: false,
            swapped: false,
            cursor_device: 0,
            slots: Vec::new(),
            black: None,
            mirror: None,
            drawn: Drawn::Nothing,
            selection: None,
            selection_wait_seq: None,
            source: None,
            target: None,
            panel_screen: None,
            sync_done: false,
        };
        let registry = app.conn.new_id();
        app.objects.insert(registry, Obj::Registry);
        app.conn.send(Req::new(1, 1).u32(registry))?;
        app.roundtrip()?;
        app.bind_globals(registry)?;
        app.roundtrip()?;
        app.roundtrip()?;
        for (what, id) in [
            ("wl_compositor", app.g.compositor),
            ("wl_shm", app.g.shm),
            ("wp_viewporter", app.g.viewporter),
            ("wl_subcompositor", app.g.subcompositor),
            ("zwlr_layer_shell_v1", app.g.layer_shell),
            ("xdg_wm_base", app.g.wm_base),
        ] {
            if id == 0 {
                return Err(io::Error::new(io::ErrorKind::Unsupported, format!("compositor lacks {what}")));
            }
        }
        Ok(app)
    }

    fn bind_globals(&mut self, registry: u32) -> io::Result<()> {
        for (name, iface, version) in std::mem::take(&mut self.pending_globals) {
            let want = match iface.as_str() {
                "wl_compositor" => 4,
                "wl_shm" => 1,
                "wp_viewporter" => 1,
                "wl_subcompositor" => 1,
                "zwlr_layer_shell_v1" => 3,
                "xdg_wm_base" => 6,
                "wp_cursor_shape_manager_v1" => 1,
                "wl_seat" => 5,
                "wl_output" => 4,
                "kde_output_order_v1" => 1,
                _ => continue,
            };
            let slot = match iface.as_str() {
                "wl_compositor" => Some(&mut self.g.compositor),
                "wl_shm" => Some(&mut self.g.shm),
                "wp_viewporter" => Some(&mut self.g.viewporter),
                "wl_subcompositor" => Some(&mut self.g.subcompositor),
                "zwlr_layer_shell_v1" => Some(&mut self.g.layer_shell),
                "xdg_wm_base" => Some(&mut self.g.wm_base),
                "wp_cursor_shape_manager_v1" => Some(&mut self.g.cursor_shape),
                "wl_seat" => Some(&mut self.g.seat),
                "kde_output_order_v1" => Some(&mut self.g.output_order),
                _ => None,
            };
            if slot.as_ref().is_some_and(|id| **id != 0) {
                continue;
            }
            let ver = version.min(want);
            let id = self.conn.new_id();
            self.conn.send(Req::new(registry, 0).u32(name).string(&iface).u32(ver).u32(id))?;
            match slot {
                Some(s) => *s = id,
                None => {
                    self.outputs.push(OutputInfo { global: name, id, scale: 1, ..Default::default() });
                    self.objects.insert(id, Obj::Output(name));
                }
            }
            match iface.as_str() {
                "wl_seat" => {
                    self.objects.insert(id, Obj::Seat);
                }
                "kde_output_order_v1" => {
                    self.objects.insert(id, Obj::OutputOrder);
                }
                "xdg_wm_base" => {
                    self.objects.insert(id, Obj::XdgWmBase);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn roundtrip(&mut self) -> io::Result<()> {
        let cb = self.conn.new_id();
        self.objects.insert(cb, Obj::SyncCallback);
        self.conn.send(Req::new(1, 0).u32(cb))?;
        self.sync_done = false;
        while !self.sync_done {
            for ev in self.conn.read_events()? {
                self.handle_event(ev)?;
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: Event) -> io::Result<()> {
        let mut a = Args::new(&ev.args);
        if ev.obj == 1 {
            if ev.opcode == 0 {
                let obj = a.u32();
                let code = a.u32();
                let msg = a.string();
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("Wayland protocol error on object {obj} (code {code}): {msg}"),
                ));
            }
            return Ok(());
        }
        let Some(&kind) = self.objects.get(&ev.obj) else { return Ok(()) };
        match (kind, ev.opcode) {
            (Obj::Registry, 0) => {
                let name = a.u32();
                let iface = a.string();
                let version = a.u32();
                self.pending_globals.push((name, iface.clone(), version));
                if iface == "wl_output" && self.g.compositor != 0 {
                    let registry = ev.obj;
                    self.bind_globals(registry)?;
                    self.outputs_dirty = true;
                }
            }
            (Obj::Registry, 1) => {
                let name = a.u32();
                if let Some(i) = self.outputs.iter().position(|o| o.global == name) {
                    let o = self.outputs.remove(i);
                    self.objects.remove(&o.id);
                    self.conn.send(Req::new(o.id, 0))?;
                    self.outputs_dirty = true;
                }
            }
            (Obj::SyncCallback, 0) => {
                self.objects.remove(&ev.obj);
                self.sync_done = true;
            }
            (Obj::FrameCallback(surface), 0) => {
                self.objects.remove(&ev.obj);
                if let Some(m) = self.mirror.as_mut().filter(|m| m.surface == surface) {
                    m.frame_pending = false;
                    self.draw_mirror()?;
                } else if let Some(s) = self.selection.as_mut().filter(|s| s.surf.surface == surface) {
                    s.surf.frame_pending = false;
                    self.draw_selection()?;
                }
            }
            (Obj::Output(global), op) => {
                if let Some(o) = self.outputs.iter_mut().find(|o| o.global == global) {
                    match op {
                        1 => {
                            let flags = a.u32();
                            let (w, h) = (a.i32(), a.i32());
                            if flags & 1 != 0 {
                                o.width = w;
                                o.height = h;
                            }
                        }
                        2 => {
                            o.done = true;
                            self.outputs_dirty = true;
                        }
                        3 => o.scale = a.i32(),
                        4 => o.name = a.string(),
                        5 => o.description = a.string(),
                        _ => {}
                    }
                }
            }
            (Obj::OutputOrder, 0) => {
                if self.order_complete {
                    self.output_order.clear();
                    self.order_complete = false;
                }
                self.output_order.push(a.string());
            }
            (Obj::OutputOrder, 1) => {
                self.order_complete = true;
                self.outputs_dirty = true;
            }
            (Obj::Seat, 0) => {
                let caps = a.u32();
                if caps & 1 != 0 && self.pointer == 0 {
                    self.pointer = self.conn.new_id();
                    self.objects.insert(self.pointer, Obj::Pointer);
                    self.conn.send(Req::new(self.g.seat, 0).u32(self.pointer))?;
                    if self.g.cursor_shape != 0 {
                        self.cursor_device = self.conn.new_id();
                        self.conn.send(Req::new(self.g.cursor_shape, 1).u32(self.cursor_device).u32(self.pointer))?;
                    }
                }
                if caps & 2 != 0 && self.keyboard == 0 {
                    self.keyboard = self.conn.new_id();
                    self.objects.insert(self.keyboard, Obj::Keyboard);
                    self.conn.send(Req::new(self.g.seat, 1).u32(self.keyboard))?;
                }
            }
            (Obj::Pointer, op) => self.pointer_event(op, &mut a)?,
            (Obj::Keyboard, 0) => {
                drop(self.conn.take_fd());
            }
            (Obj::Keyboard, 1) => {
                let _serial = a.u32();
                self.keyboard_focus = Some(a.u32());
            }
            (Obj::Keyboard, 2) => {
                let _serial = a.u32();
                let surface = a.u32();
                if self.keyboard_focus == Some(surface) {
                    self.keyboard_focus = None;
                }
            }
            (Obj::Keyboard, 3) => {
                let (_serial, _time, key, state) = (a.u32(), a.u32(), a.u32(), a.u32());
                if state != 1 {
                    return Ok(());
                }
                let mirror_focused = self.mirror.as_ref().is_some_and(|m| self.keyboard_focus == Some(m.surface));
                if key == KEY_ESC && self.selection.is_some() {
                    self.finish_selection(false)?;
                } else if key == KEY_Q && mirror_focused {
                    eprintln!("screenmirror: Q on the focused mirror window");
                    self.stop_mirror()?;
                } else if key == KEY_S && self.shift_down && mirror_focused {
                    eprintln!("screenmirror: Shift+S on the focused mirror window");
                    self.swap_outputs()?;
                }
            }
            (Obj::Keyboard, 4) => {
                let _serial = a.u32();
                let mods_depressed = a.u32();
                let mods_latched = a.u32();
                let _mods_locked = a.u32();
                let _group = a.u32();
                self.shift_down = (mods_depressed | mods_latched) & XKB_MOD_SHIFT != 0;
            }
            (Obj::Buffer, 0) => {
                for s in self.slots.iter_mut().chain(self.black.iter_mut()) {
                    if s.buffer == ev.obj {
                        s.busy = false;
                    }
                }
            }
            (Obj::LayerSurface(surface), 0) => {
                let (serial, w, h) = (a.u32(), a.u32() as i32, a.u32() as i32);
                self.conn.send(Req::new(ev.obj, 6).u32(serial))?;
                if let Some(s) = self.selection.as_mut().filter(|s| s.surf.surface == surface) {
                    s.surf.configured = true;
                    (s.surf.width, s.surf.height) = (w.max(1), h.max(1));
                    s.dirty = true;
                    self.draw_selection()?;
                }
            }
            (Obj::LayerSurface(surface), 1) => {
                if self.selection.as_ref().is_some_and(|s| s.surf.surface == surface) {
                    self.finish_selection(false)?;
                }
            }
            (Obj::XdgWmBase, 0) => {
                let serial = a.u32();
                self.conn.send(Req::new(ev.obj, 3).u32(serial))?;
            }
            (Obj::XdgToplevel(surface), 0) => {
                let (w, h) = (a.i32(), a.i32());
                let _states = a.array();
                if let Some(m) = self.mirror.as_mut().filter(|m| m.surface == surface) {
                    if w > 0 {
                        m.pending_w = w;
                    }
                    if h > 0 {
                        m.pending_h = h;
                    }
                }
            }
            (Obj::XdgToplevel(surface), 1) => {
                if self.mirror.as_ref().is_some_and(|m| m.surface == surface) {
                    eprintln!("screenmirror: compositor closed the mirror window");
                    self.stop_mirror()?;
                }
            }
            (Obj::XdgSurface(surface), 0) => {
                let serial = a.u32();
                if self.mirror.as_ref().is_none_or(|m| m.surface != surface) {
                    self.conn.send(Req::new(ev.obj, 4).u32(serial))?;
                    return Ok(());
                }
                let fallback = self.output_logical(self.target.as_deref().unwrap_or(""));
                let (xdg, width, height) = {
                    let m = self.mirror.as_mut().unwrap();
                    let w = if m.pending_w > 0 { m.pending_w } else { fallback.0 };
                    let h = if m.pending_h > 0 { m.pending_h } else { fallback.1 };
                    if (w, h) != (m.width, m.height) || !m.configured {
                        self.drawn = Drawn::Nothing;
                    }
                    m.configured = true;
                    (m.width, m.height) = (w.max(1), h.max(1));
                    m.frame_pending = false;
                    (m.xdg, m.width, m.height)
                };
                self.conn.send(Req::new(xdg, 3).i32(0).i32(0).i32(width).i32(height))?;
                self.conn.send(Req::new(xdg, 4).u32(serial))?;
                self.draw_mirror()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn pointer_event(&mut self, op: u16, a: &mut Args) -> io::Result<()> {
        let Some(sel) = self.selection.as_mut() else { return Ok(()) };
        match op {
            0 => {
                let serial = a.u32();
                let surface = a.u32();
                let (x, y) = (a.fixed(), a.fixed());
                if surface == sel.surf.surface {
                    sel.cur = (x, y);
                    if self.cursor_device != 0 {
                        self.conn.send(Req::new(self.cursor_device, 1).u32(serial).u32(CURSOR_CROSSHAIR))?;
                    }
                }
            }
            2 => {
                let _time = a.u32();
                sel.cur = (a.fixed(), a.fixed());
                if sel.start.is_some() {
                    sel.dirty = true;
                    self.draw_selection()?;
                }
            }
            3 => {
                let (_serial, _time, button, state) = (a.u32(), a.u32(), a.u32(), a.u32());
                match (button, state) {
                    (BTN_LEFT, 1) => {
                        sel.start = Some(sel.cur);
                        sel.dirty = true;
                        self.draw_selection()?;
                    }
                    (BTN_LEFT, 0) if sel.start.is_some() => self.finish_selection(true)?,
                    (BTN_RIGHT, 1) => self.finish_selection(false)?,
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn output_id(&self, name: &str) -> Option<u32> {
        self.outputs.iter().find(|o| o.name == name).map(|o| o.id)
    }

    /// Outputs in Plasma's screen order: `kde_output_order_v1`, then any
    /// connected output that order has not mentioned yet.
    fn ordered_outputs(&self) -> Vec<&OutputInfo> {
        let ready: Vec<&OutputInfo> = self.outputs.iter().filter(|o| o.done && !o.name.is_empty()).collect();
        let mut v: Vec<&OutputInfo> =
            self.output_order.iter().filter_map(|n| ready.iter().find(|o| &o.name == n).copied()).collect();
        v.extend(ready.iter().filter(|o| !self.output_order.contains(&o.name)).copied());
        v
    }

    fn refresh_panel_screen(&mut self) {
        let Some(path) = panel_config_path() else { return };
        match std::fs::read_to_string(path) {
            Ok(text) => self.panel_screen = toolbar_screen(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.panel_screen = None,
            Err(_) => {}
        }
    }

    /// Source is the output that holds the Plasma toolbar. Target is the other one.
    pub fn choose_outputs(&self) -> (Option<String>, Option<String>) {
        let ordered = self.ordered_outputs();
        let panel = self
            .panel_screen
            .filter(|i| *i >= 0)
            .and_then(|i| ordered.get(i as usize).map(|o| o.name.clone()));
        let source = env_output("SCREENMIRROR_SOURCE")
            .filter(|n| ordered.iter().any(|o| &o.name == n))
            .or(panel)
            .or_else(|| ordered.first().map(|o| o.name.clone()));
        let target = env_output("SCREENMIRROR_TARGET")
            .filter(|n| ordered.iter().any(|o| &o.name == n) && Some(n) != source.as_ref())
            .or_else(|| ordered.iter().find(|o| Some(&o.name) != source.as_ref()).map(|o| o.name.clone()));
        (source, target)
    }

    /// Base pair from `choose_outputs`. If that pair has two different outputs,
    /// flip `swapped` and apply it immediately. One screen leaves `swapped` as it is.
    fn swap_outputs(&mut self) -> io::Result<()> {
        let (source, target) = self.choose_outputs();
        let can_swap = matches!((&source, &target), (Some(s), Some(t)) if s != t);
        if !can_swap {
            eprintln!("screenmirror: Shift+S ignored; no second screen, swap unchanged");
            return Ok(());
        }
        self.swapped = !self.swapped;
        let (source, target) = exchange_pair(self.swapped, source, target);
        eprintln!(
            "screenmirror: swapped screens: source {}, target {}",
            source.as_deref().unwrap_or("(none)"),
            target.as_deref().unwrap_or("(none)")
        );
        self.reconfigure_outputs()
    }

    fn reconfigure_outputs(&mut self) -> io::Result<()> {
        self.outputs_dirty = false;
        self.refresh_panel_screen();
        let (base_source, base_target) = self.choose_outputs();
        let (source, target) = exchange_pair(self.swapped, base_source.clone(), base_target);
        if source != self.source {
            if let Some(sel) = self.selection.take() {
                self.destroy_layer(sel.surf)?;
                self.selection_wait_seq = None;
                self.shared.update(|st| st.selecting = false);
            }
            let why = if source != base_source {
                "swapped"
            } else if env_output("SCREENMIRROR_SOURCE").is_some() {
                "override"
            } else if self.panel_screen.is_some_and(|i| {
                self.ordered_outputs().get(i as usize).is_some_and(|o| Some(&o.name) == source.as_ref())
            }) {
                "panel"
            } else if source.is_some() {
                "first screen"
            } else {
                "none"
            };
            eprintln!("screenmirror: source screen: {} ({why})", source.as_deref().unwrap_or("(none)"));
            self.source = source.clone();
            self.shared.update(|st| {
                st.source = source;
                st.frame = None;
                st.want_frame = true;
            });
            self.drawn = Drawn::Nothing;
        }
        let showing = self.shared.state.lock().unwrap().showing;
        if !showing {
            self.destroy_mirror_window()?;
            if target != self.target {
                eprintln!(
                    "screenmirror: mirror target while stopped: {}",
                    target.as_deref().unwrap_or("(none)")
                );
                self.target = target;
            }
        } else if target != self.target || (target.is_some() && self.mirror.is_none()) {
            self.destroy_mirror_window()?;
            match &target {
                Some(t) => {
                    eprintln!("screenmirror: mirroring onto {t} (current virtual desktop only)");
                    let t = t.clone();
                    self.mirror = Some(self.create_xdg(&t)?);
                }
                None => eprintln!("screenmirror: no second screen connected; waiting"),
            }
            self.target = target;
            self.drawn = Drawn::Nothing;
        }
        Ok(())
    }

    fn output_logical(&self, name: &str) -> (i32, i32) {
        let Some(o) = self.outputs.iter().find(|o| o.name == name) else {
            return (1, 1);
        };
        let scale = o.scale.max(1);
        ((o.width / scale).max(1), (o.height / scale).max(1))
    }

    /// Fullscreen toplevel on `output`. KWin associates a new window with the
    /// virtual desktop that is current when it is created, and not with the others.
    fn create_xdg(&mut self, output: &str) -> io::Result<XdgWin> {
        let output_id = self.output_id(output).unwrap_or(0);
        let surface = self.conn.new_id();
        self.conn.send(Req::new(self.g.compositor, 0).u32(surface))?;
        let viewport = self.conn.new_id();
        self.conn.send(Req::new(self.g.viewporter, 1).u32(viewport).u32(surface))?;
        let xdg = self.conn.new_id();
        self.objects.insert(xdg, Obj::XdgSurface(surface));
        self.conn.send(Req::new(self.g.wm_base, 2).u32(xdg).u32(surface))?;
        let toplevel = self.conn.new_id();
        self.objects.insert(toplevel, Obj::XdgToplevel(surface));
        self.conn.send(Req::new(xdg, 1).u32(toplevel))?;
        self.conn.send(Req::new(toplevel, 2).string("ScreenMirror"))?;
        self.conn.send(Req::new(toplevel, 3).string("screenmirror"))?;
        self.conn.send(Req::new(toplevel, 11).u32(output_id))?;
        self.conn.send(Req::new(surface, 6))?;
        Ok(XdgWin {
            surface,
            viewport,
            xdg,
            toplevel,
            configured: false,
            width: 0,
            height: 0,
            pending_w: 0,
            pending_h: 0,
            frame_pending: false,
            fit: None,
            shown: 0,
        })
    }

    fn destroy_fit(&mut self, fit: Fit) -> io::Result<()> {
        // The subsurface role has to go before the surface, or the compositor
        // rejects the destroy. Unmapping is immediate.
        self.conn.send(Req::new(fit.subsurface, 0))?;
        self.conn.send(Req::new(fit.viewport, 0))?;
        self.conn.send(Req::new(fit.surface, 0))
    }

    fn clear_fit(&mut self) -> io::Result<()> {
        if let Some(fit) = self.mirror.as_mut().and_then(|m| m.fit.take()) {
            self.destroy_fit(fit)?;
        }
        Ok(())
    }

    /// A subsurface of the mirror window. It starts synchronized, so its commit
    /// is applied together with the next parent commit.
    fn ensure_fit(&mut self) -> io::Result<()> {
        if self.mirror.as_ref().is_some_and(|m| m.fit.is_some()) {
            return Ok(());
        }
        let Some(parent) = self.mirror.as_ref().map(|m| m.surface) else {
            return Ok(());
        };
        let surface = self.conn.new_id();
        self.conn.send(Req::new(self.g.compositor, 0).u32(surface))?;
        let subsurface = self.conn.new_id();
        self.conn.send(Req::new(self.g.subcompositor, 1).u32(subsurface).u32(surface).u32(parent))?;
        let viewport = self.conn.new_id();
        self.conn.send(Req::new(self.g.viewporter, 1).u32(viewport).u32(surface))?;
        if let Some(m) = self.mirror.as_mut() {
            m.fit = Some(Fit { surface, subsurface, viewport });
        }
        Ok(())
    }

    fn destroy_xdg(&mut self, w: XdgWin) -> io::Result<()> {
        if let Some(fit) = w.fit {
            self.destroy_fit(fit)?;
        }
        self.objects.remove(&w.toplevel);
        self.objects.remove(&w.xdg);
        self.conn.send(Req::new(w.toplevel, 0))?;
        self.conn.send(Req::new(w.xdg, 0))?;
        self.conn.send(Req::new(w.viewport, 0))?;
        self.conn.send(Req::new(w.surface, 0))
    }

    fn destroy_mirror_window(&mut self) -> io::Result<()> {
        if let Some(w) = self.mirror.take() {
            self.destroy_xdg(w)?;
            self.drawn = Drawn::Nothing;
        }
        Ok(())
    }

    /// Removes the mirror window and the zoom selector. The process and tray stay up.
    fn stop_mirror(&mut self) -> io::Result<()> {
        self.destroy_mirror_window()?;
        if let Some(sel) = self.selection.take() {
            self.destroy_layer(sel.surf)?;
            self.selection_wait_seq = None;
        }
        self.shared.update(|st| {
            st.showing = false;
            st.selecting = false;
        });
        if let Some(t) = &self.tray {
            t.mode_changed();
        }
        eprintln!("screenmirror: mirror removed; tray still running");
        Ok(())
    }

    fn create_layer(&mut self, output: &str, namespace: &str, keyboard: bool) -> io::Result<LayerSurf> {
        let output_id = self.output_id(output).unwrap_or(0);
        let surface = self.conn.new_id();
        self.conn.send(Req::new(self.g.compositor, 0).u32(surface))?;
        let viewport = self.conn.new_id();
        self.conn.send(Req::new(self.g.viewporter, 1).u32(viewport).u32(surface))?;
        let layer = self.conn.new_id();
        self.conn.send(
            Req::new(self.g.layer_shell, 0)
                .u32(layer)
                .u32(surface)
                .u32(output_id)
                .u32(LAYER_OVERLAY)
                .string(namespace),
        )?;
        self.objects.insert(layer, Obj::LayerSurface(surface));
        self.conn.send(Req::new(layer, 0).u32(0).u32(0))?;
        self.conn.send(Req::new(layer, 1).u32(ANCHOR_ALL))?;
        self.conn.send(Req::new(layer, 2).i32(-1))?;
        self.conn.send(Req::new(layer, 4).u32(keyboard as u32))?;
        self.conn.send(Req::new(surface, 6))?;
        Ok(LayerSurf {
            surface,
            viewport,
            layer,
            configured: false,
            width: 0,
            height: 0,
            frame_pending: false,
        })
    }

    fn destroy_layer(&mut self, l: LayerSurf) -> io::Result<()> {
        self.objects.remove(&l.layer);
        self.conn.send(Req::new(l.layer, 7))?;
        self.conn.send(Req::new(l.viewport, 0))?;
        self.conn.send(Req::new(l.surface, 0))
    }

    fn new_slot(&mut self, width: u32, height: u32, stride: u32) -> io::Result<Slot> {
        let size = stride as usize * height as usize;
        let map = ShmMap::new(size)?;
        let pool = self.conn.new_id();
        self.conn.send(Req::new(self.g.shm, 0).u32(pool).fd(map.fd.as_raw_fd()).i32(size as i32))?;
        let buffer = self.conn.new_id();
        self.conn.send(
            Req::new(pool, 0)
                .u32(buffer)
                .i32(0)
                .i32(width as i32)
                .i32(height as i32)
                .i32(stride as i32)
                .u32(WL_SHM_FORMAT_XRGB8888),
        )?;
        self.conn.send(Req::new(pool, 1))?;
        self.objects.insert(buffer, Obj::Buffer);
        Ok(Slot { buffer, map, width, height, stride, busy: false })
    }

    /// Index of an idle buffer of the given geometry, allocating one if needed.
    /// `skip` is a buffer that is still the committed contents of some surface.
    /// It may already have been released, and attaching it elsewhere is an error.
    fn slot_for(&mut self, width: u32, height: u32, stride: u32, skip: u32) -> io::Result<usize> {
        let mut i = 0;
        while i < self.slots.len() {
            let s = &self.slots[i];
            if !s.busy && (s.width, s.height, s.stride) != (width, height, stride) {
                let s = self.slots.remove(i);
                self.objects.remove(&s.buffer);
                self.conn.send(Req::new(s.buffer, 0))?;
            } else {
                i += 1;
            }
        }
        if let Some(i) = self.slots.iter().position(|s| !s.busy && s.buffer != skip) {
            return Ok(i);
        }
        let slot = self.new_slot(width, height, stride)?;
        self.slots.push(slot);
        Ok(self.slots.len() - 1)
    }

    fn present(
        &mut self,
        surface: u32,
        viewport: u32,
        width: i32,
        height: i32,
        buffer: u32,
        src: Option<(f64, f64, f64, f64)>,
        callback: bool,
    ) -> io::Result<()> {
        let (x, y, w, h) = src.unwrap_or((-1.0, -1.0, -1.0, -1.0));
        self.conn.send(Req::new(viewport, 1).fixed(x).fixed(y).fixed(w).fixed(h))?;
        self.conn.send(Req::new(viewport, 2).i32(width).i32(height))?;
        if callback {
            let cb = self.conn.new_id();
            self.objects.insert(cb, Obj::FrameCallback(surface));
            self.conn.send(Req::new(surface, 3).u32(cb))?;
        }
        self.conn.send(Req::new(surface, 1).u32(buffer).i32(0).i32(0))?;
        self.conn.send(Req::new(surface, 9).i32(0).i32(0).i32(i32::MAX).i32(i32::MAX))?;
        self.conn.send(Req::new(surface, 6))
    }

    fn ensure_black(&mut self) -> io::Result<u32> {
        if self.black.is_none() {
            let mut s = self.new_slot(1, 1, 4)?;
            s.map.as_mut_slice().copy_from_slice(&[0, 0, 0, 255]);
            self.black = Some(s);
        }
        Ok(self.black.as_ref().unwrap().buffer)
    }

    fn attach_black(&mut self, surface: u32, viewport: u32, width: i32, height: i32) -> io::Result<()> {
        let buffer = self.ensure_black()?;
        self.conn.send(Req::new(viewport, 1).fixed(-1.0).fixed(-1.0).fixed(-1.0).fixed(-1.0))?;
        self.conn.send(Req::new(viewport, 2).i32(width).i32(height))?;
        self.conn.send(Req::new(surface, 1).u32(buffer).i32(0).i32(0))?;
        self.conn.send(Req::new(surface, 9).i32(0).i32(0).i32(i32::MAX).i32(i32::MAX))
    }

    /// Applies pending parent state, including a synchronized subsurface.
    fn commit_frame(&mut self, surface: u32) -> io::Result<()> {
        let cb = self.conn.new_id();
        self.objects.insert(cb, Obj::FrameCallback(surface));
        self.conn.send(Req::new(surface, 3).u32(cb))?;
        self.conn.send(Req::new(surface, 2).i32(0).i32(0).i32(i32::MAX).i32(i32::MAX))?;
        self.conn.send(Req::new(surface, 6))
    }

    fn present_black(&mut self, surface: u32, viewport: u32, width: i32, height: i32, callback: bool) -> io::Result<()> {
        let buffer = self.ensure_black()?;
        self.present(surface, viewport, width, height, buffer, None, callback)
    }

    fn draw_mirror(&mut self) -> io::Result<()> {
        let (configured, frame_pending, surface, viewport, width, height) = {
            let Some(m) = self.mirror.as_ref() else { return Ok(()) };
            (m.configured, m.frame_pending, m.surface, m.viewport, m.width, m.height)
        };
        if !configured || frame_pending {
            return Ok(());
        }
        let (mode, frame, keep_aspect) = {
            let st = self.shared.state.lock().unwrap();
            (st.mode, st.frame.clone(), st.keep_aspect)
        };
        let frame = frame.filter(|_| mode != Mode::Black);
        let keep = keep_aspect && frame.is_some();
        let want = match &frame {
            Some(f) => Drawn::Frame(f.seq, mode, keep),
            None => Drawn::Black,
        };
        if want == self.drawn {
            return Ok(());
        }
        let shown = self.mirror.as_ref().map(|m| m.shown).unwrap_or(0);
        if !keep {
            self.clear_fit()?;
        }
        match frame {
            None => {
                let black_id = self.black.as_ref().map(|s| s.buffer).unwrap_or(0);
                if black_id == 0 || shown != black_id {
                    self.present_black(surface, viewport, width, height, false)?;
                    let black_id = self.black.as_ref().unwrap().buffer;
                    self.mirror.as_mut().unwrap().shown = black_id;
                }
            }
            Some(f) => {
                let i = self.slot_for(f.width, f.height, f.stride, shown)?;
                let slot = &mut self.slots[i];
                let n = slot.map.as_mut_slice().len().min(f.data.len());
                slot.map.as_mut_slice()[..n].copy_from_slice(&f.data[..n]);
                slot.busy = true;
                let buffer = slot.buffer;
                let src = match mode {
                    Mode::Zoom(r) => Some(region_px(&r, f.width, f.height)),
                    _ => None,
                };
                if keep {
                    let (sw, sh) = match src {
                        Some((_, _, w, h)) => (w, h),
                        None => (f.width as f64, f.height as f64),
                    };
                    let (x, y, dw, dh) = contain_rect(sw, sh, width, height);
                    if !matches!(self.drawn, Drawn::Frame(_, _, true)) {
                        eprintln!(
                            "screenmirror: keeping aspect, content {dw}×{dh} at {x},{y} on {width}×{height}"
                        );
                    }
                    let black_id = self.black.as_ref().map(|s| s.buffer).unwrap_or(0);
                    if black_id == 0 || shown != black_id {
                        // Pending until the parent commit below, so the fitted child
                        // and the black backdrop appear together.
                        self.attach_black(surface, viewport, width, height)?;
                    }
                    self.ensure_fit()?;
                    let fit = self.mirror.as_ref().unwrap().fit.unwrap();
                    self.conn.send(Req::new(fit.subsurface, 1).i32(x).i32(y))?;
                    self.present(fit.surface, fit.viewport, dw, dh, buffer, src, false)?;
                    self.commit_frame(surface)?;
                    let black_id = self.black.as_ref().unwrap().buffer;
                    self.mirror.as_mut().unwrap().shown = black_id;
                } else {
                    if matches!(self.drawn, Drawn::Frame(_, _, true)) {
                        eprintln!("screenmirror: stretching to {width}×{height}");
                    }
                    self.present(surface, viewport, width, height, buffer, src, true)?;
                    self.mirror.as_mut().unwrap().shown = buffer;
                }
                self.mirror.as_mut().unwrap().frame_pending = true;
                self.shared.update(|st| st.want_frame = true);
            }
        }
        self.drawn = want;
        Ok(())
    }

    fn start_selection(&mut self, snapshot: Arc<Frame>) -> io::Result<()> {
        let Some(source) = self.source.clone() else { return Ok(()) };
        let dimmed = snapshot.data.iter().map(|&b| (b as u16 * 2 / 5) as u8).collect();
        let surf = self.create_layer(&source, "screenmirror-select", true)?;
        eprintln!("screenmirror: select a region on {source} (drag; Esc or right click cancels)");
        self.selection = Some(Selection { surf, snapshot, dimmed, start: None, cur: (0.0, 0.0), dirty: true });
        Ok(())
    }

    fn selection_rect(sel: &Selection) -> Option<(f64, f64, f64, f64)> {
        let (sx, sy) = sel.start?;
        let (cx, cy) = sel.cur;
        let (w, h) = (sel.surf.width as f64, sel.surf.height as f64);
        let x0 = sx.min(cx).clamp(0.0, w);
        let y0 = sy.min(cy).clamp(0.0, h);
        let x1 = sx.max(cx).clamp(0.0, w);
        let y1 = sy.max(cy).clamp(0.0, h);
        Some((x0, y0, x1 - x0, y1 - y0))
    }

    fn draw_selection(&mut self) -> io::Result<()> {
        let Some(sel) = self.selection.as_ref() else { return Ok(()) };
        if !sel.surf.configured || sel.surf.frame_pending || !sel.dirty {
            return Ok(());
        }
        let snap = sel.snapshot.clone();
        let (fw, fh, stride) = (snap.width as usize, snap.height as usize, snap.stride as usize);
        let rect = Self::selection_rect(sel).map(|(x, y, w, h)| {
            let sx = fw as f64 / sel.surf.width as f64;
            let sy = fh as f64 / sel.surf.height as f64;
            let x0 = ((x * sx) as usize).min(fw);
            let y0 = ((y * sy) as usize).min(fh);
            let x1 = (((x + w) * sx).ceil() as usize).min(fw);
            let y1 = (((y + h) * sy).ceil() as usize).min(fh);
            (x0, y0, x1, y1, (sx.max(1.0) * 2.0) as usize)
        });
        let i = self.slot_for(snap.width, snap.height, snap.stride, 0)?;
        let sel = self.selection.as_mut().unwrap();
        let slot = &mut self.slots[i];
        let buf = slot.map.as_mut_slice();
        let n = buf.len().min(sel.dimmed.len());
        buf[..n].copy_from_slice(&sel.dimmed[..n]);
        if let Some((x0, y0, x1, y1, t)) = rect {
            for y in y0..y1 {
                let row = y * stride;
                buf[row + x0 * 4..row + x1 * 4].copy_from_slice(&snap.data[row + x0 * 4..row + x1 * 4]);
                let edge_row = y < y0 + t || y + t >= y1;
                for x in x0..x1 {
                    if edge_row || x < x0 + t || x + t >= x1 {
                        buf[row + x * 4..row + x * 4 + 4].copy_from_slice(&[0xe9, 0xae, 0x3d, 0xff]);
                    }
                }
            }
        }
        slot.busy = true;
        let buffer = slot.buffer;
        sel.dirty = false;
        let surf = sel.surf;
        self.present(surf.surface, surf.viewport, surf.width, surf.height, buffer, None, true)?;
        self.selection.as_mut().unwrap().surf.frame_pending = true;
        Ok(())
    }

    fn finish_selection(&mut self, accept: bool) -> io::Result<()> {
        let Some(sel) = self.selection.take() else { return Ok(()) };
        let rect = Self::selection_rect(&sel);
        let (w, h) = (sel.surf.width as f64, sel.surf.height as f64);
        self.destroy_layer(sel.surf)?;
        self.selection_wait_seq = None;
        let region = rect
            .filter(|_| accept)
            .filter(|r| r.2 >= 4.0 && r.3 >= 4.0)
            .map(|(x, y, rw, rh)| RegionF { x: x / w, y: y / h, w: rw / w, h: rh / h });
        match region {
            Some(r) => eprintln!(
                "screenmirror: zoom region {:.3},{:.3} {:.3}x{:.3} (fractions of the source)",
                r.x, r.y, r.w, r.h
            ),
            None => eprintln!("screenmirror: region selection cancelled"),
        }
        self.shared.update(|st| {
            st.selecting = false;
            if let Some(r) = region {
                st.mode = Mode::Zoom(r);
            }
        });
        if let Some(t) = &self.tray {
            t.mode_changed();
        }
        self.draw_mirror()
    }

    /// Reacts to changes made by the tray / capture threads.
    fn sync_state(&mut self) -> io::Result<bool> {
        let (quit, selecting, showing, frame, repin) = {
            let mut st = self.shared.state.lock().unwrap();
            let repin = std::mem::take(&mut st.repin);
            (st.quit, st.selecting, st.showing, st.frame.clone(), repin)
        };
        if quit {
            return Ok(false);
        }
        if !showing {
            let had = self.mirror.is_some() || self.selection.is_some();
            self.destroy_mirror_window()?;
            if let Some(sel) = self.selection.take() {
                self.destroy_layer(sel.surf)?;
                self.selection_wait_seq = None;
            }
            if had {
                eprintln!("screenmirror: mirror view removed");
            }
            return Ok(true);
        }
        // A new xdg window is pinned to the virtual desktop that is current now.
        if repin && self.mirror.is_some() {
            eprintln!("screenmirror: pinning the mirror to the current virtual desktop");
            self.destroy_mirror_window()?;
        }
        if self.mirror.is_none() {
            if let Some(t) = self.target.clone() {
                eprintln!("screenmirror: mirroring onto {t} (current virtual desktop only)");
                self.mirror = Some(self.create_xdg(&t)?);
                self.drawn = Drawn::Nothing;
            }
        }
        if selecting && self.selection.is_none() {
            match self.selection_wait_seq {
                None => {
                    self.selection_wait_seq = Some(frame.as_ref().map_or(0, |f| f.seq));
                    self.shared.update(|st| st.want_frame = true);
                }
                Some(seq) => {
                    if let Some(f) = frame.filter(|f| f.seq > seq) {
                        self.start_selection(f)?;
                    }
                }
            }
        } else if !selecting && self.selection.is_some() {
            let sel = self.selection.take().unwrap();
            self.destroy_layer(sel.surf)?;
            self.selection_wait_seq = None;
        }
        self.draw_mirror()?;
        Ok(true)
    }

    pub fn run(mut self, wake: OwnedFd) -> io::Result<()> {
        self.reconfigure_outputs()?;
        let mut wake = std::fs::File::from(wake);
        loop {
            // Re-read the panel config even when outputs are unchanged, so dragging
            // the toolbar to the other screen flips source and target.
            self.reconfigure_outputs()?;
            let mut fds = [
                sys::PollFd { fd: self.conn.raw_fd(), events: sys::POLLIN, revents: 0 },
                sys::PollFd { fd: wake.as_raw_fd(), events: sys::POLLIN, revents: 0 },
            ];
            sys::poll(&mut fds, 500)?;
            if fds[0].revents & (sys::POLLIN | sys::POLLERR | sys::POLLHUP) != 0 {
                for ev in self.conn.read_events()? {
                    self.handle_event(ev)?;
                }
            }
            if fds[1].revents & sys::POLLIN != 0 {
                let mut buf = [0u8; 64];
                while matches!(wake.read(&mut buf), Ok(n) if n > 0) {}
                if !self.sync_state()? {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::toolbar_screen;

    const SAMPLE: &str = r#"
[Containments][1]
formfactor=0
lastScreen=0
plugin=org.kde.plasma.folder

[Containments][2]
formfactor=2
lastScreen=0
location=4
plugin=org.kde.panel

[Containments][2][Applets][3]
plugin=org.kde.plasma.kickoff

[Containments][2][Applets][5]
plugin=org.kde.plasma.icontasks

[Containments][2][Applets][7]
formfactor=0
lastScreen=-1
plugin=org.kde.plasma.systemtray

[Containments][2][Applets][7][Applets][10]
plugin=org.kde.kscreen

[Containments][22]
formfactor=0
lastScreen=1
plugin=org.kde.plasma.folder
"#;

    #[test]
    fn toolbar_is_panel_screen_not_desktop() {
        assert_eq!(toolbar_screen(SAMPLE), Some(0));
    }

    #[test]
    fn moving_the_panel_flips_the_screen() {
        // The panel's lastScreen is the second one; the desktop folder's comes first.
        let mut n = 0;
        let moved: String = SAMPLE
            .split_inclusive('\n')
            .map(|line| {
                if line.starts_with("lastScreen=") {
                    n += 1;
                    if n == 2 { "lastScreen=1\n" } else { line }
                } else {
                    line
                }
            })
            .collect();
        assert_eq!(toolbar_screen(&moved), Some(1));
    }

    #[test]
    fn a_second_panel_without_the_toolbar_does_not_win() {
        let extra = format!("{SAMPLE}\n[Containments][9]\nformfactor=2\nlastScreen=1\nplugin=org.kde.panel\n");
        assert_eq!(toolbar_screen(&extra), Some(0));
    }

    #[test]
    fn contain_letterboxes_the_panel_screen_onto_the_projector() {
        // Virtual-1 is 1920×1200 (16:10). Virtual-2 is 1024×768 (4:3).
        // scale = min(1024/1920, 768/1200) = 1024/1920, so the fitted height is 640
        // and the bars are (768-640)/2 = 64 px.
        assert_eq!(super::contain_rect(1920.0, 1200.0, 1024, 768), (0, 64, 1024, 640));
    }

    #[test]
    fn contain_pillarboxes_a_tall_zoom_region() {
        assert_eq!(super::contain_rect(100.0, 200.0, 1024, 768), (320, 0, 384, 768));
    }

    #[test]
    fn exchange_pair_swaps_two_distinct_outputs() {
        let v1 = Some("Virtual-1".into());
        let v2 = Some("Virtual-2".into());
        assert_eq!(super::exchange_pair(true, v1.clone(), v2.clone()), (v2.clone(), v1.clone()));
        assert_eq!(super::exchange_pair(false, v1.clone(), v2.clone()), (v1.clone(), v2.clone()));
        assert_eq!(super::exchange_pair(true, v1.clone(), None), (v1.clone(), None));
        assert_eq!(super::exchange_pair(true, None, v2.clone()), (None, v2.clone()));
        assert_eq!(super::exchange_pair(true, v1.clone(), v1.clone()), (v1.clone(), v1));
    }
}

/// Whole source visible, centered. Destination is never larger than the target, so nothing is cropped.
fn contain_rect(src_w: f64, src_h: f64, dst_w: i32, dst_h: i32) -> (i32, i32, i32, i32) {
    let dst_w = dst_w.max(1);
    let dst_h = dst_h.max(1);
    if src_w <= 0.0 || src_h <= 0.0 {
        return (0, 0, dst_w, dst_h);
    }
    let scale = (dst_w as f64 / src_w).min(dst_h as f64 / src_h);
    let w = ((src_w * scale).floor() as i32).clamp(1, dst_w);
    let h = ((src_h * scale).floor() as i32).clamp(1, dst_h);
    ((dst_w - w) / 2, (dst_h - h) / 2, w, h)
}

fn region_px(r: &RegionF, fw: u32, fh: u32) -> (f64, f64, f64, f64) {
    let (fw, fh) = (fw as f64, fh as f64);
    let x = (r.x * fw).clamp(0.0, fw - 1.0).floor();
    let y = (r.y * fh).clamp(0.0, fh - 1.0).floor();
    let w = (r.w * fw).round().clamp(1.0, fw - x);
    let h = (r.h * fh).round().clamp(1.0, fh - y);
    (x, y, w, h)
}
