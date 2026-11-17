//! GitHub release check for the tray.
//!
//! std has no TLS, so the request goes through the system `curl`. The menu
//! item shows the outcome; a newer release can be saved from that same item.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const REPO: &str = "tedeler/ScreenMirror";
const LATEST_URL: &str = "https://api.github.com/repos/tedeler/ScreenMirror/releases/latest";

/// What the tray menu and tooltip say about updates.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateNote {
    Idle,
    Checking,
    /// Running version is at least the newest published release.
    Current {
        version: String,
    },
    /// A newer release exists and `url` is the asset to save.
    Ready {
        version: String,
        url: String,
        filename: String,
    },
    Downloading {
        version: String,
    },
    Saved {
        path: String,
    },
    Failed {
        message: String,
    },
}

pub fn menu_label(note: &UpdateNote) -> String {
    match note {
        UpdateNote::Idle => "Check for updates".to_string(),
        UpdateNote::Checking => "Checking for updates…".to_string(),
        UpdateNote::Current { version } => format!("Up to date ({version})"),
        UpdateNote::Ready { version, .. } => format!("Download {version}"),
        UpdateNote::Downloading { version } => format!("Downloading {version}…"),
        UpdateNote::Saved { path } => {
            let name = Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(path);
            format!("Saved {name}")
        }
        UpdateNote::Failed { .. } => "Update check failed".to_string(),
    }
}

pub fn tooltip_line(note: &UpdateNote) -> String {
    match note {
        UpdateNote::Idle => "Check for updates looks on GitHub for a newer release.".to_string(),
        UpdateNote::Checking => "Checking GitHub for a newer release…".to_string(),
        UpdateNote::Current { version } => format!("No newer GitHub release than {version}."),
        UpdateNote::Ready {
            version, filename, ..
        } => {
            format!("Release {version} is available. Download saves {filename}.")
        }
        UpdateNote::Downloading { version } => format!("Downloading release {version}…"),
        UpdateNote::Saved { path } => format!("Saved the release to {path}"),
        UpdateNote::Failed { message } => format!("Update check failed: {message}"),
    }
}

/// Asks GitHub for the latest release and compares it with `current` (`Cargo.toml` version).
pub fn check(current: &str) -> UpdateNote {
    match query(current) {
        Ok(note) => note,
        Err(message) => UpdateNote::Failed { message },
    }
}

/// Saves the asset from a `Ready` note into the user's Downloads directory.
pub fn download(url: &str, filename: &str) -> UpdateNote {
    match save_asset(url, filename) {
        Ok(path) => UpdateNote::Saved {
            path: path.display().to_string(),
        },
        Err(message) => UpdateNote::Failed { message },
    }
}

fn query(current: &str) -> Result<UpdateNote, String> {
    let body = curl_text(LATEST_URL, Duration::from_secs(20))?;
    let release = parse_release(&body)?;
    let latest = parse_version(&release.tag)
        .ok_or_else(|| format!("cannot read version {}", release.tag))?;
    let running = parse_version(current).ok_or_else(|| format!("cannot read version {current}"))?;
    let version = display_version(&release.tag);
    if !is_newer(&latest, &running) {
        return Ok(UpdateNote::Current {
            version: current.to_string(),
        });
    }
    let asset = pick_asset(&release.assets)
        .ok_or_else(|| "the release has no package to download".to_string())?;
    if !allowed_download_url(&asset.url) {
        return Err("the release download URL is not from this project".into());
    }
    Ok(UpdateNote::Ready {
        version,
        url: asset.url.clone(),
        filename: asset.name.clone(),
    })
}

fn save_asset(url: &str, filename: &str) -> Result<PathBuf, String> {
    if !allowed_download_url(url) {
        return Err("refusing a download outside this project's releases".into());
    }
    let name =
        safe_filename(filename).ok_or_else(|| "the release file name is not usable".to_string())?;
    let dir = downloads_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let dest = dir.join(&name);
    let partial = dir.join(format!("{name}.part"));
    let _ = std::fs::remove_file(&partial);
    curl_to_file(url, &partial, Duration::from_secs(180))?;
    std::fs::rename(&partial, &dest).map_err(|e| format!("cannot save {}: {e}", dest.display()))?;
    Ok(dest)
}

fn user_agent() -> String {
    format!("ScreenMirror/{}", env!("CARGO_PKG_VERSION"))
}

