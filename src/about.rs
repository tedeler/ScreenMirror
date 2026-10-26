//! Über window: the four identity lines, drawn with the system sans.
//!
//! The face is fontconfig's `sans` (Plasma uses Noto Sans), read from the font
//! file when the window opens. Nothing is bundled. A missing font draws a
//! notice in the same window and does not stop the mirror.

#[path = "stroke.rs"]
mod stroke;
#[path = "ttf.rs"]
mod ttf;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use ttf::Font;

const TITLE: &str = "ScreenMirror";
const BODY_PX: f32 = 17.0;
const TITLE_PX: f32 = 24.0;
const TITLE_PX_SOFT: f32 = 28.0;
const PAD_X: f32 = 40.0;
const ACCENT_H: u32 = 4;

/// XRGB8888, little-endian byte order B, G, R, X.
const PAPER: [u8; 4] = [0xFD, 0xFC, 0xFB, 0xFF];
const TITLE_INK: [u8; 4] = [0x1A, 0x17, 0x14, 0xFF];
const BODY_INK: [u8; 4] = [0x33, 0x2F, 0x2C, 0xFF];
const ACCENT: [u8; 4] = [0xE9, 0xAE, 0x3D, 0xFF];
const BORDER: [u8; 4] = [0xDC, 0xD4, 0xD0, 0xFF];
const DIVIDER: [u8; 4] = [0xED, 0xE8, 0xE6, 0xFF];

const REGULAR_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/croscore/Arimo-Regular.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
];

struct Face {
    regular: Font,
    bold: Option<Font>,
    path: String,
}

pub fn lines() -> [&'static str; 4] {
    [
        "Autor: Cursor + Edeler",
        concat!("Version: ", env!("CARGO_PKG_VERSION")),
        concat!("Git-ID: ", env!("SCREENMIRROR_GIT_ID")),
        concat!("Baudatum: ", env!("SCREENMIRROR_BUILD_DATE")),
    ]
}

/// Preferred toplevel size in logical pixels (buffer scale 1).
pub fn content_size() -> (u32, u32) {
    match installed() {
        Some(face) => face_metrics(face).size(),
        None => notice_size(),
    }
}

/// Path of the sans file used for the body, when one was found.
#[cfg(test)]
pub fn typeface_path() -> Option<String> {
    installed().map(|f| f.path.clone())
}

/// Renders the dialog into an XRGB8888 buffer of `width`×`height`.
/// Text is aligned to the left margin. Pixels outside the buffer are clipped.
pub fn render(width: u32, height: u32) -> Vec<u8> {
    let Some(len) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
    else {
        return Vec::new();
    };
    if len > 48 * 1024 * 1024 {
        return Vec::new();
    }
    let mut buf = vec![0u8; len];
    if width == 0 || height == 0 {
        return buf;
    }
    paint_paper(&mut buf, width, height);
    match installed() {
        Some(face) => paint_face(&mut buf, width, height, face),
        None => paint_notice(&mut buf, width, height),
    }
    buf
}

fn installed() -> Option<&'static Face> {
    static SLOT: OnceLock<Option<Face>> = OnceLock::new();
    SLOT.get_or_init(load_face).as_ref()
}

fn load_face() -> Option<Face> {
    let Some((path, regular)) = find_regular() else {
        eprintln!("screenmirror: no system sans font; the about window shows a notice");
        return None;
    };
    let bold_file = find_bold(&path);
    if let Some((bold_path, _)) = &bold_file {
        eprintln!("screenmirror: about title face {}", bold_path.display());
    }
    let face = Face {
        regular,
        bold: bold_file.map(|(_, font)| font),
        path: path.display().to_string(),
    };
    eprintln!("screenmirror: about typeface {}", face.path);
    Some(face)
}

fn find_regular() -> Option<(PathBuf, Font)> {
    let mut tried = Vec::new();
    if let Some(p) = fc_file("sans") {
        tried.push(p);
    }
    for c in REGULAR_CANDIDATES {
        tried.push(PathBuf::from(c));
    }
    for path in tried {
        if let Some(font) = read_font(&path) {
            return Some((path, font));
        }
    }
    walk_sans()
}

