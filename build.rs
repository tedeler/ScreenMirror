//! Compiles the About-window identity into the binary.
//!
//! `SCREENMIRROR_GIT_ID`, when set and non-empty, wins over `git rev-parse HEAD`.
//! `SOURCE_DATE_EPOCH`, when set, is the build date (UTC `YYYY-MM-DD`).
//! Neither git nor the clock failing is fatal: the window shows `unbekannt`.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SCREENMIRROR_GIT_ID");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    watch_git(&manifest);

    let git = git_id(&manifest);
    let date = build_date();
    // Known civil dates, so a broken converter fails the build instead of shipping a wrong Baudatum.
    assert_eq!(format_unix_utc_date(0).as_deref(), Some("1970-01-01"));
    assert_eq!(format_unix_utc_date(86_400).as_deref(), Some("1970-01-02"));
    assert_eq!(format_unix_utc_date(951_782_400).as_deref(), Some("2000-02-29"));
    assert_eq!(format_unix_utc_date(1_700_000_000).as_deref(), Some("2023-11-14"));
    assert_eq!(format_unix_utc_date(1_792_972_800).as_deref(), Some("2026-10-26"));

    println!("cargo:rustc-env=SCREENMIRROR_GIT_ID={git}");
    println!("cargo:rustc-env=SCREENMIRROR_BUILD_DATE={date}");
    eprintln!("screenmirror: Git-ID {git} Baudatum {date}");
}

/// Rebuild when the checked-out commit changes. `.git/HEAD` is only a symbolic
/// ref, so the branch file (or `packed-refs`) is what a commit actually updates.
fn watch_git(manifest_dir: &str) {
    let git_meta = Path::new(manifest_dir).join(".git");
    let git_dir = if git_meta.is_dir() {
        git_meta
    } else if git_meta.is_file() {
        let Ok(text) = std::fs::read_to_string(&git_meta) else { return };
        let Some(dir) = text.trim().strip_prefix("gitdir: ") else { return };
        Path::new(dir).to_path_buf()
    } else {
        return;
    };
    let head = git_dir.join("HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
    }
    let Ok(text) = std::fs::read_to_string(&head) else { return };
    if let Some(r) = text.trim().strip_prefix("ref: ") {
        let ref_file = git_dir.join(r);
        if ref_file.exists() {
            println!("cargo:rerun-if-changed={}", ref_file.display());
        }
    }
    let packed = git_dir.join("packed-refs");
    if packed.exists() {
        println!("cargo:rerun-if-changed={}", packed.display());
    }
}

fn git_id(manifest_dir: &str) -> String {
    if let Some(id) = env_token("SCREENMIRROR_GIT_ID") {
        return id;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .args(["rev-parse", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output();
    match output {
        Ok(out) if out.status.success() => token(&String::from_utf8_lossy(&out.stdout)).unwrap_or_else(|| "unbekannt".into()),
        _ => "unbekannt".into(),
    }
}

fn build_date() -> String {
    if let Some(raw) = env_raw("SOURCE_DATE_EPOCH") {
        return raw.parse::<u64>().ok().and_then(format_unix_utc_date).unwrap_or_else(|| "unbekannt".into());
    }
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format_unix_utc_date(d.as_secs()).unwrap_or_else(|| "unbekannt".into()),
        Err(_) => "unbekannt".into(),
    }
}

/// Howard Hinnant's civil-from-days, for a Unix timestamp. `None` if the arithmetic does not fit.
fn format_unix_utc_date(secs: u64) -> Option<String> {
    let days = i64::try_from(secs / 86_400).ok()?;
    let z = days.checked_add(719_468)?;
    let era = z / 146_097;
    let doe = u64::try_from(z - era * 146_097).ok()?;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = i64::try_from(yoe).ok()?.checked_add(era.checked_mul(400)?)?;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y = y.checked_add(1)?;
    }
    if !(1..=12).contains(&m) || d == 0 || d > 31 {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

fn env_raw(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn env_token(key: &str) -> Option<String> {
    env_raw(key).and_then(|s| token(&s))
}

/// First whitespace-delimited token, restricted to printable ASCII, capped so a huge value
/// cannot blow up the About window. Empty after that means "not set".
fn token(s: &str) -> Option<String> {
    let t: String = s.split_whitespace().next().unwrap_or("").chars().filter(|c| c.is_ascii_graphic()).take(64).collect();
    if t.is_empty() { None } else { Some(t) }
}