fn curl_text(url: &str, timeout: Duration) -> Result<String, String> {
    // GitHub sometimes answers the first request with 403 and the next with the release.
    match curl_text_once(url, timeout) {
        Ok(body) => Ok(body),
        Err(_) => {
            std::thread::sleep(Duration::from_millis(400));
            curl_text_once(url, timeout)
        }
    }
}

fn curl_text_once(url: &str, timeout: Duration) -> Result<String, String> {
    let mut args = curl_base(timeout);
    if let Some(flag) = args.iter_mut().find(|a| a.as_str() == "--fail") {
        *flag = "--fail-with-body".into();
    }
    args.push("-H".into());
    args.push("Accept: application/vnd.github+json".into());
    args.push(url.to_string());
    let output = run_curl(&args)?;
    if !output.status.success() {
        let body = String::from_utf8_lossy(&output.stdout);
        if let Some(message) = top_string(&body, "message") {
            return Err(shorten(&message));
        }
        return Err(curl_failure(&output));
    }
    if output.stdout.len() > 2_000_000 {
        return Err("the release response was too large".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "the release response was not text".into())
}

fn curl_to_file(url: &str, dest: &Path, timeout: Duration) -> Result<(), String> {
    let mut args = curl_base(timeout);
    args.push("--output".into());
    args.push(dest.display().to_string());
    args.push(url.to_string());
    let output = run_curl(&args)?;
    if output.status.success() {
        Ok(())
    } else {
        let _ = std::fs::remove_file(dest);
        Err(curl_failure(&output))
    }
}

fn curl_base(timeout: Duration) -> Vec<String> {
    vec![
        "--proto".into(),
        "=https".into(),
        "--tlsv1.2".into(),
        "--fail".into(),
        "--silent".into(),
        "--show-error".into(),
        "--location".into(),
        "--max-time".into(),
        timeout.as_secs().max(1).to_string(),
        "-A".into(),
        user_agent(),
    ]
}

fn run_curl(args: &[String]) -> Result<std::process::Output, String> {
    Command::new("curl").args(args).output().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            "curl is not installed".to_string()
        } else {
            format!("could not run curl ({e})")
        }
    })
}

fn curl_failure(output: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&output.stderr);
    let err = err.trim();
    if err.is_empty() {
        "GitHub did not respond".to_string()
    } else {
        shorten(err)
    }
}

fn shorten(s: &str) -> String {
    let one = s.lines().next().unwrap_or(s);
    if one.chars().count() > 160 {
        let end = one
            .char_indices()
            .nth(157)
            .map(|(i, _)| i)
            .unwrap_or(one.len());
        format!("{}…", &one[..end])
    } else {
        one.to_string()
    }
}

struct Release {
    tag: String,
    assets: Vec<Asset>,
}

struct Asset {
    name: String,
    url: String,
}

fn parse_release(json: &str) -> Result<Release, String> {
    let tag = top_string(json, "tag_name").ok_or_else(|| {
        top_string(json, "message").unwrap_or_else(|| "the release response had no version".into())
    })?;
    let assets_json = top_raw(json, "assets").unwrap_or("[]");
    if !assets_json.starts_with('[') {
        return Err("the release response had no downloads".into());
    }
    let mut assets = Vec::new();
    for object in json_objects(assets_json) {
        let Some(name) = top_string(object, "name") else {
            continue;
        };
        let Some(url) = top_string(object, "browser_download_url") else {
            continue;
        };
        assets.push(Asset { name, url });
    }
    Ok(Release { tag, assets })
}

fn pick_asset(assets: &[Asset]) -> Option<&Asset> {
    let deb = match std::env::consts::ARCH {
        "x86_64" => "_amd64.deb",
        "aarch64" => "_arm64.deb",
        _ => ".deb",
    };
    assets
        .iter()
        .find(|a| a.name.ends_with(deb))
        .or_else(|| assets.iter().find(|a| a.name.ends_with(".deb")))
        .or_else(|| {
            assets
                .iter()
                .find(|a| a.name.contains("linux") && a.name.ends_with(".tar.gz"))
        })
}

fn download_prefix() -> String {
    format!("https://github.com/{REPO}/releases/download/")
}

