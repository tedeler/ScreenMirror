//! Über window, laid out like a KDE About dialog.
//!
//! A light client area, an original pair of screens on the left, the four
//! identity facts in the system sans on the right, and a Schließen button at
//! the bottom right. The face is fontconfig's `sans` (Plasma uses Noto Sans),
//! read from the font file when the window opens. Nothing is bundled. A missing
//! font draws a notice in the same window and does not stop the mirror.

#[path = "stroke.rs"]
mod stroke;
#[path = "ttf.rs"]
mod ttf;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use ttf::Font;

/// `xdg_toplevel` title. The compositor draws it in the frame.
pub const WINDOW_TITLE: &str = "Über ScreenMirror";

const HEADING: &str = "ScreenMirror";
const INTRO: &str = "Der Bildschirm mit der Plasma-Leiste erscheint auf dem anderen Bildschirm.";
const BUTTON_LABEL: &str = "Schließen";
const NOTICE: &str = "Systemschrift nicht gefunden.";

const BODY_PX: f32 = 17.0;
const TITLE_PX: f32 = 26.0;
const TITLE_PX_SOFT: f32 = 28.0;
const INTRO_PX: f32 = 16.0;
const BUTTON_PX: f32 = 15.0;
const LEFT_COL: f32 = 256.0;
const EDGE: f32 = 16.0;
const PANEL_GAP: f32 = 14.0;
const PANEL_PAD_X: f32 = 32.0;
const PANEL_PAD_TOP: f32 = 28.0;
const PANEL_PAD_BOT: f32 = 24.0;
const TITLE_GAP: f32 = 12.0;
const INTRO_GAP: f32 = 22.0;

/// XRGB8888, little-endian byte order B, G, R, X.
const WINDOW: [u8; 4] = [0xF1, 0xF0, 0xEF, 0xFF];
const PANEL: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
const PANEL_EDGE: [u8; 4] = [0xD0, 0xCC, 0xC8, 0xFF];
const INK: [u8; 4] = [0x32, 0x30, 0x2E, 0xFF];
const MUTED: [u8; 4] = [0x62, 0x5C, 0x54, 0xFF];
const BUTTON_FILL: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
const BUTTON_EDGE: [u8; 4] = [0xA4, 0x9E, 0x96, 0xFF];
const HAIRLINE: [u8; 4] = [0xD6, 0xD2, 0xCE, 0xFF];
/// Disc behind the corner ×, still lighter than the ink.
const CLOSE_DISC: [u8; 4] = [0xE2, 0xDC, 0xD6, 0xFF];
/// Side of the square close control, in buffer pixels.
const CLOSE_SIDE: u32 = 40;

const BEZEL: [u8; 4] = [0x4A, 0x3C, 0x2E, 0xFF];
const SKY: [u8; 4] = [0xE6, 0xC4, 0x8C, 0xFF];
const MEADOW: [u8; 4] = [0x68, 0xB0, 0x72, 0xFF];
const HILL: [u8; 4] = [0x4C, 0x90, 0x52, 0xFF];
const SUN: [u8; 4] = [0x4C, 0xB4, 0xF0, 0xFF];
const CLOUD: [u8; 4] = [0xFF, 0xFB, 0xF7, 0xFF];
const STAND: [u8; 4] = [0x62, 0x54, 0x46, 0xFF];
const BACKDROP: [u8; 4] = [0xF2, 0xE6, 0xDC, 0xFF];

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
    natural_size(installed())
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
    paint_window(&mut buf, width, height);
    let placed = place(width, height);
    paint_illustration(&mut buf, width, height, placed.art);
    paint_panel(&mut buf, width, height, placed.panel);
    match installed() {
        Some(face) => paint_prose(&mut buf, width, height, Some(face), &placed),
        None => paint_notice(&mut buf, width, height),
    }
    paint_button(&mut buf, width, height, installed(), placed.button);
    paint_close(&mut buf, width, height);
    buf
}

/// Schließen hit target, in buffer pixels. `(x, y, width, height)`.
pub fn button_rect(width: u32, height: u32) -> (i32, i32, u32, u32) {
    if width < 48 || height < 48 {
        return (0, 0, 0, 0);
    }
    place(width, height).button
}

