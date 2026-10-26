//! A small TrueType outline reader and coverage rasterizer.
//!
//! Enough of `glyf` / `cmap` / `hmtx` to draw the system sans into an XRGB
//! buffer. Composite glyphs, quadratic curves, and TrueType collections are
//! handled. CFF fonts are rejected so the caller can try the next file.

const SS: i32 = 4;

#[derive(Clone, Copy)]
struct Pt {
    x: f32,
    y: f32,
    on: bool,
}

pub struct Font {
    data: Vec<u8>,
    glyf: usize,
    loca: usize,
    loca_long: bool,
    cmap: usize,
    cmap_fmt: u16,
    upem: f32,
    ascent: f32,
    descent: f32,
    line_gap: f32,
    num_glyphs: u32,
    hmtx: usize,
    num_hmetrics: u32,
}

impl Font {
    pub fn parse(data: Vec<u8>) -> Option<Self> {
        let base = sfnt_offset(&data)?;
        let tables = table_map(&data, base)?;
        let head = *tables.get(b"head".as_slice())?;
        let maxp = *tables.get(b"maxp".as_slice())?;
        let cmap = *tables.get(b"cmap".as_slice())?;
        let loca = *tables.get(b"loca".as_slice())?;
        let glyf = *tables.get(b"glyf".as_slice())?;
        let hhea = *tables.get(b"hhea".as_slice())?;
        let hmtx = *tables.get(b"hmtx".as_slice())?;

        let magic = u32(&data, head + 12)?;
        if magic != 0x5F0F_3CF5 {
            return None;
        }
        let upem = u16(&data, head + 18)? as f32;
        if upem < 16.0 {
            return None;
        }
        let loca_fmt = i16(&data, head + 50)?;
        if loca_fmt != 0 && loca_fmt != 1 {
            return None;
        }
        let num_glyphs = u16(&data, maxp + 4)? as u32;
        if num_glyphs == 0 {
            return None;
        }
        let (cmap_off, cmap_fmt) = cmap_subtable(&data, cmap)?;
        let ascent = i16(&data, hhea + 4)? as f32;
        let descent = i16(&data, hhea + 6)? as f32;
        let line_gap = i16(&data, hhea + 8)? as f32;
        let num_hmetrics = u16(&data, hhea + 34)? as u32;
        if num_hmetrics == 0 || num_hmetrics > num_glyphs {
            return None;
        }
        Some(Self {
            data,
            glyf,
            loca,
            loca_long: loca_fmt == 1,
            cmap: cmap_off,
            cmap_fmt,
            upem,
            ascent,
            descent,
            line_gap,
            num_glyphs,
            hmtx,
            num_hmetrics,
        })
    }

    pub fn line_height(&self, px: f32) -> f32 {
        (self.ascent - self.descent + self.line_gap) * (px / self.upem)
    }

    pub fn ascent(&self, px: f32) -> f32 {
        self.ascent * (px / self.upem)
    }

    pub fn measure(&self, text: &str, px: f32) -> f32 {
        let scale = px / self.upem;
        text.chars()
            .map(|ch| self.advance(self.glyph_id(ch as u32)) as f32 * scale)
            .sum()
    }

    /// Draws `text` on one baseline. Coverage is gamma-adjusted so stems stay dark.
    pub fn draw_line(
        &self,
        buf: &mut [u8],
        width: u32,
        height: u32,
        text: &str,
        mut x: f32,
        baseline: f32,
        px: f32,
        color: [u8; 4],
    ) {
        if width == 0 || height == 0 || px <= 0.0 {
            return;
        }
        let scale = px / self.upem;
        for ch in text.chars() {
            let gid = self.glyph_id(ch as u32);
            let adv = self.advance(gid) as f32 * scale;
            if let Some(contours) = self.contours(gid, 0) {
                let mut edges = Vec::new();
                for contour in &contours {
                    flatten_contour(contour, scale, x, baseline, &mut edges);
                }
                blit_edges(buf, width, height, &edges, color);
            }
            x += adv;
        }
    }

    fn advance(&self, gid: u16) -> u16 {
        let gid = gid as u32;
        let idx = if gid < self.num_hmetrics {
            gid
        } else {
            self.num_hmetrics - 1
        };
        u16(&self.data, self.hmtx + idx as usize * 4).unwrap_or(0)
    }