fn allowed_download_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix(&download_prefix()) else {
        return false;
    };
    !rest.is_empty()
        && !rest.contains("..")
        && !rest.contains('\\')
        && rest
            .chars()
            .all(|c| c.is_ascii() && !c.is_whitespace() && c != '"' && c != '\'')
}

fn safe_filename(name: &str) -> Option<String> {
    if name.is_empty() || name.starts_with('.') || name.contains("..") {
        return None;
    }
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
    {
        Some(name.to_string())
    } else {
        None
    }
}

fn downloads_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home = PathBuf::from(&home);
        let dirs = home.join(".config/user-dirs.dirs");
        if let Ok(text) = std::fs::read_to_string(dirs) {
            for line in text.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix("XDG_DOWNLOAD_DIR=") else {
                    continue;
                };
                let rest = rest
                    .trim()
                    .trim_matches('"')
                    .replace("$HOME", &home.to_string_lossy());
                if !rest.is_empty() {
                    return PathBuf::from(rest);
                }
            }
        }
        return home.join("Downloads");
    }
    std::env::temp_dir()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    parts: Vec<u64>,
}

fn parse_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().trim_start_matches(['v', 'V']);
    if raw.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for piece in raw.split('.') {
        if piece.is_empty() || !piece.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts.push(piece.parse().ok()?);
    }
    Some(Version { parts })
}

