//! System tray icon: a StatusNotifierItem with a com.canonical.dbusmenu context menu,
//! served on the session bus by hand.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::dbus::{self, Connection, Message, Sender, Value};
use crate::icon;
use crate::state::{Mode, Shared};

const SNI_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const SNI_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";

const ID_FULL: i32 = 1;
const ID_ZOOM: i32 = 2;
const ID_BLACK: i32 = 3;
const ID_SEP: i32 = 4;
const ID_QUIT: i32 = 5;
const ID_NONE: i32 = 6;
const ID_DESKTOP: i32 = 7;
const ID_ASPECT: i32 = 8;
const ID_ABOUT: i32 = 9;

const MENU_IDS: [i32; 9] = [ID_FULL, ID_ZOOM, ID_BLACK, ID_ASPECT, ID_NONE, ID_DESKTOP, ID_SEP, ID_ABOUT, ID_QUIT];
/// Properties re-sent when the mode changes. Über is static; it is included because the list is explicit.
const UPDATED_IDS: [i32; 6] = [ID_FULL, ID_ZOOM, ID_BLACK, ID_ASPECT, ID_NONE, ID_ABOUT];

struct MenuSnap {
    mode: Mode,
    showing: bool,
    keep_aspect: bool,
}

fn menu_snap(shared: &Shared) -> MenuSnap {
    let st = shared.state.lock().unwrap();
    MenuSnap { mode: st.mode, showing: st.showing, keep_aspect: st.keep_aspect }
}

const KGLOBALACCEL: &str = "org.kde.kglobalaccel";
const KGLOBALACCEL_PATH: &str = "/kglobalaccel";
const KGLOBALACCEL_IFACE: &str = "org.kde.KGlobalAccel";

/// Drops a Q shortcut left behind by an older build. KGlobalAccel keeps that
/// grab after the process exits, so the letter q stops working system-wide.
fn release_legacy_global_q(conn: &mut Connection) {
    let action = Value::Array(
        "s".into(),
        ["screenmirror", "stop-mirror", "ScreenMirror", "Stop mirroring"]
            .into_iter()
            .map(Value::str)
            .collect(),
    );
    let clear = Message::method_call(
        KGLOBALACCEL,
        KGLOBALACCEL_PATH,
        KGLOBALACCEL_IFACE,
        "setShortcut",
        vec![action, Value::Array("i".into(), vec![]), Value::U32(2 | 4)],
    );
    if let Err(e) = conn.call(&clear) {
        eprintln!("screenmirror: could not clear a leftover global Q ({e})");
    }
    let unregister = Message::method_call(
        KGLOBALACCEL,
        KGLOBALACCEL_PATH,
        KGLOBALACCEL_IFACE,
        "unregister",
        vec![Value::str("screenmirror"), Value::str("stop-mirror")],
    );
    match conn.call(&unregister) {
        Ok(_) => eprintln!("screenmirror: global Q shortcut is not registered"),
        Err(e) => eprintln!("screenmirror: could not unregister global Q ({e})"),
    }
}

#[derive(Clone)]
pub struct TrayHandle {
    sender: Sender,
    revision: Arc<AtomicU32>,
    shared: Arc<Shared>,
}

impl TrayHandle {
    /// Tells the tray host that the menu checkmarks, icon and tooltip changed.
    pub fn mode_changed(&self) {
        let rev = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let snap = menu_snap(&self.shared);
        let _ = self.sender.send(&Message::signal(MENU_PATH, MENU_IFACE, "LayoutUpdated", vec![
            Value::U32(rev),
            Value::I32(0),
        ]));
        let updated: Vec<Value> = UPDATED_IDS
            .iter()
            .map(|&id| Value::Struct(vec![Value::I32(id), item_props(id, &snap)]))
            .collect();
        let _ = self.sender.send(&Message::signal(MENU_PATH, MENU_IFACE, "ItemsPropertiesUpdated", vec![
            Value::Array("(ia{sv})".into(), updated),
            Value::Array("(ias)".into(), vec![]),
        ]));
        for sig in ["NewIcon", "NewToolTip", "NewTitle"] {
            let _ = self.sender.send(&Message::signal(SNI_PATH, SNI_IFACE, sig, vec![]));
        }
    }
}