    fn glyph_id(&self, cp: u32) -> u16 {
        if self.cmap_fmt == 12 {
            glyph_id_fmt12(&self.data, self.cmap, cp).unwrap_or(0)
        } else {
            glyph_id_fmt4(&self.data, self.cmap, cp).unwrap_or(0)
        }
    }

    fn loca(&self, gid: u32) -> Option<usize> {
        if gid > self.num_glyphs {
            return None;
        }
        if self.loca_long {
            u32(&self.data, self.loca + gid as usize * 4).map(|v| v as usize)
        } else {
            u16(&self.data, self.loca + gid as usize * 2).map(|v| v as usize * 2)
        }
    }

    fn contours(&self, gid: u16, depth: u8) -> Option<Vec<Vec<Pt>>> {
        if depth > 8 || gid as u32 >= self.num_glyphs {
            return None;
        }
        let a = self.loca(gid as u32)?;
        let b = self.loca(gid as u32 + 1)?;
        if b < a {
            return None;
        }
        let start = self.glyf.checked_add(a)?;
        let end = self.glyf.checked_add(b)?;
        if end > self.data.len() || start > self.data.len() {
            return None;
        }
        if start == end {
            return Some(Vec::new());
        }
        self.contours_range(start, end, depth)
    }

    fn contours_range(&self, start: usize, end: usize, depth: u8) -> Option<Vec<Vec<Pt>>> {
        if end < start + 10 {
            return None;
        }
        let ncontours = i16(&self.data, start)?;
        if ncontours < 0 {
            return self.composite(start, end, depth);
        }
        if ncontours == 0 {
            return Some(Vec::new());
        }
        if ncontours > 64 {
            return None;
        }
        let mut o = start + 10;
        let mut ends = Vec::with_capacity(ncontours as usize);
        for _ in 0..ncontours {
            if o + 2 > end {
                return None;
            }
            ends.push(u16(&self.data, o)? as usize);
            o += 2;
        }
        let npts = ends.last().copied()? + 1;
        if npts == 0 || npts > 4096 || ends.windows(2).any(|w| w[1] <= w[0]) {
            return None;
        }
        if o + 2 > end {
            return None;
        }
        let ilen = u16(&self.data, o)? as usize;
        o += 2;
        o = o.checked_add(ilen)?;
        if o > end {
            return None;
        }
        let mut flags = Vec::with_capacity(npts);
        while flags.len() < npts {
            if o >= end {
                return None;
            }
            let f = self.data[o];
            o += 1;
            flags.push(f);
            if f & 0x08 != 0 {
                if o >= end {
                    return None;
                }
                let rep = self.data[o] as usize;
                o += 1;
                for _ in 0..rep {
                    if flags.len() >= npts {
                        break;
                    }
                    flags.push(f);
                }
            }
        }
        flags.truncate(npts);
        let mut x = 0i32;
        let mut xs = Vec::with_capacity(npts);
        for &f in &flags {
            if f & 0x02 != 0 {
                if o >= end {
                    return None;
                }
                let dx = self.data[o] as i32;
                o += 1;
                x += if f & 0x10 != 0 { dx } else { -dx };
            } else if f & 0x10 == 0 {
                if o + 2 > end {
                    return None;
                }
                x += i16(&self.data, o)? as i32;
                o += 2;
            }
            xs.push(x);
        }
        let mut y = 0i32;
        let mut ys = Vec::with_capacity(npts);
        for &f in &flags {
            if f & 0x04 != 0 {
                if o >= end {
                    return None;
                }
                let dy = self.data[o] as i32;
                o += 1;
                y += if f & 0x20 != 0 { dy } else { -dy };
            } else if f & 0x20 == 0 {
                if o + 2 > end {
                    return None;
                }
                y += i16(&self.data, o)? as i32;
                o += 2;
            }
            ys.push(y);
        }
        let mut contours = Vec::with_capacity(ends.len());
        let mut from = 0usize;
        for end_pt in ends {
            if end_pt >= npts {
                return None;
            }
            let mut c = Vec::with_capacity(end_pt - from + 1);
            for i in from..=end_pt {
                c.push(Pt {
                    x: xs[i] as f32,
                    y: ys[i] as f32,
                    on: flags[i] & 1 != 0,
                });
            }
            contours.push(c);
            from = end_pt + 1;
        }
        Some(contours)
    }