fn display_version(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// True when `latest` is a strictly newer dotted version than `running`.
fn is_newer(latest: &Version, running: &Version) -> bool {
    let n = latest.parts.len().max(running.parts.len());
    for i in 0..n {
        let a = latest.parts.get(i).copied().unwrap_or(0);
        let b = running.parts.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

struct Scan<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Scan<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn skip_ws(&mut self) {
        self.i += self
            .rest()
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(|c| c.len_utf8())
            .sum::<usize>();
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_ws();
        if self.rest().starts_with(c) {
            self.i += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Option<String> {
        self.skip_ws();
        if !self.rest().starts_with('"') {
            return None;
        }
        let (value, n) = parse_json_string(self.rest())?;
        self.i += n;
        Some(value)
    }

    fn skip_value(&mut self) -> bool {
        self.skip_ws();
        let r = self.rest();
        if r.starts_with('"') {
            return self.string().is_some();
        }
        if r.starts_with('{') {
            let object = slice_container(r, '{', '}').unwrap_or("");
            if object.is_empty() {
                return false;
            }
            self.i += object.len();
            return true;
        }
        if r.starts_with('[') {
            let array = slice_container(r, '[', ']').unwrap_or("");
            if array.is_empty() {
                return false;
            }
            self.i += array.len();
            return true;
        }
        let mut n = r.len();
        if let Some(i) = r.find([',', '}', ']']) {
            n = n.min(i);
        }
        if let Some(i) = r.find(char::is_whitespace) {
            n = n.min(i);
        }
        if n == 0 {
            return false;
        }
        self.i += n;
        true
    }
}

/// First top-level string field. Nested objects (an uploader's `"name"`) are skipped.
fn top_string(json: &str, key: &str) -> Option<String> {
    let mut sc = Scan { s: json, i: 0 };
    if !sc.eat('{') {
        return None;
    }
    loop {
        sc.skip_ws();
        if sc.rest().starts_with('}') {
            return None;
        }
        let found = sc.string()?;
        if !sc.eat(':') {
            return None;
        }
        if found == key {
            return sc.string();
        }
        if !sc.skip_value() {
            return None;
        }
        sc.skip_ws();
        if sc.eat(',') {
            continue;
        }
        return None;
    }
}

fn top_raw<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let mut sc = Scan { s: json, i: 0 };
    if !sc.eat('{') {
        return None;
    }
    loop {
        sc.skip_ws();
        if sc.rest().starts_with('}') {
            return None;
        }
        let found = sc.string()?;
        if !sc.eat(':') {
            return None;
        }
        if found == key {
            sc.skip_ws();
            let r = sc.rest();
            if r.starts_with('[') {
                return slice_container(r, '[', ']');
            }
            if r.starts_with('{') {
                return slice_container(r, '{', '}');
            }
            return None;
        }
        if !sc.skip_value() {
            return None;
        }
        sc.skip_ws();
        if sc.eat(',') {
            continue;
        }
        return None;
    }
}

/// Top-level objects of a JSON array. Braces inside strings are ignored.
fn json_objects(array: &str) -> Vec<&str> {
    let bytes = array.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    let mut obj_start = None;
    let mut out = Vec::new();
    while i < bytes.len() {
        if in_str {
            if escape {
                escape = false;
            } else if bytes[i] == b'\\' {
                escape = true;
            } else if bytes[i] == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match bytes[i] {
            b'"' => in_str = true,
            b'[' | b'{' => {
                if bytes[i] == b'{' && depth == 1 {
                    obj_start = Some(i);
                }
                depth += 1;
            }
            b']' | b'}' => {
                if bytes[i] == b'}' && depth == 2 {
                    if let Some(start) = obj_start.take() {
                        out.push(&array[start..=i]);
                    }
                }
                depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn slice_container(s: &str, open: char, close: char) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first().copied()? != open as u8 {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b if b == open as u8 => depth += 1,
            b if b == close as u8 => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_json_string(input: &str) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    if bytes.first().copied()? != b'"' {
        return None;
    }
    let mut out = String::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                i += 1;
                match *bytes.get(i)? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        if i + 4 >= bytes.len() {
                            return None;
                        }
                        let hex = std::str::from_utf8(&bytes[i + 1..i + 5]).ok()?;
                        let cp = u32::from_str_radix(hex, 16).ok()?;
                        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        i += 4;
                    }
                    _ => return None,
                }
                i += 1;
            }
            b => {
                let ch = input[i..].chars().next()?;
                out.push(ch);
                i += ch.len_utf8();
                let _ = b;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
    {
      "message_unused": "no",
      "tag_name": "v0.0.8",
      "uploader": {"name": "not-the-asset", "browser_download_url": "https://example.invalid/nope"},
      "assets": [
        {
          "uploader": {"name": "someone"},
          "name": "screenmirror-v0.0.8-x86_64-unknown-linux-gnu.tar.gz",
          "browser_download_url": "https://github.com/tedeler/ScreenMirror/releases/download/v0.0.8/screenmirror-v0.0.8-x86_64-unknown-linux-gnu.tar.gz"
        },
        {
          "name": "screenmirror_0.0.8_amd64.deb",
          "browser_download_url": "https://github.com/tedeler/ScreenMirror/releases/download/v0.0.8/screenmirror_0.0.8_amd64.deb",
          "uploader": {"name": "tedeler", "browser_download_url": "https://example.invalid/nested"}
        }
      ]
    }
    "#;

    #[test]
    fn a_newer_release_offers_the_deb() {
        let note = parse_release(FIXTURE).and_then(|release| {
            let latest = parse_version(&release.tag).unwrap();
            let running = parse_version("0.0.7").unwrap();
            assert!(is_newer(&latest, &running));
            let asset = pick_asset(&release.assets).unwrap();
            assert!(allowed_download_url(&asset.url));
            Ok(UpdateNote::Ready {
                version: display_version(&release.tag),
                url: asset.url.clone(),
                filename: asset.name.clone(),
            })
        });
        match note.unwrap() {
            UpdateNote::Ready {
                version,
                url,
                filename,
            } => {
                assert_eq!(version, "0.0.8");
                assert_eq!(filename, "screenmirror_0.0.8_amd64.deb");
                assert!(url.ends_with("/screenmirror_0.0.8_amd64.deb"));
                assert_eq!(
                    menu_label(&UpdateNote::Ready {
                        version,
                        url,
                        filename: filename.clone()
                    }),
                    "Download 0.0.8"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_same_version_is_current() {
        let release = parse_release(
            &FIXTURE
                .replace("v0.0.8", "v0.0.7")
                .replace("0.0.8", "0.0.7"),
        )
        .unwrap();
        let latest = parse_version(&release.tag).unwrap();
        let running = parse_version("0.0.7").unwrap();
        assert!(!is_newer(&latest, &running));
        let note = UpdateNote::Current {
            version: "0.0.7".into(),
        };
        assert_eq!(menu_label(&note), "Up to date (0.0.7)");
        assert!(tooltip_line(&note).contains("0.0.7"));
    }

    #[test]
    fn an_older_release_is_not_offered() {
        assert!(!is_newer(
            &parse_version("v0.0.6").unwrap(),
            &parse_version("0.0.7").unwrap()
        ));
        assert!(is_newer(
            &parse_version("0.0.10").unwrap(),
            &parse_version("0.0.9").unwrap()
        ));
        assert!(is_newer(
            &parse_version("v0.1.0").unwrap(),
            &parse_version("0.0.7").unwrap()
        ));
        assert!(!is_newer(
            &parse_version("0.0.7").unwrap(),
            &parse_version("0.0.7").unwrap()
        ));
        assert!(parse_version("v0.0.7-rc1").is_none());
    }

    #[test]
    fn download_urls_stay_on_this_repos_releases() {
        assert!(allowed_download_url(
            "https://github.com/tedeler/ScreenMirror/releases/download/v0.0.8/screenmirror_0.0.8_amd64.deb"
        ));
        assert!(!allowed_download_url(
            "http://github.com/tedeler/ScreenMirror/releases/download/v0.0.8/a.deb"
        ));
        assert!(!allowed_download_url(
            "https://github.com/other/ScreenMirror/releases/download/v0.0.8/a.deb"
        ));
        assert!(!allowed_download_url(
            "https://github.com/tedeler/ScreenMirror/releases/download/../secret"
        ));
        assert!(safe_filename("screenmirror_0.0.8_amd64.deb").is_some());
        assert!(safe_filename("../etc/passwd").is_none());
        assert!(safe_filename("a b.deb").is_none());
    }

    #[test]
    fn curl_is_https_only() {
        let args = curl_base(Duration::from_secs(20));
        assert_eq!(&args[..2], ["--proto", "=https"]);
        assert!(args.iter().any(|a| a == "--tlsv1.2"));
        assert!(args.iter().any(|a| a.starts_with("ScreenMirror/")));
    }

    #[test]
    fn labels_cover_each_step() {
        assert_eq!(menu_label(&UpdateNote::Idle), "Check for updates");
        assert_eq!(menu_label(&UpdateNote::Checking), "Checking for updates…");
        assert_eq!(
            menu_label(&UpdateNote::Downloading {
                version: "0.0.8".into()
            }),
            "Downloading 0.0.8…"
        );
        assert_eq!(
            menu_label(&UpdateNote::Saved {
                path: "/home/me/Downloads/screenmirror_0.0.8_amd64.deb".into()
            }),
            "Saved screenmirror_0.0.8_amd64.deb"
        );
        assert_eq!(
            menu_label(&UpdateNote::Failed {
                message: "nope".into()
            }),
            "Update check failed"
        );
        assert!(tooltip_line(&UpdateNote::Failed {
            message: "curl is not installed".into()
        })
        .contains("curl is not installed"));
    }

    #[test]
    fn github_error_object_becomes_a_message() {
        let err = parse_release(
            r#"{"message":"Not Found","documentation_url":"https://docs.github.com"}"#,
        );
        match err {
            Err(message) => assert!(message.contains("Not Found"), "{message}"),
            Ok(_) => panic!("an error object is not a release"),
        }
    }

    #[test]
    fn live_github_release_is_not_newer_than_this_build() {
        if std::env::var_os("SCREENMIRROR_LIVE_UPDATE").is_none() {
            return;
        }
        let note = check(env!("CARGO_PKG_VERSION"));
        match note {
            UpdateNote::Current { version } => assert_eq!(version, env!("CARGO_PKG_VERSION")),
            other => panic!("expected the published release to be current, got {other:?}"),
        }
    }

    #[test]
    fn live_download_saves_the_published_deb() {
        if std::env::var_os("SCREENMIRROR_LIVE_UPDATE").is_none() {
            return;
        }
        let home = std::env::temp_dir().join(format!("screenmirror-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        let note = download(
            "https://github.com/tedeler/ScreenMirror/releases/download/v0.0.7/screenmirror_0.0.7_amd64.deb",
            "screenmirror_0.0.7_amd64.deb",
        );
        match note {
            UpdateNote::Saved { path } => {
                let len = std::fs::metadata(&path).unwrap().len();
                assert!(len > 10_000, "{path} is {len} bytes");
                assert!(
                    path.ends_with("Downloads/screenmirror_0.0.7_amd64.deb"),
                    "{path}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn escaped_url_round_trips() {
        let (s, n) = parse_json_string(r#""https:\/\/github.com\/tedeler\/ScreenMirror""#).unwrap();
        assert_eq!(s, "https://github.com/tedeler/ScreenMirror");
        assert_eq!(n, r#""https:\/\/github.com\/tedeler\/ScreenMirror""#.len());
    }
}