fn item_props(id: i32, snap: &MenuSnap) -> Value {
    let radio = |label: &str, on: bool| {
        Value::dict_sv(vec![
            ("label", Value::str(label)),
            ("toggle-type", Value::str("radio")),
            ("toggle-state", Value::I32(on as i32)),
        ])
    };
    let MenuSnap { mode, showing, keep_aspect } = snap;
    match id {
        0 => Value::dict_sv(vec![("children-display", Value::str("submenu"))]),
        ID_FULL => radio("Full screen", *showing && matches!(mode, Mode::Full)),
        ID_ZOOM => radio("Zoom – select region…", *showing && matches!(mode, Mode::Zoom(_))),
        ID_BLACK => radio("Black", *showing && matches!(mode, Mode::Black)),
        ID_ASPECT => Value::dict_sv(vec![
            ("label", Value::str("Seitenverhältnis behalten")),
            ("toggle-type", Value::str("checkmark")),
            ("toggle-state", Value::I32(*keep_aspect as i32)),
        ]),
        ID_NONE => radio("Keine Spiegelung", !*showing),
        ID_DESKTOP => Value::dict_sv(vec![("label", Value::str("Auf diese Arbeitsfläche"))]),
        ID_SEP => Value::dict_sv(vec![("type", Value::str("separator"))]),
        ID_ABOUT => Value::dict_sv(vec![("label", Value::str("Über"))]),
        ID_QUIT => Value::dict_sv(vec![("label", Value::str("Quit")), ("icon-name", Value::str("application-exit"))]),
        _ => Value::dict_sv(vec![]),
    }
}

fn layout(id: i32, depth: i32, snap: &MenuSnap) -> Value {
    let children = if id == 0 && depth != 0 {
        MENU_IDS.iter().map(|&c| Value::variant(layout(c, depth - 1, snap))).collect()
    } else {
        vec![]
    };
    Value::Struct(vec![Value::I32(id), item_props(id, snap), Value::Array("v".into(), children)])
}

fn tooltip_text(mode: &Mode, showing: bool, keep_aspect: bool, error: &Option<String>) -> String {
    if !showing {
        let base = "Not mirroring. Choose a mode to start again.".to_string();
        return match error {
            Some(e) => format!("{base}\nCapture error: {e}"),
            None => base,
        };
    }
    let base = match mode {
        Mode::Full => "Mirroring the whole screen".to_string(),
        Mode::Zoom(r) => format!(
            "Mirroring a region ({:.0}%×{:.0}% at {:.0}%,{:.0}%)",
            r.w * 100.0,
            r.h * 100.0,
            r.x * 100.0,
            r.y * 100.0
        ),
        Mode::Black => "Second screen blanked".to_string(),
    };
    let fit = match (mode, keep_aspect) {
        (Mode::Black, _) => String::new(),
        (_, true) => "\nAspect ratio kept; unused area is black.".to_string(),
        (_, false) => "\nStretched to fill the screen.".to_string(),
    };
    let base = format!(
        "{base}{fit}\nQ stops the mirror while its window is focused.\nShift+S swaps the two screens while this window is focused."
    );
    match error {
        Some(e) => format!("{base}\nCapture error: {e}"),
        None => base,
    }
}

