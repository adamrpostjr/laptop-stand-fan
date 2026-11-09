//! The fan glyph, drawn in code (no image assets to ship): a ring, a hub and
//! four curved blades, 4× supersampled for clean edges. The colour carries
//! the state in the tray.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    /// Connected, following the curve.
    Ok,
    /// Connected, manual override or failsafe.
    Attention,
    /// A fan has stalled.
    Alarm,
    /// No service / not connected.
    Idle,
}

impl Tone {
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Tone::Ok => [0x3d, 0xc4, 0x8a],
            Tone::Attention => [0xf2, 0xb1, 0x3a],
            Tone::Alarm => [0xf0, 0x54, 0x4c],
            Tone::Idle => [0x9a, 0xa0, 0xa6],
        }
    }
}

pub fn rgba(size: u32, tone: Tone) -> Vec<u8> {
    const SS: u32 = 4;
    let [r, g, b] = tone.rgb();
    let s = size as f32;
    let c = s / 2.0;
    let outer = s * 0.48;
    let ring = s * 0.075;
    let hub = s * 0.11;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32 - c;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32 - c;
                    let d = (x * x + y * y).sqrt();
                    let a = y.atan2(x);
                    let in_ring = d <= outer && d >= outer - ring;
                    let in_hub = d <= hub;
                    // Blades: four lobes, twisted with radius so they curve.
                    let blade_zone = d > hub && d < outer - ring * 1.6;
                    let twist = a + d / s * 2.2;
                    let in_blade = blade_zone && (4.0 * twist).sin() > 0.25;
                    if in_ring || in_hub || in_blade {
                        hits += 1;
                    }
                }
            }
            let alpha = (hits * 255 / (SS * SS)) as u8;
            out.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    out
}

/// A multi-size .ico (32-bit DIB entries) for the installer and shortcuts.
pub fn ico(tone: Tone) -> Vec<u8> {
    const SIZES: [u32; 5] = [16, 24, 32, 48, 64];
    let images: Vec<Vec<u8>> = SIZES.iter().map(|&s| dib(s, &rgba(s, tone))).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]); // reserved, type = icon
    out.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (&s, img) in SIZES.iter().zip(&images) {
        out.extend_from_slice(&[s as u8, s as u8, 0, 0]); // w, h, colours, reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

/// BITMAPINFOHEADER + bottom-up BGRA rows + an all-zero AND mask.
fn dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(2 * size as i32).to_le_bytes()); // XOR + AND
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]); // compression .. important colours
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.resize(out.len() + (mask_row * size) as usize, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ico_layout() {
        let f = ico(Tone::Ok);
        assert_eq!(&f[..6], &[0, 0, 1, 0, 5, 0]);
        // First entry points just past the directory.
        assert_eq!(u32::from_le_bytes(f[18..22].try_into().unwrap()), 6 + 16 * 5);
        // 16 px entry: header + pixels + mask.
        assert_eq!(u32::from_le_bytes(f[14..18].try_into().unwrap()), 40 + 16 * 16 * 4 + 4 * 16);
    }

    #[test]
    fn sizes_and_coverage() {
        let px = rgba(32, Tone::Ok);
        assert_eq!(px.len(), 32 * 32 * 4);
        let opaque = px.chunks(4).filter(|p| p[3] > 128).count();
        // A recognisable glyph: neither empty nor a solid square.
        assert!((150..700).contains(&opaque), "{opaque}");
        assert_eq!(px[3], 0, "corner is transparent");
    }
}
