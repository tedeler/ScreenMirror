//! ScreenMirror: mirrors the laptop screen onto the second screen on KDE Plasma (Wayland).
//! std only; talks D-Bus and the Wayland wire protocol directly.

mod capture;
mod dbus;
mod icon;
mod mirror;
mod state;
mod sys;
mod tray;
mod wayland;

use std::io::{self, Write};
use std::path::PathBuf;

const USAGE: &str = "\
ScreenMirror – mirror the screen that has the Plasma panel onto the other screen

Usage:
  screenmirror [--fps N] [--no-cursor] [--no-mirror]
                                         run (tray: Full / Zoom / Black,
                                         Seitenverhältnis behalten,
                                         Keine Spiegelung, Auf diese Arbeitsfläche;
                                         Q stops the mirror and Shift+S swaps the two screens
                                         only while the mirror window is focused)
  screenmirror --install [--autostart]   write the .desktop file KWin needs to
                                         authorize screen capture (and autostart)
  screenmirror --list-outputs            show screens and which ones are used
  screenmirror --screenshot OUTPUT FILE  save one capture of OUTPUT as PPM

Environment:
  SCREENMIRROR_SOURCE=NAME   screen to mirror (default: the one holding the Plasma panel)
  SCREENMIRROR_TARGET=NAME   screen to show the mirror on (default: the other screen)
";

fn data_home() -> io::Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    Ok(PathBuf::from(home).join(".local/share"))
}

fn config_home() -> io::Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    Ok(PathBuf::from(home).join(".config"))
}

fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=ScreenMirror\n\
         Comment=Mirror the laptop screen onto the second screen\n\
         Exec={exe}\n\
         Icon=video-display\n\
         Terminal=false\n\
         Categories=Utility;\n\
         X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2\n"
    )
}

fn install(autostart: bool) -> io::Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let exe = exe.to_string_lossy().into_owned();
    if exe.contains(char::is_whitespace) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "executable path must not contain spaces"));
    }
    let entry = desktop_entry(&exe);
    let apps = data_home()?.join("applications");
    std::fs::create_dir_all(&apps)?;
    let path = apps.join("screenmirror.desktop");
    std::fs::write(&path, &entry)?;
    println!("wrote {}", path.display());
    if autostart {
        let dir = config_home()?.join("autostart");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("screenmirror.desktop");
        std::fs::write(&path, &entry)?;
        println!("wrote {}", path.display());
    }
    // KWin looks the executable up through KSycoca; refresh it if the tool is around.
    for tool in ["kbuildsycoca6", "kbuildsycoca5"] {
        if let Ok(status) = std::process::Command::new(tool).arg("--noincremental").status() {
            if status.success() {
                println!("refreshed the KDE service cache ({tool})");
                break;
            }
        }
    }
    Ok(())
}

fn list_outputs() -> io::Result<()> {
    let (shared, _wake) = state::Shared::new(true)?;
    let app = mirror::App::connect(shared, None)?;
    for o in &app.outputs {
        println!(
            "{:<12} {}x{} scale {}  {}",
            o.name, o.width, o.height, o.scale, o.description
        );
    }
    let (source, target) = app.choose_outputs();
    println!("source: {}", source.as_deref().unwrap_or("(none)"));
    println!("target: {}", target.as_deref().unwrap_or("(none)"));
    Ok(())
}

fn screenshot(output: &str, file: &str) -> io::Result<()> {
    let mut c = capture::Capturer::new()?;
    let f = c.capture_screen(output)?;
    let mut out = Vec::with_capacity(f.width as usize * f.height as usize * 3 + 32);
    write!(out, "P6\n{} {}\n255\n", f.width, f.height)?;
    for row in f.data.chunks(f.stride as usize).take(f.height as usize) {
        for px in row[..f.width as usize * 4].chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    std::fs::write(file, out)?;
    println!("{output}: {}x{} -> {file}", f.width, f.height);
    Ok(())
}

fn run(fps: u32, cursor: bool, showing: bool) -> io::Result<()> {
    let (shared, wake) = state::Shared::new(showing)?;
    let tray = match tray::start(shared.clone()) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("screenmirror: tray icon unavailable: {e}");
            None
        }
    };
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("capture".into()).spawn(move || {
            capture::run(shared, fps, cursor);
        })?;
    }
    let app = mirror::App::connect(shared.clone(), tray)?;
    app.run(wake)?;
    shared.update(|st| st.quit = true);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut fps = 30;
    let mut cursor = true;
    let mut showing = true;
    let mut i = 0;
    let result = loop {
        match args.get(i).map(String::as_str) {
            None => break run(fps, cursor, showing),
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return;
            }
            Some("--version") => {
                println!("screenmirror {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some("--install") => break install(args.iter().any(|a| a == "--autostart")),
            Some("--list-outputs") => break list_outputs(),
            Some("--screenshot") => match (args.get(i + 1), args.get(i + 2)) {
                (Some(o), Some(f)) => break screenshot(o, f),
                _ => {
                    eprint!("{USAGE}");
                    std::process::exit(2);
                }
            },
            Some("--fps") => {
                fps = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(30);
                i += 2;
            }
            Some("--no-cursor") => {
                cursor = false;
                i += 1;
            }
            Some("--no-mirror") => {
                showing = false;
                i += 1;
            }
            Some(other) => {
                eprintln!("unknown argument {other}\n");
                eprint!("{USAGE}");
                std::process::exit(2);
            }
        }
    };
    if let Err(e) = result {
        eprintln!("screenmirror: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}