fn sni_props(shared: &Shared) -> Vec<(&'static str, Value)> {
    let (mode, showing, keep_aspect, err) = {
        let st = shared.state.lock().unwrap();
        (st.mode, st.showing, st.keep_aspect, st.capture_error.clone())
    };
    let title = if showing {
        format!("ScreenMirror – {}", mode.name())
    } else {
        "ScreenMirror".to_string()
    };
    vec![
        ("Category", Value::str("Hardware")),
        ("Id", Value::str("screenmirror")),
        ("Title", Value::Str(title.clone())),
        ("Status", Value::str("Active")),
        ("WindowId", Value::I32(0)),
        ("IconName", Value::str("")),
        ("IconPixmap", icon::pixmaps(&mode, showing)),
        ("IconThemePath", Value::str("")),
        ("OverlayIconName", Value::str("")),
        ("OverlayIconPixmap", Value::Array("(iiay)".into(), vec![])),
        ("AttentionIconName", Value::str("")),
        ("AttentionIconPixmap", Value::Array("(iiay)".into(), vec![])),
        ("AttentionMovieName", Value::str("")),
        (
            "ToolTip",
            Value::Struct(vec![
                Value::str(""),
                Value::Array("(iiay)".into(), vec![]),
                Value::Str(title),
                Value::Str(tooltip_text(&mode, showing, keep_aspect, &err)),
            ]),
        ),
        ("ItemIsMenu", Value::Bool(true)),
        ("Menu", Value::Path(MENU_PATH.into())),
    ]
}

fn menu_props() -> Vec<(&'static str, Value)> {
    vec![
        ("Version", Value::U32(3)),
        ("TextDirection", Value::str("ltr")),
        ("Status", Value::str("normal")),
        ("IconThemePath", Value::Array("s".into(), vec![])),
    ]
}

const INTROSPECT_SNI: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
 <interface name="org.kde.StatusNotifierItem">
  <property name="Category" type="s" access="read"/>
  <property name="Id" type="s" access="read"/>
  <property name="Title" type="s" access="read"/>
  <property name="Status" type="s" access="read"/>
  <property name="WindowId" type="i" access="read"/>
  <property name="IconName" type="s" access="read"/>
  <property name="IconPixmap" type="a(iiay)" access="read"/>
  <property name="IconThemePath" type="s" access="read"/>
  <property name="OverlayIconName" type="s" access="read"/>
  <property name="OverlayIconPixmap" type="a(iiay)" access="read"/>
  <property name="AttentionIconName" type="s" access="read"/>
  <property name="AttentionIconPixmap" type="a(iiay)" access="read"/>
  <property name="AttentionMovieName" type="s" access="read"/>
  <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
  <property name="ItemIsMenu" type="b" access="read"/>
  <property name="Menu" type="o" access="read"/>
  <method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
  <method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
  <method name="SecondaryActivate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
  <method name="Scroll"><arg name="delta" type="i" direction="in"/><arg name="orientation" type="s" direction="in"/></method>
  <signal name="NewTitle"/><signal name="NewIcon"/><signal name="NewAttentionIcon"/><signal name="NewOverlayIcon"/><signal name="NewToolTip"/>
  <signal name="NewStatus"><arg name="status" type="s"/></signal>
 </interface>
 <interface name="org.freedesktop.DBus.Properties">
  <method name="Get"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method>
  <method name="GetAll"><arg type="s" direction="in"/><arg type="a{sv}" direction="out"/></method>
 </interface>
 <interface name="org.freedesktop.DBus.Introspectable">
  <method name="Introspect"><arg type="s" direction="out"/></method>
 </interface>
</node>"#;

