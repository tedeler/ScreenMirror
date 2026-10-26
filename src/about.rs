//! Über dialog.
//!
//! The four facts live here. A separate Qt process (`screenmirror-about`)
//! shows them in a normal dialog. The mirror keeps running while it is open,
//! and a second click does not open another one.

use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

pub const WINDOW_TITLE: &str = "Über ScreenMirror";

pub fn lines() -> [&'static str; 4] {
    [
        "Autor: Cursor + Edeler",
        concat!("Version: ", env!("CARGO_PKG_VERSION")),
        concat!("Git-ID: ", env!("SCREENMIRROR_GIT_ID")),
        concat!("Baudatum: ", env!("SCREENMIRROR_BUILD_DATE")),
    ]
}

/// Arguments after the helper path: title, author, version, git id, build date.
pub fn arguments() -> [&'static str; 5] {
    let lines = lines();
    [
        WINDOW_TITLE,
        fact(lines[0]),
        fact(lines[1]),
        fact(lines[2]),
        fact(lines[3]),
    ]
}

fn fact(line: &'static str) -> &'static str {
    line.split_once(": ").map(|(_, value)| value).unwrap_or(line)
}

fn helper_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("SCREENMIRROR_ABOUT_HELPER").filter(|p| !p.is_empty()) {
        paths.push(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("screenmirror-about"));
        }
    }
    paths.push(PathBuf::from(env!("SCREENMIRROR_ABOUT_HELPER")));
    paths.push(PathBuf::from("/usr/libexec/screenmirror-about"));
    paths
}

pub fn helper_path() -> Option<PathBuf> {
    helper_candidates().into_iter().find(|path| path.is_file())
}

fn dialog_slot() -> &'static Mutex<Option<Child>> {
    static DIALOG: Mutex<Option<Child>> = Mutex::new(None);
    &DIALOG
}

fn still_running(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// Collects a helper that has already exited.
pub fn reap() {
    let Ok(mut slot) = dialog_slot().lock() else {
        return;
    };
    if slot.as_mut().is_some_and(|child| !still_running(child)) {
        *slot = None;
    }
}

/// Starts the dialog, or does nothing when one is already open.
pub fn open() {
    let Ok(mut slot) = dialog_slot().lock() else {
        return;
    };
    if let Some(child) = slot.as_mut() {
        if still_running(child) {
            eprintln!("screenmirror: about dialog already open");
            return;
        }
        *slot = None;
    }
    match spawn() {
        Ok(child) => {
            eprintln!("screenmirror: about dialog started");
            *slot = Some(child);
        }
        Err(e) => eprintln!("screenmirror: could not open the about dialog ({e})"),
    }
}

/// Stops the helper when ScreenMirror itself quits.
pub fn dismiss() {
    let Ok(mut slot) = dialog_slot().lock() else {
        return;
    };
    if let Some(mut child) = slot.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn spawn() -> io::Result<Child> {
    let path = helper_path().ok_or_else(|| {
        io::Error::new(ErrorKind::NotFound, "screenmirror-about is not installed")
    })?;
    eprintln!("screenmirror: about helper {}", path.display());
    Command::new(path)
        .args(arguments())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_use_the_german_labels_and_compiled_values() {
        let lines = lines();
        assert_eq!(lines[0], "Autor: Cursor + Edeler");
        assert_eq!(lines[1], format!("Version: {}", env!("CARGO_PKG_VERSION")));
        assert_eq!(lines[2], format!("Git-ID: {}", env!("SCREENMIRROR_GIT_ID")));
        assert_eq!(lines[3], format!("Baudatum: {}", env!("SCREENMIRROR_BUILD_DATE")));
        let git = fact(lines[2]);
        let date = fact(lines[3]);
        assert!(!git.is_empty() && !git.contains(char::is_whitespace));
        assert!(
            date == "unbekannt"
                || (date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-')
        );
    }

    #[test]
    fn arguments_are_the_title_and_the_four_facts() {
        let args = arguments();
        assert_eq!(args[0], "Über ScreenMirror");
        assert_eq!(args[1], "Cursor + Edeler");
        assert_eq!(args[2], env!("CARGO_PKG_VERSION"));
        assert_eq!(args[3], fact(lines()[2]));
        assert_eq!(args[4], fact(lines()[3]));
    }

    #[test]
    fn helper_binary_is_on_disk() {
        let path = helper_path().expect("screenmirror-about");
        assert!(path.ends_with("screenmirror-about"), "{}", path.display());
    }

    #[test]
    fn a_live_child_blocks_a_second_dialog() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(still_running(&mut child));
        let _ = child.kill();
        let _ = child.wait();
        assert!(!still_running(&mut child));
    }
}
