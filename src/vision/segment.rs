use crate::domain::Rect;

pub fn dilate(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<u8> {
    let mut out = vec![0; mask.len()];
    for y in 0..height {
        for x in 0..width {
            let mut hit = false;
            for yy in y.saturating_sub(radius)..=(y + radius).min(height - 1) {
                for xx in x.saturating_sub(radius)..=(x + radius).min(width - 1) {
                    if mask[yy * width + xx] != 0 {
                        hit = true;
                    }
                }
            }
            out[y * width + x] = u8::from(hit);
        }
    }
    out
}

pub fn map_probability_mask(
    prob: &[f32],
    pw: usize,
    ph: usize,
    width: usize,
    height: usize,
    threshold: f32,
    region: Option<Rect>,
) -> Vec<u8> {
    let mut out = vec![0; width * height];
    for y in 0..height {
        for x in 0..width {
            let px = ((x * pw) / width).min(pw - 1);
            let py = ((y * ph) / height).min(ph - 1);
            let inside = region
                .map(|r| {
                    (x as f32) >= r.x1
                        && (x as f32) < r.x2
                        && (y as f32) >= r.y1
                        && (y as f32) < r.y2
                })
                .unwrap_or(true);
            out[y * width + x] = u8::from(inside && prob[py * pw + px] > threshold);
        }
    }
    out
}