const INTROSPECT_MENU: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
 <interface name="com.canonical.dbusmenu">
  <property name="Version" type="u" access="read"/>
  <property name="TextDirection" type="s" access="read"/>
  <property name="Status" type="s" access="read"/>
  <property name="IconThemePath" type="as" access="read"/>
  <method name="GetLayout"><arg type="i" name="parentId" direction="in"/><arg type="i" name="recursionDepth" direction="in"/><arg type="as" name="propertyNames" direction="in"/><arg type="u" name="revision" direction="out"/><arg type="(ia{sv}av)" name="layout" direction="out"/></method>
  <method name="GetGroupProperties"><arg type="ai" name="ids" direction="in"/><arg type="as" name="propertyNames" direction="in"/><arg type="a(ia{sv})" name="properties" direction="out"/></method>
  <method name="GetProperty"><arg type="i" name="id" direction="in"/><arg type="s" name="name" direction="in"/><arg type="v" name="value" direction="out"/></method>
  <method name="Event"><arg type="i" name="id" direction="in"/><arg type="s" name="eventId" direction="in"/><arg type="v" name="data" direction="in"/><arg type="u" name="timestamp" direction="in"/></method>
  <method name="EventGroup"><arg type="a(isvu)" name="events" direction="in"/><arg type="ai" name="idErrors" direction="out"/></method>
  <method name="AboutToShow"><arg type="i" name="id" direction="in"/><arg type="b" name="needUpdate" direction="out"/></method>
  <method name="AboutToShowGroup"><arg type="ai" name="ids" direction="in"/><arg type="ai" name="updatesNeeded" direction="out"/><arg type="ai" name="idErrors" direction="out"/></method>
  <signal name="ItemsPropertiesUpdated"><arg type="a(ia{sv})" name="updatedProps"/><arg type="a(ias)" name="removedProps"/></signal>
  <signal name="LayoutUpdated"><arg type="u" name="revision"/><arg type="i" name="parent"/></signal>
  <signal name="ItemActivationRequested"><arg type="i" name="id"/><arg type="u" name="timestamp"/></signal>
 </interface>
 <interface name="org.freedesktop.DBus.Properties">
  <method name="Get"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method>
  <method name="GetAll"><arg type="s" direction="in"/><arg type="a{sv}" direction="out"/></method>
 </interface>
</node>"#;

const INTROSPECT_ROOT: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node><node name="StatusNotifierItem"/><node name="MenuBar"/></node>"#;

struct Tray {
    conn: Connection,
    handle: TrayHandle,
    service: String,
}

impl Tray {
    fn register_with_watcher(&mut self) {
        let msg = Message::method_call(WATCHER, "/StatusNotifierWatcher", WATCHER, "RegisterStatusNotifierItem", vec![
            Value::Str(self.service.clone()),
        ]);
        if let Err(e) = self.handle.sender.send(&msg) {
            eprintln!("screenmirror: tray: cannot register: {e}");
        }
    }

    fn props_for(&self, path: &str, iface: &str) -> Option<Vec<(&'static str, Value)>> {
        match (path, iface) {
            (SNI_PATH, SNI_IFACE) => Some(sni_props(&self.handle.shared)),
            (MENU_PATH, MENU_IFACE) => Some(menu_props()),
            _ => None,
        }
    }

    fn on_menu_event(&mut self, id: i32, event: &str) {
        if event != "clicked" {
            return;
        }
        let shared = &self.handle.shared;
        match id {
            ID_FULL => shared.update(|st| {
                st.mode = Mode::Full;
                st.selecting = false;
                st.showing = true;
            }),
            ID_ZOOM => shared.update(|st| {
                st.selecting = true;
                st.showing = true;
            }),
            ID_BLACK => shared.update(|st| {
                st.mode = Mode::Black;
                st.selecting = false;
                st.showing = true;
            }),
            ID_ASPECT => {
                let on = {
                    let mut st = shared.state.lock().unwrap();
                    st.keep_aspect = !st.keep_aspect;
                    st.keep_aspect
                };
                shared.wake();
                eprintln!("screenmirror: keep aspect ratio: {on}");
            }
            ID_NONE => {
                shared.update(|st| {
                    st.showing = false;
                    st.selecting = false;
                });
                eprintln!("screenmirror: mirror removed from the menu");
            }
            ID_DESKTOP => {
                let showing = shared.state.lock().unwrap().showing;
                if showing {
                    shared.update(|st| st.repin = true);
                    eprintln!("screenmirror: move mirror onto the current virtual desktop");
                } else {
                    shared.update(|st| {
                        st.mode = Mode::Full;
                        st.selecting = false;
                        st.showing = true;
                    });
                    eprintln!("screenmirror: start Full on the current virtual desktop");
                }
            }
            ID_ABOUT => {
                shared.update(|st| st.show_about = true);
                eprintln!("screenmirror: about window requested");
            }
            ID_QUIT => shared.update(|st| st.quit = true),
            _ => return,
        }
        eprintln!("screenmirror: menu: item {id} clicked");
        self.handle.mode_changed();
    }