/// True when `(px, py)` lies on the Schließen button. Coordinates are surface pixels.
pub fn button_contains(width: u32, height: u32, px: f64, py: f64) -> bool {
    contains_rect(button_rect(width, height), px, py)
}

/// Square hit target of the corner close control, in buffer pixels.
/// `(x, y, width, height)`. Empty when the surface is too small to draw one.
pub fn close_rect(width: u32, height: u32) -> (i32, i32, u32, u32) {
    if width < 16 || height < 16 {
        return (0, 0, 0, 0);
    }
    let side = CLOSE_SIDE.min(width).min(height);
    let inset_x = 10.min((width - side) as i32);
    let inset_y = 10.min((height - side) as i32);
    let x = width as i32 - side as i32 - inset_x;
    (x.max(0), inset_y.max(0), side, side)
}

/// True when `(px, py)` lies in the close control. Coordinates are surface pixels.
pub fn close_contains(width: u32, height: u32, px: f64, py: f64) -> bool {
    contains_rect(close_rect(width, height), px, py)
}

fn contains_rect(rect: (i32, i32, u32, u32), px: f64, py: f64) -> bool {
    if !px.is_finite() || !py.is_finite() {
        return false;
    }
    let (x, y, w, h) = rect;
    if w == 0 || h == 0 {
        return false;
    }
    px >= x as f64 && py >= y as f64 && px < x as f64 + w as f64 && py < y as f64 + h as f64
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

struct Rhythm {
    text_h: f32,
    title_px: f32,
    title_baseline: f32,
    intro_step: f32,
    intro_baseline: f32,
    body_baseline: f32,
    body_step: f32,
    label_col: f32,
}

struct Place {
    panel: (i32, i32, u32, u32),
    art: (i32, i32, u32, u32),
    button: (i32, i32, u32, u32),
    text_x: f32,
    text_max_w: f32,
    label_col: f32,
    title_px: f32,
    title_baseline: f32,
    intro_step: f32,
    intro_baseline: f32,
    body_baseline: f32,
    body_step: f32,
    notice_top: f32,
}

fn face_ref(face: Option<&Face>, bold: bool) -> Option<&Font> {
    face.map(|f| {
        if bold {
            f.bold.as_ref().unwrap_or(&f.regular)
        } else {
            &f.regular
        }
    })
}

fn text_width(face: Option<&Face>, text: &str, px: f32, bold: bool) -> f32 {
    match face_ref(face, bold) {
        Some(font) => font.measure(text, px),
        None => stroke_width(text, px),
    }
}

fn text_lh(face: Option<&Face>, px: f32, bold: bool) -> f32 {
    match face_ref(face, bold) {
        Some(font) => font.line_height(px),
        None => px * 1.45,
    }
}

fn text_ascent(face: Option<&Face>, px: f32, bold: bool) -> f32 {
    match face_ref(face, bold) {
        Some(font) => font.ascent(px),
        None => px * 0.80,
    }
}

fn title_px(face: Option<&Face>) -> f32 {
    if face.is_some_and(|f| f.bold.is_some()) {
        TITLE_PX
    } else {
        TITLE_PX_SOFT
    }
}

fn fact_parts(line: &str) -> (&str, &str) {
    line.split_once(": ").unwrap_or((line, ""))
}

fn label_column(face: Option<&Face>) -> f32 {
    ["Autor", "Version", "Git-ID", "Baudatum"]
        .into_iter()
        .map(|label| text_width(face, label, BODY_PX, false))
        .fold(0.0_f32, f32::max)
        + 28.0
}

fn preferred_text_w(face: Option<&Face>) -> f32 {
    let heading = text_width(face, HEADING, title_px(face), true);
    let labels = label_column(face);
    let value = lines()
        .iter()
        .map(|line| text_width(face, fact_parts(line).1, BODY_PX, false))
        .fold(0.0_f32, f32::max);
    heading.max(labels + value + 10.0).max(420.0)
}

fn wrap_measured(text: &str, max_w: f32, measure: impl Fn(&str) -> f32) -> Vec<String> {
    if text.is_empty() || measure(text) <= max_w {
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
            if measure(&text[start..next]) <= max_w {
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
        while start < text.len() && text.as_bytes().get(start) == Some(&b' ') {
            start += 1;
        }
    }
    if lines.is_empty() {
        vec![text.to_string()]
    } else {
        lines
    }
}

fn intro_lines(face: Option<&Face>, max_w: f32) -> Vec<String> {
    wrap_measured(INTRO, max_w, |text| text_width(face, text, INTRO_PX, false))
}

fn rhythm(face: Option<&Face>, text_w: f32) -> Rhythm {
    let title_px = title_px(face);
    let title_lh = text_lh(face, title_px, true);
    let intro_step = text_lh(face, INTRO_PX, false).max(INTRO_PX * 1.25);
    let intro_n = intro_lines(face, text_w).len().max(1);
    let body_lh = text_lh(face, BODY_PX, false);
    let body_step = body_lh * 1.62;
    let notice_h = if face.is_none() {
        text_lh(None, BODY_PX, false) + 8.0
    } else {
        0.0
    };
    let mut y = PANEL_PAD_TOP + notice_h;
    let title_baseline = y + text_ascent(face, title_px, true);
    y += title_lh + TITLE_GAP;
    let intro_baseline = y + text_ascent(face, INTRO_PX, false);
    y += intro_step * intro_n as f32 + INTRO_GAP;
    let body_baseline = y + text_ascent(face, BODY_PX, false);
    y += body_step * 4.0 + PANEL_PAD_BOT;
    Rhythm {
        text_h: y,
        title_px,
        title_baseline,
        intro_step,
        intro_baseline,
        body_baseline,
        body_step,
        label_col: label_column(face),
    }
}

fn button_size(face: Option<&Face>) -> (u32, u32) {
    let label = text_width(face, BUTTON_LABEL, BUTTON_PX, false);
    let w = (label + 54.0).ceil().max(132.0) as u32;
    let h = (text_lh(face, BUTTON_PX, false) + 16.0)
        .ceil()
        .clamp(34.0, 44.0) as u32;
    (w, h)
}

fn natural_size(face: Option<&Face>) -> (u32, u32) {
    let text_w = preferred_text_w(face);
    let block = rhythm(face, text_w);
    let (_, bh) = button_size(face);
    let width = (LEFT_COL + PANEL_PAD_X * 2.0 + text_w + EDGE)
        .ceil()
        .max(740.0) as u32;
    let height = (EDGE + block.text_h + PANEL_GAP + bh as f32 + EDGE)
        .ceil()
        .max(420.0) as u32;
    (width, height)
}

fn notice_size() -> (u32, u32) {
    natural_size(None)
}

fn place(width: u32, height: u32) -> Place {
    let face = installed();
    let (bw, bh) = button_size(face);
    let margin = EDGE as i32;
    let bx = (width as i32 - margin - bw as i32).max(4);
    let by = (height as i32 - margin - bh as i32).max(4);
    let button = (
        bx,
        by,
        bw.min(width.saturating_sub(8)),
        bh.min(height.saturating_sub(8)),
    );

    let panel_x = (LEFT_COL as i32).min((width as i32 - 120).max(margin));
    let panel_y = margin;
    let panel_r = (width as i32 - margin).max(panel_x + 40);
    let panel_b = (by - PANEL_GAP as i32).max(panel_y + 80);
    let panel = (
        panel_x,
        panel_y,
        (panel_r - panel_x).max(0) as u32,
        (panel_b - panel_y).max(0) as u32,
    );

    let art_b = (by - 8).max(panel_y + 40);
    let art = (
        8,
        18,
        panel_x.saturating_sub(16).max(0) as u32,
        (art_b - 26).max(0) as u32,
    );

    let text_x = panel_x as f32 + PANEL_PAD_X;
    let text_max_w = (panel.2 as f32 - PANEL_PAD_X * 2.0).max(40.0);
    let block = rhythm(face, text_max_w);
    let top = panel_y as f32;
    Place {
        panel,
        art,
        button,
        text_x,
        text_max_w,
        label_col: block.label_col,
        title_px: block.title_px,
        title_baseline: top + block.title_baseline,
        intro_step: block.intro_step,
        intro_baseline: top + block.intro_baseline,
        body_baseline: top + block.body_baseline,
        body_step: block.body_step,
        notice_top: top + PANEL_PAD_TOP,
    }
}

fn draw_text(
    buf: &mut [u8],
    width: u32,
    height: u32,
    face: Option<&Face>,
    text: &str,
    x: f32,
    baseline: f32,
    px: f32,
    color: [u8; 4],
    bold: bool,
) {
    if let Some(font) = face_ref(face, bold) {
        font.draw_line(buf, width, height, text, x, baseline, px, color);
    } else {
        draw_strokes(buf, width, height, text, x, baseline - px * 0.78, px, color);
    }
}

fn paint_prose(buf: &mut [u8], width: u32, height: u32, face: Option<&Face>, placed: &Place) {
    draw_text(
        buf,
        width,
        height,
        face,
        HEADING,
        placed.text_x,
        placed.title_baseline,
        placed.title_px,
        INK,
        true,
    );
    let mut baseline = placed.intro_baseline;
    for line in intro_lines(face, placed.text_max_w) {
        draw_text(
            buf,
            width,
            height,
            face,
            &line,
            placed.text_x,
            baseline,
            INTRO_PX,
            INK,
            false,
        );
        baseline += placed.intro_step;
    }
    baseline = placed.body_baseline;
    for line in lines() {
        let (label, value) = fact_parts(line);
        draw_text(
            buf,
            width,
            height,
            face,
            label,
            placed.text_x,
            baseline,
            BODY_PX,
            MUTED,
            false,
        );
        draw_text(
            buf,
            width,
            height,
            face,
            value,
            placed.text_x + placed.label_col,
            baseline,
            BODY_PX,
            INK,
            false,
        );
        baseline += placed.body_step;
    }
}

fn paint_notice(buf: &mut [u8], width: u32, height: u32) {
    let placed = place(width, height);
    draw_strokes(
        buf,
        width,
        height,
        NOTICE,
        placed.text_x,
        placed.notice_top,
        BODY_PX,
        MUTED,
    );
    paint_prose(buf, width, height, None, &placed);
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

fn paint_close(buf: &mut [u8], width: u32, height: u32) {
    let (x, y, w, h) = close_rect(width, height);
    if w < 8 || h < 8 {
        return;
    }
    let cx = x as f32 + w as f32 * 0.5;
    let cy = y as f32 + h as f32 * 0.5;
    let disc_r = w.min(h) as f32 * 0.46;
    paint_disc(buf, width, height, cx, cy, disc_r, CLOSE_DISC);
    let arm = w.min(h) as f32 * 0.28;
    let radius = (w.min(h) as f32 * 0.075).max(1.6);
    stroke(
        buf,
        width,
        height,
        cx - arm,
        cy - arm,
        cx + arm,
        cy + arm,
        radius,
        INK,
    );
    stroke(
        buf,
        width,
        height,
        cx + arm,
        cy - arm,
        cx - arm,
        cy + arm,
        radius,
        INK,
    );
}

fn paint_disc(
    buf: &mut [u8],
    width: u32,
    height: u32,
    cx: f32,
    cy: f32,
    radius: f32,
    color: [u8; 4],
) {
    let edge = 0.85;
    let minx = (cx - radius - edge - 1.0).floor() as i32;
    let maxx = (cx + radius + edge + 1.0).ceil() as i32;
    let miny = (cy - radius - edge - 1.0).floor() as i32;
    let maxy = (cy + radius + edge + 1.0).ceil() as i32;
    for y in miny..=maxy {
        for x in minx..=maxx {
            let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
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

fn paint_window(buf: &mut [u8], width: u32, height: u32) {
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&WINDOW);
    }
    fill_rect(buf, width, height, 0, 0, 1, height, HAIRLINE);
    if width > 1 {
        fill_rect(
            buf,
            width,
            height,
            (width - 1) as i32,
            0,
            1,
            height,
            HAIRLINE,
        );
    }
    if height > 0 {
        fill_rect(buf, width, height, 0, 0, width, 1, HAIRLINE);
    }
    if height > 1 {
        fill_rect(
            buf,
            width,
            height,
            0,
            (height - 1) as i32,
            width,
            1,
            HAIRLINE,
        );
    }
}

fn paint_panel(buf: &mut [u8], width: u32, height: u32, panel: (i32, i32, u32, u32)) {
    let (x, y, w, h) = panel;
    if w < 8 || h < 8 {
        return;
    }
    paint_round(
        buf,
        width,
        height,
        x as f32,
        y as f32,
        w as f32,
        h as f32,
        8.0,
        Some(PANEL),
        Some(PANEL_EDGE),
    );
}

fn paint_button(
    buf: &mut [u8],
    width: u32,
    height: u32,
    face: Option<&Face>,
    button: (i32, i32, u32, u32),
) {
    let (x, y, w, h) = button;
    if w < 8 || h < 8 {
        return;
    }
    paint_round(
        buf,
        width,
        height,
        x as f32,
        y as f32,
        w as f32,
        h as f32,
        5.0,
        Some(BUTTON_FILL),
        Some(BUTTON_EDGE),
    );
    let cy = y as f32 + h as f32 * 0.5;
    let ix = x as f32 + 16.0;
    let arm = 4.6;
    stroke(
        buf,
        width,
        height,
        ix - arm,
        cy - arm,
        ix + arm,
        cy + arm,
        1.15,
        INK,
    );
    stroke(
        buf,
        width,
        height,
        ix + arm,
        cy - arm,
        ix - arm,
        cy + arm,
        1.15,
        INK,
    );
    let text_x = ix + 14.0;
    let baseline = y as f32
        + (h as f32 - text_lh(face, BUTTON_PX, false)) * 0.5
        + text_ascent(face, BUTTON_PX, false);
    draw_text(
        buf,
        width,
        height,
        face,
        BUTTON_LABEL,
        text_x,
        baseline,
        BUTTON_PX,
        INK,
        false,
    );
}

fn paint_illustration(buf: &mut [u8], width: u32, height: u32, art: (i32, i32, u32, u32)) {
    let (ax, ay, aw, ah) = art;
    if aw < 48 || ah < 48 {
        return;
    }
    let cx = ax as f32 + aw as f32 * 0.50;
    let cy = ay as f32 + ah as f32 * 0.46;
    let backdrop = (aw as f32).min(ah as f32) * 0.40;
    paint_circle(buf, width, height, cx, cy + 8.0, backdrop, BACKDROP);
    let mon_w = ((aw as f32 - 28.0) * 0.46).clamp(72.0, 118.0);
    let mon_h = mon_w * 0.68;
    let gap = 16.0;
    let pair_w = mon_w * 2.0 + gap;
    let x0 = ax as f32 + (aw as f32 - pair_w) * 0.5;
    let foot = (cy + mon_h * 0.62).min(ay as f32 + ah as f32 - 16.0);
    paint_ellipse(
        buf,
        width,
        height,
        cx,
        foot + 10.0,
        pair_w * 0.42,
        7.0,
        [0xE4, 0xDE, 0xD8, 0xFF],
    );
    paint_monitor(
        buf,
        width,
        height,
        x0,
        foot - mon_h - 8.0,
        mon_w,
        mon_h,
        foot,
        false,
    );
    paint_monitor(
        buf,
        width,
        height,
        x0 + mon_w + gap,
        foot - mon_h - 20.0,
        mon_w,
        mon_h,
        foot,
        true,
    );
}

fn paint_monitor(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    foot: f32,
    mirrored: bool,
) {
    paint_round(buf, width, height, x, y, w, h, 7.0, Some(BEZEL), None);
    let inset = (w * 0.075).max(6.0);
    let sx = x + inset;
    let sy = y + inset;
    let sw = (w - inset * 2.0).max(4.0);
    let sh = (h - inset * 2.2).max(4.0);
    fill_rect(
        buf,
        width,
        height,
        sx as i32,
        sy as i32,
        sw as u32,
        (sh * 0.58) as u32,
        SKY,
    );
    let land_y = sy + sh * 0.56;
    fill_rect(
        buf,
        width,
        height,
        sx as i32,
        land_y as i32,
        sw as u32,
        (sy + sh - land_y).max(1.0) as u32,
        MEADOW,
    );
    let hill_x = if mirrored {
        sx + sw * 0.66
    } else {
        sx + sw * 0.34
    };
    paint_ellipse(
        buf,
        width,
        height,
        hill_x,
        sy + sh * 0.70,
        sw * 0.28,
        sh * 0.20,
        HILL,
    );
    let sun_x = if mirrored {
        sx + sw * 0.30
    } else {
        sx + sw * 0.70
    };
    paint_circle(buf, width, height, sun_x, sy + sh * 0.28, sw * 0.075, SUN);
    let cloud_x = if mirrored {
        sx + sw * 0.62
    } else {
        sx + sw * 0.34
    };
    paint_ellipse(
        buf,
        width,
        height,
        cloud_x,
        sy + sh * 0.24,
        sw * 0.14,
        sh * 0.07,
        CLOUD,
    );
    let neck_w = (w * 0.09).max(6.0);
    let neck_top = y + h - 1.0;
    if foot > neck_top {
        fill_rect(
            buf,
            width,
            height,
            (x + w * 0.5 - neck_w * 0.5) as i32,
            neck_top as i32,
            neck_w as u32,
            (foot - neck_top) as u32,
            STAND,
        );
    }
    let base_w = w * 0.46;
    paint_round(
        buf,
        width,
        height,
        x + w * 0.5 - base_w * 0.5,
        foot - 2.0,
        base_w,
        (h * 0.07).max(6.0),
        3.0,
        Some(STAND),
        None,
    );
}

fn paint_round(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    fill: Option<[u8; 4]>,
    border: Option<[u8; 4]>,
) {
    if w <= 1.0 || h <= 1.0 {
        return;
    }
    let radius = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    let x0 = (x - 1.5).floor().max(0.0) as i32;
    let y0 = (y - 1.5).floor().max(0.0) as i32;
    let x1 = ((x + w + 1.5).ceil() as i32).min(width as i32 - 1);
    let y1 = ((y + h + 1.5).ceil() as i32).min(height as i32 - 1);
    for py in y0..=y1 {
        for px in x0..=x1 {
            let sd = sd_round_rect(px as f32 + 0.5, py as f32 + 0.5, x, y, w, h, radius);
            if let Some(color) = fill {
                if sd <= -1.05 {
                    put(buf, width, height, px, py, color);
                } else {
                    let cov = coverage_inside(sd);
                    if cov > 0.0 {
                        ttf::blend(buf, width, height, px, py, color, cov);
                    }
                }
            }
            if let Some(color) = border {
                let cov = coverage_inside(sd.abs() - 0.75);
                if cov > 0.0 {
                    ttf::blend(buf, width, height, px, py, color, cov);
                }
            }
        }
    }
}

fn paint_circle(
    buf: &mut [u8],
    width: u32,
    height: u32,
    cx: f32,
    cy: f32,
    radius: f32,
    color: [u8; 4],
) {
    if radius <= 0.5 {
        return;
    }
    let x0 = (cx - radius - 1.5).floor().max(0.0) as i32;
    let y0 = (cy - radius - 1.5).floor().max(0.0) as i32;
    let x1 = ((cx + radius + 1.5).ceil() as i32).min(width as i32 - 1);
    let y1 = ((cy + radius + 1.5).ceil() as i32).min(height as i32 - 1);
    for py in y0..=y1 {
        for px in x0..=x1 {
            let sd = (px as f32 + 0.5 - cx).hypot(py as f32 + 0.5 - cy) - radius;
            if sd <= -1.05 {
                put(buf, width, height, px, py, color);
            } else {
                let cov = coverage_inside(sd);
                if cov > 0.0 {
                    ttf::blend(buf, width, height, px, py, color, cov);
                }
            }
        }
    }
}

fn paint_ellipse(
    buf: &mut [u8],
    width: u32,
    height: u32,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    color: [u8; 4],
) {
    let rx = rx.max(0.5);
    let ry = ry.max(0.5);
    let x0 = (cx - rx - 1.5).floor().max(0.0) as i32;
    let y0 = (cy - ry - 1.5).floor().max(0.0) as i32;
    let x1 = ((cx + rx + 1.5).ceil() as i32).min(width as i32 - 1);
    let y1 = ((cy + ry + 1.5).ceil() as i32).min(height as i32 - 1);
    for py in y0..=y1 {
        for px in x0..=x1 {
            let nx = (px as f32 + 0.5 - cx) / rx;
            let ny = (py as f32 + 0.5 - cy) / ry;
            let sd = (nx.hypot(ny) - 1.0) * rx.min(ry);
            if sd <= -1.05 {
                put(buf, width, height, px, py, color);
            } else {
                let cov = coverage_inside(sd);
                if cov > 0.0 {
                    ttf::blend(buf, width, height, px, py, color, cov);
                }
            }
        }
    }
}

fn sd_round_rect(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, radius: f32) -> f32 {
    let qx = (px - (x + w * 0.5)).abs() - w * 0.5 + radius;
    let qy = (py - (y + h * 0.5)).abs() - h * 0.5 + radius;
    qy.max(qx).min(0.0) + qx.max(0.0).hypot(qy.max(0.0)) - radius
}

fn coverage_inside(sd: f32) -> f32 {
    let edge = 0.85;
    if sd <= -edge {
        1.0
    } else if sd >= edge {
        0.0
    } else {
        (edge - sd) / (2.0 * edge)
    }
}

fn put(buf: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let i = ((y as u32 * width + x as u32) * 4) as usize;
    if i + 4 <= buf.len() {
        buf[i..i + 4].copy_from_slice(&color);
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
    fn dialog_is_a_light_about_window() {
        let (w, h) = content_size();
        assert!((640..=1400).contains(&w), "width {w}");
        assert!((400..=800).contains(&h), "height {h}");
        let buf = render(w, h);
        assert_eq!(buf.len(), w as usize * h as usize * 4);
        assert_eq!(&buf[0..4], &HAIRLINE);
        let inset = ((6 * w + 6) * 4) as usize;
        assert_eq!(&buf[inset..inset + 4], &WINDOW);
        let light = buf
            .chunks_exact(4)
            .filter(|p| p[0] > 220 && p[1] > 220 && p[2] > 220)
            .count();
        assert!(light * 10 > buf.len() / 4 * 6, "surface should stay light");
        let placed = place(w, h);
        let (px, py) = (placed.panel.0 + 24, placed.panel.1 + 10);
        let i = ((py as u32 * w + px as u32) * 4) as usize;
        assert_eq!(&buf[i..i + 4], &PANEL, "the right side is a light card");
        let mut color = 0u32;
        let x1 = (placed.art.0 + placed.art.2 as i32).max(0) as u32;
        let y1 = (placed.art.1 + placed.art.3 as i32).max(0) as u32;
        for y in (placed.art.1.max(0) as u32)..y1.min(h) {
            for x in (placed.art.0.max(0) as u32)..x1.min(w / 2) {
                let i = ((y * w + x) * 4) as usize;
                let (b, g, r) = (buf[i] as i16, buf[i + 1] as i16, buf[i + 2] as i16);
                if (r - b).abs() > 28 || (g - b).abs() > 36 {
                    color += 1;
                }
            }
        }
        assert!(
            color > 400,
            "illustration should sit on the left, colorful pixels {color}"
        );
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
        for y in 8..h.saturating_sub(2) {
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
        paint_window(&mut buf, w, h);
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
    fn git_id_fits_the_text_column() {
        let Some(face) = installed() else { return };
        let (w, h) = content_size();
        let placed = place(w, h);
        let value = lines()[2].split_once(": ").unwrap().1;
        let text_w = face.regular.measure(value, BODY_PX);
        assert!(
            placed.label_col + text_w <= placed.text_max_w + 1.0,
            "git value {text_w} col {} max {}",
            placed.label_col,
            placed.text_max_w
        );
        let last = placed.body_baseline + placed.body_step * 3.0 + 6.0;
        let bottom = placed.panel.1 as f32 + placed.panel.3 as f32;
        assert!(
            last < bottom,
            "facts overflow the card ({last} >= {bottom})"
        );
    }

    #[test]
    fn schliessen_button_is_a_button_at_the_bottom_right() {
        let (w, h) = content_size();
        let (x, y, bw, bh) = button_rect(w, h);
        assert!(bw >= 110 && bh >= 32, "button {bw}x{bh}");
        assert!(x as u32 > w / 2, "Schließen stays on the right, x {x}");
        assert!(y as u32 > h / 2, "Schließen stays along the bottom, y {y}");
        assert!(y as u32 + bh + 6 < h, "padding under the button");
        assert!(x as u32 + bw + 6 < w, "padding to the right of the button");
        let buf = render(w, h);
        let sum = |px: i32, py: i32| {
            let i = ((py as u32 * w + px as u32) * 4) as usize;
            buf[i] as u16 + buf[i + 1] as u16 + buf[i + 2] as u16
        };
        let edge = sum(x, y + bh as i32 / 2);
        let fill = sum(x + bw as i32 - 8, y + bh as i32 / 2);
        assert!(
            edge + 30 < fill,
            "border {edge} should be darker than the fill {fill}"
        );
        let mut ink = 0u32;
        for py in y..y + bh as i32 {
            for px in x..x + bw as i32 {
                if sum(px, py) < 420 {
                    ink += 1;
                }
            }
        }
        assert!(ink > 70, "Schließen label ink {ink}");
        assert!(button_contains(
            w,
            h,
            x as f64 + bw as f64 * 0.5,
            y as f64 + bh as f64 * 0.5
        ));
        assert!(!close_contains(
            w,
            h,
            x as f64 + 4.0,
            y as f64 + bh as f64 * 0.5
        ));
        let (kx, ky, kw, kh) = close_rect(w, h);
        assert!(!button_contains(
            w,
            h,
            kx as f64 + kw as f64 * 0.5,
            ky as f64 + kh as f64 * 0.5
        ));
        assert!(!button_contains(w, h, f64::NAN, 0.0));
        assert_eq!(button_rect(0, 0), (0, 0, 0, 0));
    }

    #[test]
    fn window_title_names_screenmirror() {
        assert_eq!(WINDOW_TITLE, "Über ScreenMirror");
    }

    #[test]
    fn close_control_is_a_cross_in_the_top_right() {
        let (w, h) = content_size();
        let (x, y, bw, bh) = close_rect(w, h);
        assert!(bw >= 16 && bh >= 16, "close control {bw}x{bh}");
        assert!(x as u32 > w / 2, "close control stays on the right, x {x}");
        assert!(y < 24, "close control stays in the top corner, y {y}");
        assert!(x as u32 + bw < w, "inset from the hairline");
        let buf = render(w, h);
        let sum = |px: i32, py: i32| {
            let i = ((py as u32 * w + px as u32) * 4) as usize;
            buf[i] as u16 + buf[i + 1] as u16 + buf[i + 2] as u16
        };
        let cx = x + bw as i32 / 2;
        let cy = y + bh as i32 / 2;
        assert!(sum(cx, cy) < 250, "crossing of the × should be dark");
        // Both arms of the ×, inside the stroke, not only the center pixel.
        for (dx, dy) in [(5, 5), (-5, -5), (5, -5), (-5, 5)] {
            assert!(
                sum(cx + dx, cy + dy) < 250,
                "arm at {dx},{dy} sum {}",
                sum(cx + dx, cy + dy)
            );
        }
        // Straight above the crossing is the disc, so the mark is a cross.
        assert!(sum(cx, cy - 14) > 400, "disc above the × stays light");
        assert!(close_contains(w, h, cx as f64, cy as f64));
        assert!(close_contains(w, h, x as f64, y as f64));
        assert!(!close_contains(w, h, x as f64 + bw as f64, cy as f64));
        assert!(!close_contains(w, h, 20.0, 30.0));
        assert!(!close_contains(w, h, f64::NAN, 0.0));
        assert_eq!(close_rect(0, 0), (0, 0, 0, 0));
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