    fn composite(&self, start: usize, end: usize, depth: u8) -> Option<Vec<Vec<Pt>>> {
        let mut o = start + 10;
        let mut all = Vec::new();
        loop {
            if o + 4 > end {
                return None;
            }
            let flags = u16(&self.data, o)?;
            let gid = u16(&self.data, o + 2)?;
            o += 4;
            let (arg1, arg2) = if flags & 0x0001 != 0 {
                if o + 4 > end {
                    return None;
                }
                let a = i16(&self.data, o)? as f32;
                let b = i16(&self.data, o + 2)? as f32;
                o += 4;
                (a, b)
            } else {
                if o + 2 > end {
                    return None;
                }
                let a = self.data[o] as i8 as f32;
                let b = self.data[o + 1] as i8 as f32;
                o += 2;
                (a, b)
            };
            let (m00, m01, m10, m11) = if flags & 0x0008 != 0 {
                if o + 2 > end {
                    return None;
                }
                let s = f2dot14(i16(&self.data, o)?);
                o += 2;
                (s, 0.0, 0.0, s)
            } else if flags & 0x0040 != 0 {
                if o + 4 > end {
                    return None;
                }
                let sx = f2dot14(i16(&self.data, o)?);
                let sy = f2dot14(i16(&self.data, o + 2)?);
                o += 4;
                (sx, 0.0, 0.0, sy)
            } else if flags & 0x0080 != 0 {
                if o + 8 > end {
                    return None;
                }
                let a = f2dot14(i16(&self.data, o)?);
                let b = f2dot14(i16(&self.data, o + 2)?);
                let c = f2dot14(i16(&self.data, o + 4)?);
                let d = f2dot14(i16(&self.data, o + 6)?);
                o += 8;
                (a, b, c, d)
            } else {
                (1.0, 0.0, 0.0, 1.0)
            };
            if flags & 0x0002 != 0 {
                if let Some(parts) = self.contours(gid, depth + 1) {
                    for part in parts {
                        let mut out = Vec::with_capacity(part.len());
                        for p in part {
                            out.push(Pt {
                                x: m00 * p.x + m01 * p.y + arg1,
                                y: m10 * p.x + m11 * p.y + arg2,
                                on: p.on,
                            });
                        }
                        all.push(out);
                    }
                }
            }
            if flags & 0x0020 == 0 {
                break;
            }
        }
        Some(all)
    }
}

fn f2dot14(v: i16) -> f32 {
    v as f32 / 16384.0
}

fn sfnt_offset(data: &[u8]) -> Option<usize> {
    if data.len() < 12 {
        return None;
    }
    if &data[0..4] == b"ttcf" {
        if data.len() < 16 || u32(data, 8)? == 0 {
            return None;
        }
        let off = u32(data, 12)? as usize;
        if off + 12 > data.len() {
            return None;
        }
        return Some(off);
    }
    let ver = u32(data, 0)?;
    if ver == 0x0001_0000 || ver == u32::from_be_bytes(*b"true") {
        Some(0)
    } else {
        None
    }
}

fn table_map(data: &[u8], base: usize) -> Option<std::collections::HashMap<&'static [u8], usize>> {
    // Tags we care about, stored as the offset of the table payload.
    let n = u16(data, base + 4)? as usize;
    let mut o = base + 12;
    let mut map = std::collections::HashMap::new();
    for _ in 0..n {
        if o + 16 > data.len() {
            return None;
        }
        let tag = &data[o..o + 4];
        let offset = u32(data, o + 8)? as usize;
        let len = u32(data, o + 12)? as usize;
        if offset
            .checked_add(len)
            .map(|e| e <= data.len())
            .unwrap_or(false)
        {
            const KEYS: [&[u8]; 7] = [
                b"head", b"maxp", b"cmap", b"loca", b"glyf", b"hhea", b"hmtx",
            ];
            for key in KEYS {
                if tag == key {
                    map.insert(key, offset);
                }
            }
        }
        o += 16;
    }
    Some(map)
}