    fn handle_call(&mut self, m: &Message) -> Message {
        let path = m.path.as_deref().unwrap_or("");
        let iface = m.interface.as_deref().unwrap_or("");
        let member = m.member.as_deref().unwrap_or("");
        let arg_str = |i: usize| m.body.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let arg_i = |i: usize| m.body.get(i).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let snap = menu_snap(&self.handle.shared);
        match (iface, member) {
            ("org.freedesktop.DBus.Properties", "GetAll") => match self.props_for(path, &arg_str(0)) {
                Some(p) => m.reply(vec![Value::dict_sv(p)]),
                None => m.reply(vec![Value::dict_sv(vec![])]),
            },
            ("org.freedesktop.DBus.Properties", "Get") => {
                let name = arg_str(1);
                match self.props_for(path, &arg_str(0)).and_then(|p| p.into_iter().find(|(k, _)| *k == name)) {
                    Some((_, v)) => m.reply(vec![Value::variant(v)]),
                    None => m.error("org.freedesktop.DBus.Error.UnknownProperty", &format!("no property {name}")),
                }
            }
            ("org.freedesktop.DBus.Properties", "Set") => {
                m.error("org.freedesktop.DBus.Error.PropertyReadOnly", "properties are read-only")
            }
            ("org.freedesktop.DBus.Introspectable", "Introspect") => m.reply(vec![Value::str(match path {
                SNI_PATH => INTROSPECT_SNI,
                MENU_PATH => INTROSPECT_MENU,
                _ => INTROSPECT_ROOT,
            })]),
            ("org.freedesktop.DBus.Peer", "Ping") => m.reply(vec![]),
            ("org.freedesktop.DBus.Peer", "GetMachineId") => {
                let id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
                m.reply(vec![Value::Str(id.trim().to_string())])
            }
            (SNI_IFACE, "Activate" | "SecondaryActivate" | "ContextMenu" | "Scroll") => m.reply(vec![]),
            (MENU_IFACE, "GetLayout") => {
                let rev = self.handle.revision.load(Ordering::SeqCst);
                m.reply(vec![Value::U32(rev), layout(arg_i(0), arg_i(1), &snap)])
            }
            (MENU_IFACE, "GetGroupProperties") => {
                let mut ids: Vec<i32> = m
                    .body
                    .first()
                    .and_then(|v| v.as_array())
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|v| v.as_i64().map(|x| x as i32))
                    .collect();
                if ids.is_empty() {
                    ids = std::iter::once(0).chain(MENU_IDS).collect();
                }
                let items = ids.iter().map(|&id| Value::Struct(vec![Value::I32(id), item_props(id, &snap)])).collect();
                m.reply(vec![Value::Array("(ia{sv})".into(), items)])
            }
            (MENU_IFACE, "GetProperty") => {
                let name = arg_str(1);
                match item_props(arg_i(0), &snap).dict_get(&name) {
                    Some(v) => m.reply(vec![Value::variant(v.clone())]),
                    None => m.error("com.canonical.dbusmenu.Error.UnknownProperty", &name),
                }
            }
            (MENU_IFACE, "Event") => {
                self.on_menu_event(arg_i(0), &arg_str(1));
                m.reply(vec![])
            }
            (MENU_IFACE, "EventGroup") => {
                let events: Vec<(i32, String)> = m
                    .body
                    .first()
                    .and_then(|v| v.as_array())
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|e| match e {
                        Value::Struct(f) => Some((
                            f.first()?.as_i64()? as i32,
                            f.get(1)?.as_str()?.to_string(),
                        )),
                        _ => None,
                    })
                    .collect();
                for (id, ev) in events {
                    self.on_menu_event(id, &ev);
                }
                m.reply(vec![Value::Array("i".into(), vec![])])
            }
            (MENU_IFACE, "AboutToShow") => m.reply(vec![Value::Bool(false)]),
            (MENU_IFACE, "AboutToShowGroup") => {
                m.reply(vec![Value::Array("i".into(), vec![]), Value::Array("i".into(), vec![])])
            }
            _ => m.error("org.freedesktop.DBus.Error.UnknownMethod", &format!("unknown method {iface}.{member}")),
        }
    }

    fn run(mut self) {
        loop {
            let m = match self.conn.next() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("screenmirror: tray: bus connection lost: {e}");
                    return;
                }
            };
            match m.msg_type {
                dbus::METHOD_CALL => {
                    let reply = self.handle_call(&m);
                    if m.flags & dbus::FLAG_NO_REPLY == 0 {
                        let _ = self.handle.sender.send(&reply);
                    }
                }
                dbus::SIGNAL if m.is("org.freedesktop.DBus", "NameOwnerChanged") => {
                    let name = m.body.first().and_then(|v| v.as_str()).unwrap_or("");
                    let new_owner = m.body.get(2).and_then(|v| v.as_str()).unwrap_or("");
                    if name == WATCHER && !new_owner.is_empty() {
                        eprintln!("screenmirror: tray: StatusNotifierWatcher appeared, registering");
                        self.register_with_watcher();
                    }
                }
                dbus::ERROR => eprintln!("screenmirror: tray: {}", m.error_text()),
                _ => {}
            }
        }
    }
}

