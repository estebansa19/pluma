//! The Pluma icon, drawn in code (no image files). Shared by the app (window and tray icons)
//! and by `build.rs`, which embeds it in the executable.

fn in_round_rect(u: f32, v: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> bool {
    let dx = ((u - cx).abs() - (hx - r)).max(0.0);
    let dy = ((v - cy).abs() - (hy - r)).max(0.0);
    dx * dx + dy * dy <= r * r
}

fn icon_sample(u: f32, v: f32) -> Option<[f32; 3]> {
    if !in_round_rect(u, v, 0.5, 0.5, 0.47, 0.47, 0.24) {
        return None;
    }
    let mouse = in_round_rect(u, v, 0.5, 0.54, 0.19, 0.29, 0.19);
    let split = (u - 0.5).abs() < 0.035 && v > 0.2 && v < 0.47;
    if mouse && !split {
        return Some([255.0, 255.0, 255.0]);
    }
    // blue → lavender gradient
    let t = ((u + v) * 0.5).clamp(0.0, 1.0);
    let a = [0x7A as f32, 0xA2 as f32, 0xF7 as f32];
    let b = [0xBB as f32, 0x9A as f32, 0xF7 as f32];
    Some([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t])
}

/// RGBA icon (not premultiplied) of `size`×`size` with 4×4 antialiasing.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (size * size * 4) as usize];
    const SS: u32 = 4;
    for y in 0..size {
        for x in 0..size {
            let (mut r, mut g, mut b, mut n) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if let Some(c) = icon_sample(u, v) {
                        r += c[0];
                        g += c[1];
                        b += c[2];
                        n += 1.0;
                    }
                }
            }
            let i = ((y * size + x) * 4) as usize;
            if n > 0.0 {
                out[i] = (r / n) as u8;
                out[i + 1] = (g / n) as u8;
                out[i + 2] = (b / n) as u8;
                out[i + 3] = (n / (SS * SS) as f32 * 255.0).round() as u8;
            }
        }
    }
    out
}