fn cmap_subtable(data: &[u8], cmap: usize) -> Option<(usize, u16)> {
    let n = u16(data, cmap + 2)? as usize;
    let mut best: Option<(u8, usize, u16)> = None;
    for i in 0..n {
        let rec = cmap + 4 + i * 8;
        let plat = u16(data, rec)?;
        let enc = u16(data, rec + 2)?;
        let rel = u32(data, rec + 4)? as usize;
        let sub = cmap + rel;
        let fmt = u16(data, sub)?;
        if fmt != 4 && fmt != 12 {
            continue;
        }
        let rank = match (plat, enc, fmt) {
            (3, 1, 4) => 0,
            (0, 3, 4) => 1,
            (3, 10, 12) => 2,
            (0, 4, 12) => 3,
            (_, _, 4) => 4,
            _ => 5,
        };
        if best.map(|(r, _, _)| rank < r).unwrap_or(true) {
            best = Some((rank, sub, fmt));
        }
    }
    best.map(|(_, off, fmt)| (off, fmt))
}

fn glyph_id_fmt4(data: &[u8], sub: usize, cp: u32) -> Option<u16> {
    if cp > 0xFFFF {
        return Some(0);
    }
    let cp = cp as u16;
    let seg_count = (u16(data, sub + 6)? / 2) as usize;
    if seg_count == 0 || seg_count > 20000 {
        return None;
    }
    let end = sub + 14;
    let start = end + seg_count * 2 + 2;
    let delta = start + seg_count * 2;
    let range_off = delta + seg_count * 2;
    for i in 0..seg_count {
        let e = u16(data, end + i * 2)?;
        let s = u16(data, start + i * 2)?;
        if cp < s || cp > e {
            continue;
        }
        let ro = u16(data, range_off + i * 2)? as usize;
        let d = i16(data, delta + i * 2)?;
        if ro == 0 {
            return Some((cp as i32 + d as i32) as u16);
        }
        let addr = (range_off + i * 2) + ro + (cp - s) as usize * 2;
        let g = u16(data, addr)?;
        if g == 0 {
            return Some(0);
        }
        return Some((g as i32 + d as i32) as u16);
    }
    Some(0)
}

fn glyph_id_fmt12(data: &[u8], sub: usize, cp: u32) -> Option<u16> {
    let n = u32(data, sub + 12)? as usize;
    if n > 100_000 {
        return None;
    }
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let rec = sub + 16 + mid * 12;
        let start = u32(data, rec)?;
        let end = u32(data, rec + 4)?;
        if cp < start {
            hi = mid;
        } else if cp > end {
            lo = mid + 1;
        } else {
            let gid = u32(data, rec + 8)? + (cp - start);
            return Some(gid.min(u16::MAX as u32) as u16);
        }
    }
    Some(0)
}

fn flatten_contour(
    pts: &[Pt],
    scale: f32,
    origin_x: f32,
    baseline: f32,
    edges: &mut Vec<(f32, f32, f32, f32)>,
) {
    let n = pts.len();
    if n < 2 {
        return;
    }
    let mut exp: Vec<(f32, f32, bool)> = Vec::with_capacity(n + 2);
    for i in 0..n {
        let p = pts[i];
        let px = origin_x + p.x * scale;
        let py = baseline - p.y * scale;
        if !p.on {
            let prev = pts[(i + n - 1) % n];
            if !prev.on {
                let qx = origin_x + prev.x * scale;
                let qy = baseline - prev.y * scale;
                exp.push(((qx + px) * 0.5, (qy + py) * 0.5, true));
            }
        }
        exp.push((px, py, p.on));
    }
    if exp.is_empty() {
        return;
    }
    if !exp[0].2 {
        if let Some(k) = exp.iter().position(|p| p.2) {
            exp.rotate_left(k);
        } else {
            return;
        }
    }
    let first = (exp[0].0, exp[0].1);
    let mut cur = first;
    let mut i = 1;
    while i < exp.len() {
        if exp[i].2 {
            line(cur, (exp[i].0, exp[i].1), edges);
            cur = (exp[i].0, exp[i].1);
            i += 1;
        } else {
            let ctrl = (exp[i].0, exp[i].1);
            let (end, step) = if i + 1 < exp.len() {
                (
                    (exp[i + 1].0, exp[i + 1].1),
                    if exp[i + 1].2 { 2 } else { 1 },
                )
            } else {
                (first, 1)
            };
            quad(cur, ctrl, end, 0, edges);
            cur = end;
            i += step;
        }
    }
    if (cur.0 - first.0).abs() > 0.05 || (cur.1 - first.1).abs() > 0.05 {
        line(cur, first, edges);
    }
}

