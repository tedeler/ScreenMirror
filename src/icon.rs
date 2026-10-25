//! Tray icon pixmaps drawn in code (a small monitor whose screen reflects the mode).

use crate::dbus::Value;
use crate::state::Mode;

type Rgba = [f32; 4];

fn inside(u: f32, v: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> bool {
    u >= x0 && u < x1 && v >= y0 && v < y1
}

fn sample(mode: &Mode, u: f32, v: f32) -> Option<Rgba> {
    let frame = [0.43, 0.48, 0.51, 1.0];
    let blue_top = [0.24, 0.68, 0.91, 1.0];
    let blue_bottom = [0.11, 0.47, 0.80, 1.0];
    if inside(u, v, 0.14, 0.20, 0.86, 0.66) {
        let t = (v - 0.20) / 0.46;
        let blue = [0, 1, 2].map(|i| blue_top[i] * (1.0 - t) + blue_bottom[i] * t);
        return Some(match mode {
            Mode::Black => [0.0, 0.0, 0.0, 1.0],
            Mode::Full => [blue[0], blue[1], blue[2], 1.0],
            Mode::Zoom(_) => {
                let outer = inside(u, v, 0.32, 0.30, 0.68, 0.56);
                let inner = inside(u, v, 0.37, 0.35, 0.63, 0.51);
                if outer && !inner {
                    [1.0, 1.0, 1.0, 1.0]
                } else {
                    [blue[0], blue[1], blue[2], 1.0]
                }
            }
        });
    }
    if inside(u, v, 0.06, 0.12, 0.94, 0.74)
        || inside(u, v, 0.44, 0.74, 0.56, 0.84)
        || inside(u, v, 0.26, 0.84, 0.74, 0.92)
    {
        return Some(frame);
    }
    None
}

/// One `(iiay)` pixmap in ARGB32 network byte order.
fn pixmap(mode: &Mode, size: i32) -> Value {
    const SS: i32 = 4;
    let mut bytes = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if let Some(c) = sample(mode, u, v) {
                        for i in 0..3 {
                            acc[i] += c[i] * c[3];
                        }
                        acc[3] += c[3];
                    }
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            let ch = |i: usize| if acc[3] > 0.0 { (acc[i] / acc[3] * 255.0).round() as u8 } else { 0 };
            bytes.extend_from_slice(&[(a * 255.0).round() as u8, ch(0), ch(1), ch(2)]);
        }
    }
    Value::Struct(vec![
        Value::I32(size),
        Value::I32(size),
        Value::Array("y".into(), bytes.into_iter().map(Value::Byte).collect()),
    ])
}

/// `a(iiay)` icon pixmaps at common tray sizes.
pub fn pixmaps(mode: &Mode) -> Value {
    Value::Array("(iiay)".into(), [16, 22, 24, 32, 48, 64].iter().map(|&s| pixmap(mode, s)).collect())
}
