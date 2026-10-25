//! Native Wayland display side.
//!
//! * The mirror is an `xdg_toplevel` fullscreen on the secondary output, so KWin
//!   puts it on the virtual desktop that is current when the window is created
//!   and hides it on every other desktop. Captured frames go into `wl_shm`
//!   buffers and `wp_viewporter` stretches them to the output size (aspect ratio
//!   deliberately ignored); in Zoom mode the viewport source rectangle crops the
//!   selected region, so scaling happens in the compositor. A layer-shell surface
//!   would be forced onto every desktop, which is why the mirror is not one.
//! * The zoom region selector is a layer surface on the source output that
//!   shows a dimmed freeze-frame; drag with the left button to select,
//!   Esc / right click cancels.
//! * Q unmaps the mirror when that window has keyboard focus, and leaves the process up.
//!   It is not a global shortcut, so Q still types everywhere else.

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
    Frame(u64, Mode),
}

#[derive(Default)]
struct Globals {
    compositor: u32,
    shm: u32,
    viewporter: u32,
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
                if key == KEY_ESC && self.selection.is_some() {
                    self.finish_selection(false)?;
                } else if key == KEY_Q && self.mirror.as_ref().is_some_and(|m| self.keyboard_focus == Some(m.surface)) {
                    eprintln!("screenmirror: Q on the focused mirror window");
                    self.stop_mirror()?;
                }
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

    fn reconfigure_outputs(&mut self) -> io::Result<()> {
        self.outputs_dirty = false;
        self.refresh_panel_screen();
        let (source, target) = self.choose_outputs();
        if source != self.source {
            if let Some(sel) = self.selection.take() {
                self.destroy_layer(sel.surf)?;
                self.selection_wait_seq = None;
                self.shared.update(|st| st.selecting = false);
            }
            let why = if env_output("SCREENMIRROR_SOURCE").is_some() {
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
        })
    }

    fn destroy_xdg(&mut self, w: XdgWin) -> io::Result<()> {
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
    fn slot_for(&mut self, width: u32, height: u32, stride: u32) -> io::Result<usize> {
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
        if let Some(i) = self.slots.iter().position(|s| !s.busy) {
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

    fn draw_mirror(&mut self) -> io::Result<()> {
        let (configured, frame_pending, surface, viewport, width, height) = {
            let Some(m) = self.mirror.as_ref() else { return Ok(()) };
            (m.configured, m.frame_pending, m.surface, m.viewport, m.width, m.height)
        };
        if !configured || frame_pending {
            return Ok(());
        }
        let (mode, frame) = {
            let st = self.shared.state.lock().unwrap();
            (st.mode, st.frame.clone())
        };
        let frame = frame.filter(|_| mode != Mode::Black);
        let want = match &frame {
            Some(f) => Drawn::Frame(f.seq, mode),
            None => Drawn::Black,
        };
        if want == self.drawn {
            return Ok(());
        }
        let surf = (surface, viewport, width, height);
        match frame {
            None => {
                if self.black.is_none() {
                    let mut s = self.new_slot(1, 1, 4)?;
                    s.map.as_mut_slice().copy_from_slice(&[0, 0, 0, 255]);
                    self.black = Some(s);
                }
                let buffer = self.black.as_ref().unwrap().buffer;
                self.present(surf.0, surf.1, surf.2, surf.3, buffer, None, false)?;
            }
            Some(f) => {
                let i = self.slot_for(f.width, f.height, f.stride)?;
                let slot = &mut self.slots[i];
                let n = slot.map.as_mut_slice().len().min(f.data.len());
                slot.map.as_mut_slice()[..n].copy_from_slice(&f.data[..n]);
                slot.busy = true;
                let buffer = slot.buffer;
                let src = match mode {
                    Mode::Zoom(r) => Some(region_px(&r, f.width, f.height)),
                    _ => None,
                };
                self.present(surf.0, surf.1, surf.2, surf.3, buffer, src, true)?;
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
        let i = self.slot_for(snap.width, snap.height, snap.stride)?;
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
        let (quit, selecting, showing, frame) = {
            let st = self.shared.state.lock().unwrap();
            (st.quit, st.selecting, st.showing, st.frame.clone())
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
}

fn region_px(r: &RegionF, fw: u32, fh: u32) -> (f64, f64, f64, f64) {
    let (fw, fh) = (fw as f64, fh as f64);
    let x = (r.x * fw).clamp(0.0, fw - 1.0).floor();
    let y = (r.y * fh).clamp(0.0, fh - 1.0).floor();
    let w = (r.w * fw).round().clamp(1.0, fw - x);
    let h = (r.h * fh).round().clamp(1.0, fh - y);
    (x, y, w, h)
}