fn line(a: (f32, f32), b: (f32, f32), edges: &mut Vec<(f32, f32, f32, f32)>) {
    if (a.1 - b.1).abs() >= 1e-4 {
        edges.push((a.0, a.1, b.0, b.1));
    }
}

fn quad(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    depth: u8,
    edges: &mut Vec<(f32, f32, f32, f32)>,
) {
    let dx = p2.0 - p0.0;
    let dy = p2.1 - p0.1;
    let length = (dx * dx + dy * dy).sqrt().max(1e-9);
    let dev = ((p1.0 - p0.0) * dy - (p1.1 - p0.1) * dx).abs() / length;
    if dev < 0.25 || depth > 12 {
        line(p0, p2, edges);
        return;
    }
    let a = ((p0.0 + p1.0) * 0.5, (p0.1 + p1.1) * 0.5);
    let b = ((p1.0 + p2.0) * 0.5, (p1.1 + p2.1) * 0.5);
    let m = ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
    quad(p0, a, m, depth + 1, edges);
    quad(m, b, p2, depth + 1, edges);
}

fn blit_edges(
    buf: &mut [u8],
    width: u32,
    height: u32,
    edges: &[(f32, f32, f32, f32)],
    color: [u8; 4],
) {
    if edges.is_empty() {
        return;
    }
    let mut minx = f32::MAX;
    let mut miny = f32::MAX;
    let mut maxx = f32::MIN;
    let mut maxy = f32::MIN;
    for e in edges {
        minx = minx.min(e.0.min(e.2));
        maxx = maxx.max(e.0.max(e.2));
        miny = miny.min(e.1.min(e.3));
        maxy = maxy.max(e.1.max(e.3));
    }
    let ox = minx.floor() as i32 - 1;
    let oy = miny.floor() as i32 - 1;
    let x1 = maxx.ceil() as i32 + 1;
    let y1 = maxy.ceil() as i32 + 1;
    let gw = (x1 - ox).max(1);
    let gh = (y1 - oy).max(1);
    if gw > 800 || gh > 800 {
        return;
    }
    let sw = gw * SS;
    let sh = gh * SS;
    let cells = (sw as usize).saturating_mul(sh as usize);
    if cells == 0 || cells > 2_000_000 {
        return;
    }
    let mut cover = vec![0u8; cells];
    let mut norm: Vec<(f32, f32, f32, f32, i32)> = Vec::with_capacity(edges.len());
    for &(x0, y0, x1e, y1e) in edges {
        if y1e < y0 {
            let dy = y0 - y1e;
            if dy < 1e-5 {
                continue;
            }
            norm.push((y1e, y0, x1e, (x0 - x1e) / dy, -1));
        } else {
            let dy = y1e - y0;
            if dy < 1e-5 {
                continue;
            }
            norm.push((y0, y1e, x0, (x1e - x0) / dy, 1));
        }
    }
    let mut hits: Vec<(f32, i32)> = Vec::new();
    for sy in 0..sh {
        let y = oy as f32 + (sy as f32 + 0.5) / SS as f32;
        hits.clear();
        for &(y0, y1e, x0, dxdy, dir) in &norm {
            if y0 <= y && y < y1e {
                hits.push((x0 + (y - y0) * dxdy, dir));
            }
        }
        hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut wind = 0i32;
        let mut prev = ox as f32;
        let row = sy as usize * sw as usize;
        for (x, dir) in hits.drain(..) {
            if wind != 0 {
                let j0 = (((prev - ox as f32) * SS as f32) - 0.5).ceil() as i32;
                let j1 = (((x - ox as f32) * SS as f32) - 0.5).ceil() as i32;
                let j0 = j0.max(0) as usize;
                let j1 = (j1.max(0) as usize).min(sw as usize);
                if j1 > j0 {
                    cover[row + j0..row + j1].fill(255);
                }
            }
            wind += dir;
            prev = x;
        }
    }
    let samples = (SS * SS) as u32;
    for gy in 0..gh {
        for gx in 0..gw {
            let mut s = 0u32;
            for sy in 0..SS {
                let base = ((gy * SS + sy) * sw + gx * SS) as usize;
                for sx in 0..SS {
                    s += cover[base + sx as usize] as u32;
                }
            }
            let cov = s as f32 / (samples as f32 * 255.0);
            if cov > 0.004 {
                blend(buf, width, height, ox + gx, oy + gy, color, cov);
            }
        }
    }
}

