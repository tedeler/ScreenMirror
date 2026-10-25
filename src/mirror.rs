//! Native Wayland display side.
//!
//! * The mirror is a wlr-layer-shell surface on the second output (overlay layer,
//!   anchored to all edges, ignoring panels' exclusive zones). Captured frames go
//!   into wl_shm buffers and `wp_viewporter` stretches them to the output size
//!   (aspect ratio deliberately ignored); in Zoom mode the viewport source
//!   rectangle crops the selected region, so scaling happens in the compositor.
//! * The zoom region selector is a second layer surface on the source output that
//!   shows a dimmed freeze-frame; drag with the left button to select,
//!   Esc / right click cancels.

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
    cursor_device: u32,
    slots: Vec<Slot>,
    black: Option<Slot>,
    mirror: Option<LayerSurf>,
    drawn: Drawn,
    selection: Option<Selection>,
    selection_wait_seq: Option<u64>,
    source: Option<String>,
    target: Option<String>,
    sync_done: bool,
}

fn env_output(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

fn is_internal(name: &str) -> bool {
    ["eDP", "LVDS", "DSI"].iter().any(|p| name.starts_with(p))
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
            cursor_device: 0,
            slots: Vec::new(),
            black: None,
            mirror: None,
            drawn: Drawn::Nothing,
            selection: None,
            selection_wait_seq: None,
            source: None,
            target: None,
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
            (Obj::Keyboard, 3) => {
                let (_serial, _time, key, state) = (a.u32(), a.u32(), a.u32(), a.u32());
                if key == KEY_ESC && state == 1 && self.selection.is_some() {
                    self.finish_selection(false)?;
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
                if let Some(m) = self.mirror.as_mut().filter(|m| m.surface == surface) {
                    m.configured = true;
                    if (w, h) != (m.width, m.height) {
                        self.drawn = Drawn::Nothing;
                    }
                    (m.width, m.height) = (w.max(1), h.max(1));
                    self.draw_mirror()?;
                } else if let Some(s) = self.selection.as_mut().filter(|s| s.surf.surface == surface) {
                    s.surf.configured = true;
                    (s.surf.width, s.surf.height) = (w.max(1), h.max(1));
                    s.dirty = true;
                    self.draw_selection()?;
                }
            }
            (Obj::LayerSurface(surface), 1) => {
                if self.mirror.as_ref().is_some_and(|m| m.surface == surface) {
                    let m = self.mirror.take().unwrap();
                    self.destroy_layer(m)?;
                    self.target = None;
                    self.outputs_dirty = true;
                } else if self.selection.as_ref().is_some_and(|s| s.surf.surface == surface) {
                    self.finish_selection(false)?;
                }
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

    /// Picks the source (laptop panel) and target (second screen) outputs.
    pub fn choose_outputs(&self) -> (Option<String>, Option<String>) {
        let ready: Vec<&OutputInfo> = self.outputs.iter().filter(|o| o.done && !o.name.is_empty()).collect();
        let ordered: Vec<&OutputInfo> = {
            let mut v: Vec<&OutputInfo> =
                self.output_order.iter().filter_map(|n| ready.iter().find(|o| &o.name == n).copied()).collect();
            v.extend(ready.iter().filter(|o| !self.output_order.contains(&o.name)).copied());
            v
        };
        let source = env_output("SCREENMIRROR_SOURCE")
            .filter(|n| ready.iter().any(|o| &o.name == n))
            .or_else(|| ordered.iter().find(|o| is_internal(&o.name)).map(|o| o.name.clone()))
            .or_else(|| ordered.first().map(|o| o.name.clone()));
        let target = env_output("SCREENMIRROR_TARGET")
            .filter(|n| ready.iter().any(|o| &o.name == n) && Some(n) != source.as_ref())
            .or_else(|| ordered.iter().find(|o| Some(&o.name) != source.as_ref()).map(|o| o.name.clone()));
        (source, target)
    }

    fn reconfigure_outputs(&mut self) -> io::Result<()> {
        self.outputs_dirty = false;
        let (source, target) = self.choose_outputs();
        if source != self.source {
            eprintln!("screenmirror: source screen: {}", source.as_deref().unwrap_or("(none)"));
            self.source = source.clone();
            self.shared.update(|st| {
                st.source = source;
                st.frame = None;
                st.want_frame = true;
            });
            self.drawn = Drawn::Nothing;
        }
        if target != self.target || (target.is_some() && self.mirror.is_none()) {
            if let Some(m) = self.mirror.take() {
                self.destroy_layer(m)?;
            }
            match &target {
                Some(t) => {
                    eprintln!("screenmirror: mirroring onto: {t}");
                    let t = t.clone();
                    self.mirror = Some(self.create_layer(&t, "screenmirror", false)?);
                }
                None => eprintln!("screenmirror: no second screen connected; waiting"),
            }
            self.target = target;
            self.drawn = Drawn::Nothing;
        }
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

    fn present(&mut self, surf: &LayerSurf, buffer: u32, src: Option<(f64, f64, f64, f64)>, callback: bool) -> io::Result<()> {
        let (x, y, w, h) = src.unwrap_or((-1.0, -1.0, -1.0, -1.0));
        self.conn.send(Req::new(surf.viewport, 1).fixed(x).fixed(y).fixed(w).fixed(h))?;
        self.conn.send(Req::new(surf.viewport, 2).i32(surf.width).i32(surf.height))?;
        if callback {
            let cb = self.conn.new_id();
            self.objects.insert(cb, Obj::FrameCallback(surf.surface));
            self.conn.send(Req::new(surf.surface, 3).u32(cb))?;
        }
        self.conn.send(Req::new(surf.surface, 1).u32(buffer).i32(0).i32(0))?;
        self.conn.send(Req::new(surf.surface, 9).i32(0).i32(0).i32(i32::MAX).i32(i32::MAX))?;
        self.conn.send(Req::new(surf.surface, 6))
    }

    fn draw_mirror(&mut self) -> io::Result<()> {
        let Some(m) = self.mirror.as_ref() else { return Ok(()) };
        if !m.configured || m.frame_pending {
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
        let surf = *m;
        match frame {
            None => {
                if self.black.is_none() {
                    let mut s = self.new_slot(1, 1, 4)?;
                    s.map.as_mut_slice().copy_from_slice(&[0, 0, 0, 255]);
                    self.black = Some(s);
                }
                let buffer = self.black.as_ref().unwrap().buffer;
                self.present(&surf, buffer, None, false)?;
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
                self.present(&surf, buffer, src, true)?;
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
        self.present(&surf, buffer, None, true)?;
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
        let (quit, selecting, frame) = {
            let st = self.shared.state.lock().unwrap();
            (st.quit, st.selecting, st.frame.clone())
        };
        if quit {
            return Ok(false);
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
            if self.outputs_dirty {
                self.reconfigure_outputs()?;
            }
            let mut fds = [
                sys::PollFd { fd: self.conn.raw_fd(), events: sys::POLLIN, revents: 0 },
                sys::PollFd { fd: wake.as_raw_fd(), events: sys::POLLIN, revents: 0 },
            ];
            sys::poll(&mut fds, -1)?;
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

fn region_px(r: &RegionF, fw: u32, fh: u32) -> (f64, f64, f64, f64) {
    let (fw, fh) = (fw as f64, fh as f64);
    let x = (r.x * fw).clamp(0.0, fw - 1.0).floor();
    let y = (r.y * fh).clamp(0.0, fh - 1.0).floor();
    let w = (r.w * fw).round().clamp(1.0, fw - x);
    let h = (r.h * fh).round().clamp(1.0, fh - y);
    (x, y, w, h)
}