/// Connects to the session bus, exports the tray icon and menu, and serves them on a thread.
pub fn start(shared: Arc<Shared>) -> io::Result<TrayHandle> {
    let mut conn = Connection::session()?;
    release_legacy_global_q(&mut conn);
    let service = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    conn.bus_call("RequestName", vec![Value::Str(service.clone()), Value::U32(4)])?;
    conn.bus_call("AddMatch", vec![Value::Str(format!(
        "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='{WATCHER}'"
    ))])?;
    let handle = TrayHandle { sender: conn.sender.clone(), revision: Arc::new(AtomicU32::new(1)), shared };
    let mut tray = Tray { conn, handle: handle.clone(), service };
    tray.register_with_watcher();
    std::thread::Builder::new().name("tray".into()).spawn(move || tray.run())?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_is_labeled_ueber_and_listed_with_the_other_items() {
        assert_eq!(
            MENU_IDS,
            [ID_FULL, ID_ZOOM, ID_BLACK, ID_ASPECT, ID_NONE, ID_DESKTOP, ID_SEP, ID_ABOUT, ID_QUIT]
        );
        assert!(UPDATED_IDS.contains(&ID_ABOUT));
        let snap = MenuSnap { mode: Mode::Full, showing: true, keep_aspect: true };
        let label = |id| item_props(id, &snap).dict_get("label").and_then(|v| v.as_str()).map(str::to_string);
        assert_eq!(label(ID_FULL).as_deref(), Some("Full screen"));
        assert_eq!(label(ID_ZOOM).as_deref(), Some("Zoom – select region…"));
        assert_eq!(label(ID_BLACK).as_deref(), Some("Black"));
        assert_eq!(label(ID_ASPECT).as_deref(), Some("Seitenverhältnis behalten"));
        assert_eq!(label(ID_NONE).as_deref(), Some("Keine Spiegelung"));
        assert_eq!(label(ID_DESKTOP).as_deref(), Some("Auf diese Arbeitsfläche"));
        assert_eq!(label(ID_ABOUT).as_deref(), Some("Über"));
        assert_eq!(label(ID_QUIT).as_deref(), Some("Quit"));
        let root = layout(0, 1, &snap);
        let Value::Struct(fields) = root else { panic!("layout root") };
        let children = fields[2].as_array().unwrap();
        let ids: Vec<i32> = children
            .iter()
            .map(|v| match v {
                Value::Variant(inner) => match &**inner {
                    Value::Struct(f) => f[0].as_i64().unwrap() as i32,
                    _ => panic!("child"),
                },
                _ => panic!("variant"),
            })
            .collect();
        assert_eq!(ids, MENU_IDS.to_vec());
    }
}