fn find_bold(regular: &Path) -> Option<(PathBuf, Font)> {
    let mut tried = Vec::new();
    if let Some(p) = fc_file("sans:weight=bold") {
        tried.push(p);
    }
    if let Some(p) = sibling_bold(regular) {
        tried.push(p);
    }
    for path in tried {
        if path == regular {
            continue;
        }
        if let Some(font) = read_font(&path) {
            return Some((path, font));
        }
    }
    None
}

fn sibling_bold(regular: &Path) -> Option<PathBuf> {
    let name = regular.file_name()?.to_str()?;
    let dir = regular.parent()?;
    let stem = name.strip_suffix(".ttf").unwrap_or(name);
    let mut names = Vec::new();
    if stem.contains("Regular") {
        names.push(format!("{}.ttf", stem.replace("Regular", "Bold")));
    }
    if stem.contains("regular") {
        names.push(format!("{}.ttf", stem.replace("regular", "bold")));
    }
    names.push(format!("{stem}-Bold.ttf"));
    for n in names {
        if n == name {
            continue;
        }
        let path = dir.join(n);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn fc_file(pattern: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let path = PathBuf::from(text.lines().next()?.trim());
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

fn read_font(path: &Path) -> Option<Font> {
    let data = std::fs::read(path).ok()?;
    Font::parse(data)
}

fn walk_sans() -> Option<(PathBuf, Font)> {
    let mut found = Vec::new();
    for root in ["/usr/share/fonts", "/usr/local/share/fonts"] {
        score_tree(Path::new(root), 0, &mut found);
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in found.into_iter().take(12) {
        if let Some(font) = read_font(&path) {
            return Some((path, font));
        }
    }
    None
}

fn score_tree(dir: &Path, depth: u8, found: &mut Vec<(i32, PathBuf)>) {
    if depth > 5 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            score_tree(&path, depth + 1, found);
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let score = sans_score(name);
        if score >= 0 {
            found.push((score, path));
        }
    }
}

fn sans_score(name: &str) -> i32 {
    let n = name.to_ascii_lowercase();
    if !(n.ends_with(".ttf") || n.ends_with(".ttc") || n.ends_with(".otf")) {
        return -1;
    }
    if n.contains("mono") || n.contains("serif") || n.contains("emoji") || n.contains("display") {
        return -1;
    }
    if n.contains("bold")
        || n.contains("italic")
        || n.contains("black")
        || n.contains("light")
        || n.contains("thin")
        || n.contains("medium")
        || n.contains("condensed")
    {
        return -1;
    }
    if !n.contains("sans") {
        return -1;
    }
    let mut score = 10;
    if n.contains("notosans-regular") {
        score += 80;
    } else if n.contains("dejavusans.ttf") {
        score += 60;
    } else if n.contains("liberationsans-regular") {
        score += 60;
    }
    if n.contains("regular") {
        score += 15;
    }
    score
}

struct Metrics {
    width: u32,
    height: u32,
    title_px: f32,
    title_baseline: f32,
    divider_y: i32,
    body_baseline: f32,
    body_step: f32,
}

impl Metrics {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

fn face_metrics(face: &Face) -> Metrics {
    let title_px = if face.bold.is_some() {
        TITLE_PX
    } else {
        TITLE_PX_SOFT
    };
    let title_font = face.bold.as_ref().unwrap_or(&face.regular);
    let title_w = title_font.measure(TITLE, title_px);
    let body_w = lines()
        .iter()
        .map(|l| face.regular.measure(l, BODY_PX))
        .fold(0.0_f32, f32::max);
    let width = (title_w.max(body_w) + PAD_X * 2.0 + 12.0).ceil().max(480.0) as u32;
    let title_lh = title_font.line_height(title_px);
    let body_lh = face.regular.line_height(BODY_PX);
    let mut y = ACCENT_H as f32 + 32.0;
    let title_baseline = y + title_font.ascent(title_px);
    y += title_lh + 16.0;
    let divider_y = y.round() as i32;
    y += 1.0 + 18.0;
    let body_baseline = y + face.regular.ascent(BODY_PX);
    y += body_lh * 4.0;
    let height = (y + 32.0).ceil() as u32;
    Metrics {
        width,
        height,
        title_px,
        title_baseline,
        divider_y,
        body_baseline,
        body_step: body_lh,
    }
}

fn paint_face(buf: &mut [u8], width: u32, height: u32, face: &Face) {
    let m = face_metrics(face);
    let pad = margin(width);
    let title_font = face.bold.as_ref().unwrap_or(&face.regular);
    title_font.draw_line(
        buf,
        width,
        height,
        TITLE,
        pad,
        m.title_baseline,
        m.title_px,
        TITLE_INK,
    );
    fill_rect(
        buf,
        width,
        height,
        pad as i32,
        m.divider_y,
        (width as f32 - pad * 2.0).max(0.0) as u32,
        1,
        DIVIDER,
    );
    let max_w = (width as f32 - pad * 2.0).max(8.0);
    let mut baseline = m.body_baseline;
    for line in lines() {
        for part in wrap(&face.regular, line, BODY_PX, max_w) {
            face.regular
                .draw_line(buf, width, height, &part, pad, baseline, BODY_PX, BODY_INK);
            baseline += m.body_step;
        }
    }
}

fn margin(width: u32) -> f32 {
    if width as f32 > PAD_X * 2.0 + 80.0 {
        PAD_X
    } else {
        16.0
    }
}

fn wrap(font: &Font, text: &str, px: f32, max_w: f32) -> Vec<String> {
    if text.is_empty() || font.measure(text, px) <= max_w {
        return vec![text.to_string()];
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < text.len() && lines.len() < 6 {
        let mut last_fit = start;
        let mut last_break = None;
        for (i, &(idx, ch)) in chars.iter().enumerate() {
            if idx < start {
                continue;
            }
            let next = chars.get(i + 1).map(|c| c.0).unwrap_or(text.len());
            if font.measure(&text[start..next], px) <= max_w {
                last_fit = next;
                if ch == ' ' {
                    last_break = Some(idx);
                }
            } else {
                break;
            }
        }
        let mut cut = if last_fit >= text.len() {
            text.len()
        } else if let Some(br) = last_break.filter(|b| *b > start) {
            br
        } else if last_fit > start {
            last_fit
        } else {
            chars
                .iter()
                .map(|(i, _)| *i)
                .find(|i| *i > start)
                .unwrap_or(text.len())
        };
        if cut <= start {
            cut = chars
                .iter()
                .map(|(i, _)| *i)
                .find(|i| *i > start)
                .unwrap_or(text.len());
        }
        lines.push(text[start..cut].trim_end().to_string());
        start = cut;
        while start < text.len() && text.as_bytes()[start] == b' ' {
            start += 1;
        }
    }
    lines
}

fn notice_lines() -> Vec<String> {
    let mut lines = Vec::with_capacity(6);
    lines.push("Systemschrift nicht gefunden.".into());
    lines.push(String::new());
    lines.extend(self::lines().map(str::to_string));
    lines
}

fn notice_size() -> (u32, u32) {
    let px = 18.0;
    let widest = notice_lines()
        .iter()
        .map(|l| stroke_width(l, px))
        .fold(0.0_f32, f32::max);
    let width = (widest + 36.0 * 2.0 + 12.0).ceil().max(520.0) as u32;
    let height = (4.0 + 28.0 + px * 1.55 * notice_lines().len() as f32 + 30.0).ceil() as u32;
    (width, height)
}

fn paint_notice(buf: &mut [u8], width: u32, height: u32) {
    let px = 18.0;
    let pad = margin(width).min(36.0);
    let step = px * 1.55;
    let mut y = ACCENT_H as f32 + 28.0;
    for line in notice_lines() {
        draw_strokes(buf, width, height, &line, pad, y, px, BODY_INK);
        y += step;
    }
}

fn stroke_width(text: &str, px: f32) -> f32 {
    let scale = px / 14.0;
    text.chars().map(|ch| stroke::advance(ch) * scale).sum()
}

fn draw_strokes(
    buf: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
    x: f32,
    y: f32,
    px: f32,
    color: [u8; 4],
) {
    let scale = px / 14.0;
    let radius = px * 0.062;
    let mut pen = x;
    for ch in text.chars() {
        for &(x0, y0, x1, y1) in stroke::strokes(ch) {
            stroke(
                buf,
                width,
                height,
                pen + x0 * scale,
                y + y0 * scale,
                pen + x1 * scale,
                y + y1 * scale,
                radius,
                color,
            );
        }
        pen += stroke::advance(ch) * scale;
    }
}

fn stroke(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    color: [u8; 4],
) {
    let edge = 0.85;
    let minx = (x0.min(x1) - radius - edge - 1.0).floor() as i32;
    let maxx = (x0.max(x1) + radius + edge + 1.0).ceil() as i32;
    let miny = (y0.min(y1) - radius - edge - 1.0).floor() as i32;
    let maxy = (y0.max(y1) + radius + edge + 1.0).ceil() as i32;
    let vx = x1 - x0;
    let vy = y1 - y0;
    let len2 = vx * vx + vy * vy;
    for y in miny..=maxy {
        for x in minx..=maxx {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let t = if len2 < 1e-8 {
                0.0
            } else {
                ((px - x0) * vx + (py - y0) * vy) / len2
            };
            let t = t.clamp(0.0, 1.0);
            let d = (px - (x0 + t * vx)).hypot(py - (y0 + t * vy));
            let cov = if d <= radius - edge {
                1.0
            } else if d >= radius + edge {
                0.0
            } else {
                (radius + edge - d) / (2.0 * edge)
            };
            if cov > 0.0 {
                ttf::blend(buf, width, height, x, y, color, cov);
            }
        }
    }
}

fn paint_paper(buf: &mut [u8], width: u32, height: u32) {
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&PAPER);
    }
    fill_rect(
        buf,
        width,
        height,
        0,
        0,
        width,
        ACCENT_H.min(height),
        ACCENT,
    );
    fill_rect(buf, width, height, 0, 0, 1, height, BORDER);
    if width > 1 {
        fill_rect(buf, width, height, (width - 1) as i32, 0, 1, height, BORDER);
    }
    if height > 1 {
        fill_rect(buf, width, height, 0, (height - 1) as i32, width, 1, BORDER);
    }
}

fn fill_rect(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    color: [u8; 4],
) {
    if w == 0 || h == 0 {
        return;
    }
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x + w as i32).max(0) as u32).min(width);
    let y1 = ((y + h as i32).max(0) as u32).min(height);
    for yy in y0..y1 {
        for xx in x0..x1 {
            let i = ((yy * width + xx) * 4) as usize;
            buf[i..i + 4].copy_from_slice(&color);
        }
    }
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
        assert_eq!(
            lines[3],
            format!("Baudatum: {}", env!("SCREENMIRROR_BUILD_DATE"))
        );
        let git = lines[2].split_once(": ").unwrap().1;
        let date = lines[3].split_once(": ").unwrap().1;
        assert!(!git.is_empty() && !git.contains(char::is_whitespace));
        assert!(
            date == "unbekannt"
                || (date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-')
        );
    }

    #[test]
    fn dialog_is_a_small_light_window() {
        let (w, h) = content_size();
        assert!((420..=1400).contains(&w), "width {w}");
        assert!((180..=800).contains(&h), "height {h}");
        let buf = render(w, h);
        assert_eq!(buf.len(), w as usize * h as usize * 4);
        // Framed, not a black field. The corner is the hairline; inside is paper.
        assert_eq!(&buf[0..4], &BORDER);
        let inset = ((12 * w + 12) * 4) as usize;
        assert_eq!(&buf[inset..inset + 4], &PAPER);
        let light = buf
            .chunks_exact(4)
            .filter(|p| p[0] > 220 && p[1] > 220 && p[2] > 220)
            .count();
        assert!(light * 10 > buf.len() / 4 * 6, "surface should stay light");
        // Accent sits on the top edge, inside the hairline.
        let accent = ((2 * w + 8) * 4) as usize;
        assert_eq!(&buf[accent..accent + 4], &ACCENT);
    }

    #[test]
    fn each_fact_and_the_title_paint_ink() {
        let (w, h) = content_size();
        let buf = render(w, h);
        let dark = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            let s = buf[i] as u16 + buf[i + 1] as u16 + buf[i + 2] as u16;
            s < 520
        };
        let mut bands = [0u32; 8];
        for y in ACCENT_H..h.saturating_sub(2) {
            let mut row = 0u32;
            for x in 8..w.saturating_sub(8) {
                if dark(x, y) {
                    row += 1;
                }
            }
            if row > 8 {
                bands[(y * 8 / h).min(7) as usize] += 1;
            }
        }
        let used = bands.iter().filter(|n| **n > 0).count();
        assert!(
            used >= 4,
            "ink should span the title and the facts, bands {bands:?}"
        );
    }

    #[test]
    fn system_font_is_antialiased_when_installed() {
        if typeface_path().is_none() {
            return;
        }
        let (w, h) = content_size();
        let buf = render(w, h);
        let mut gray = 0u32;
        for px in buf.chunks_exact(4) {
            let sum = px[0] as i16 + px[1] as i16 + px[2] as i16;
            let spread = (px[0] as i16 - px[2] as i16).abs();
            if (120..680).contains(&sum) && spread < 40 {
                gray += 1;
            }
        }
        assert!(
            gray > 200,
            "expected antialiased system sans, gray pixels {gray}"
        );
        let path = typeface_path().unwrap();
        assert!(
            path.ends_with(".ttf") || path.ends_with(".otf") || path.ends_with(".ttc"),
            "{path}"
        );
    }

    #[test]
    fn missing_font_notice_still_paints_the_facts() {
        let (w, h) = notice_size();
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        paint_paper(&mut buf, w, h);
        paint_notice(&mut buf, w, h);
        let ink = buf
            .chunks_exact(4)
            .filter(|p| {
                let s = p[0] as u16 + p[1] as u16 + p[2] as u16;
                s < 500
            })
            .count();
        assert!(ink > 400, "notice ink {ink}");
        // The notice is a stroked face, so coverage is not a 1-bit bitmap.
        let mut levels = std::collections::BTreeSet::new();
        for px in buf.chunks_exact(4) {
            levels.insert(px[0] / 16);
        }
        assert!(levels.len() > 4, "notice should not be the old pixel font");
    }

    #[test]
    fn git_id_fits_the_dialog_width() {
        let Some(face) = installed() else { return };
        let (w, _) = content_size();
        let git = lines()[2];
        let text_w = face.regular.measure(git, BODY_PX);
        assert!(
            text_w + PAD_X * 2.0 + 4.0 <= w as f32,
            "git line {text_w} in {w}"
        );
    }

    #[test]
    fn preview_ppm() {
        if std::env::var_os("SCREENMIRROR_PREVIEW").is_none() {
            return;
        }
        let (w, h) = content_size();
        let buf = render(w, h);
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in buf.chunks_exact(4) {
            ppm.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        let path = std::env::var("SCREENMIRROR_PREVIEW")
            .unwrap_or_else(|_| "/tmp/about-preview.ppm".into());
        std::fs::write(&path, ppm).unwrap();
        eprintln!(
            "screenmirror: about preview {path} ({w}x{h}) face {:?}",
            typeface_path()
        );
    }
}