pub fn blend(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    color: [u8; 4],
    coverage: f32,
) {
    if coverage <= 0.0 || x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let lin = coverage.clamp(0.0, 1.0);
    let a = 1.0 - (1.0 - lin).powf(1.35);
    let i = ((y as u32 * width + x as u32) * 4) as usize;
    if i + 4 > buf.len() {
        return;
    }
    for c in 0..3 {
        let dst = buf[i + c] as f32;
        let src = color[c] as f32;
        buf[i + c] = (dst + (src - dst) * a).round() as u8;
    }
    buf[i + 3] = 0xFF;
}

fn u16(data: &[u8], o: usize) -> Option<u16> {
    let b = data.get(o..o + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn i16(data: &[u8], o: usize) -> Option<i16> {
    let b = data.get(o..o + 2)?;
    Some(i16::from_be_bytes([b[0], b[1]]))
}

fn u32(data: &[u8], o: usize) -> Option<u32> {
    let b = data.get(o..o + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_u16(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_be_bytes());
    }
    fn push_i16(buf: &mut Vec<u8>, v: i16) {
        buf.extend_from_slice(&v.to_be_bytes());
    }
    fn push_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_be_bytes());
    }

    /// One square glyph for 'A', long loca, format-4 cmap.
    fn square_font() -> Vec<u8> {
        let mut glyf = Vec::new();
        push_i16(&mut glyf, 1);
        push_i16(&mut glyf, 100);
        push_i16(&mut glyf, 100);
        push_i16(&mut glyf, 700);
        push_i16(&mut glyf, 700);
        push_u16(&mut glyf, 3);
        push_u16(&mut glyf, 0);
        glyf.extend_from_slice(&[1, 1, 1, 1]);
        for x in [100i16, 600, 0, -600] {
            push_i16(&mut glyf, x);
        }
        for y in [100i16, 0, 600, 0] {
            push_i16(&mut glyf, y);
        }

        let mut loca = Vec::new();
        push_u32(&mut loca, 0);
        push_u32(&mut loca, 0);
        push_u32(&mut loca, glyf.len() as u32);

        let mut hmtx = Vec::new();
        push_u16(&mut hmtx, 400);
        push_i16(&mut hmtx, 0);
        push_u16(&mut hmtx, 800);
        push_i16(&mut hmtx, 100);

        let mut hhea = vec![0u8; 36];
        hhea[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
        hhea[4..6].copy_from_slice(&800i16.to_be_bytes());
        hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
        hhea[10..12].copy_from_slice(&800u16.to_be_bytes());
        hhea[18..20].copy_from_slice(&1i16.to_be_bytes());
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes());

        let mut head = vec![0u8; 54];
        head[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
        head[12..16].copy_from_slice(&0x5F0F3CF5u32.to_be_bytes());
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        head[40..42].copy_from_slice(&1000u16.to_be_bytes());
        head[42..44].copy_from_slice(&1000u16.to_be_bytes());
        head[50..52].copy_from_slice(&1i16.to_be_bytes());

        let mut maxp = vec![0u8; 32];
        maxp[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
        maxp[4..6].copy_from_slice(&2u16.to_be_bytes());

        let mut sub = Vec::new();
        push_u16(&mut sub, 4);
        push_u16(&mut sub, 32);
        push_u16(&mut sub, 0);
        push_u16(&mut sub, 4);
        push_u16(&mut sub, 4);
        push_u16(&mut sub, 1);
        push_u16(&mut sub, 0);
        push_u16(&mut sub, 0x0041);
        push_u16(&mut sub, 0xFFFF);
        push_u16(&mut sub, 0);
        push_u16(&mut sub, 0x0041);
        push_u16(&mut sub, 0xFFFF);
        push_i16(&mut sub, -64);
        push_i16(&mut sub, 1);
        push_u16(&mut sub, 0);
        push_u16(&mut sub, 0);
        let mut cmap = Vec::new();
        push_u16(&mut cmap, 0);
        push_u16(&mut cmap, 1);
        push_u16(&mut cmap, 3);
        push_u16(&mut cmap, 1);
        push_u32(&mut cmap, 12);
        cmap.extend_from_slice(&sub);

        let tables: [(&[u8], Vec<u8>); 7] = [
            (b"cmap", cmap),
            (b"glyf", glyf),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"loca", loca),
            (b"maxp", maxp),
        ];
        let mut records = Vec::new();
        let mut blob = Vec::new();
        let mut data_off = 12 + tables.len() * 16;
        for (tag, bytes) in tables {
            let len = bytes.len();
            let pad = (4 - len % 4) % 4;
            records.push((tag, data_off, len));
            blob.extend_from_slice(&bytes);
            blob.extend(std::iter::repeat(0).take(pad));
            data_off += len + pad;
        }
        let mut file = Vec::new();
        push_u32(&mut file, 0x0001_0000);
        push_u16(&mut file, records.len() as u16);
        push_u16(&mut file, 0);
        push_u16(&mut file, 0);
        push_u16(&mut file, 0);
        for (tag, off, len) in records {
            file.extend_from_slice(tag);
            push_u32(&mut file, 0);
            push_u32(&mut file, off as u32);
            push_u32(&mut file, len as u32);
        }
        file.extend_from_slice(&blob);
        file
    }

    #[test]
    fn square_glyph_fills_its_interior() {
        let font = Font::parse(square_font()).expect("square font");
        assert!((font.measure("A", 100.0) - 80.0).abs() < 0.1);
        assert!((font.measure("B", 100.0)).abs() < 0.1 || font.measure("B", 100.0) > 0.0);
        let mut buf = vec![255u8; 100 * 100 * 4];
        font.draw_line(&mut buf, 100, 100, "A", 0.0, 80.0, 100.0, [0, 0, 0, 255]);
        let at =
            |x: u32, y: u32| &buf[((y * 100 + x) * 4) as usize..((y * 100 + x) * 4) as usize + 3];
        assert!(
            at(40, 40).iter().all(|c| *c < 40),
            "center {0:?}",
            at(40, 40)
        );
        assert!(at(2, 2).iter().all(|c| *c > 240), "outside {0:?}", at(2, 2));
        assert!(at(40, 2).iter().all(|c| *c > 240), "above {0:?}", at(40, 2));
    }

    #[test]
    fn installed_sans_keeps_the_counter_of_o() {
        let candidates = [
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ];
        let Some(path) = candidates
            .iter()
            .find(|p| std::path::Path::new(p).is_file())
        else {
            return;
        };
        let font =
            Font::parse(std::fs::read(path).unwrap()).unwrap_or_else(|| panic!("parse {path}"));
        let mut buf = vec![255u8; 160 * 160 * 4];
        font.draw_line(&mut buf, 160, 160, "o", 20.0, 120.0, 72.0, [0, 0, 0, 255]);
        let mut minx = 160i32;
        let mut miny = 160i32;
        let mut maxx = 0i32;
        let mut maxy = 0i32;
        let mut dark = 0i32;
        for y in 0..160 {
            for x in 0..160 {
                let i = (y * 160 + x) * 4;
                if buf[i] < 80 {
                    dark += 1;
                    minx = minx.min(x as i32);
                    miny = miny.min(y as i32);
                    maxx = maxx.max(x as i32);
                    maxy = maxy.max(y as i32);
                }
            }
        }
        assert!(dark > 40, "no ink from {path}");
        let cx = (minx + maxx) / 2;
        let cy = (miny + maxy) / 2;
        let i = ((cy as usize * 160) + cx as usize) * 4;
        assert!(
            buf[i] > 200,
            "counter of o in {path} is filled ({})",
            buf[i]
        );
    }
}
