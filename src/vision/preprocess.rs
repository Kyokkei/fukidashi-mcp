use image::{DynamicImage, RgbImage, imageops::FilterType};

use crate::error::{FukidashiError, Result};

pub fn detector_tensor(image: &RgbImage) -> (Vec<f32>, [i64; 2]) {
    // Upstream uses PIL's bicubic resize. CatmullRom is image-rs' close equivalent.
    let resized = DynamicImage::ImageRgb8(image.clone())
        .resize_exact(640, 640, FilterType::CatmullRom)
        .to_rgb8();
    let mut out = vec![0.0; 3 * 640 * 640];
    for (y, row) in resized.rows().enumerate() {
        for (x, px) in row.enumerate() {
            let i = y * 640 + x;
            out[i] = f32::from(px[0]) / 255.0;
            out[640 * 640 + i] = f32::from(px[1]) / 255.0;
            out[2 * 640 * 640 + i] = f32::from(px[2]) / 255.0;
        }
    }
    (out, [i64::from(image.width()), i64::from(image.height())])
}

pub fn round_ties_even(value: f64) -> i64 {
    // Rust's f32::round is ties-away-from-zero; Python round used by upstream is
    // ties-to-even. Work in f64 to make the parity check stable at stride boundaries.
    let v = value;
    let floor = v.floor();
    let fraction = v - floor;
    if (fraction - 0.5).abs() < f64::EPSILON {
        if (floor as i64) % 2 == 0 {
            floor as i64
        } else {
            floor as i64 + 1
        }
    } else {
        v.round() as i64
    }
}

pub fn db_dimensions(
    height: u32,
    width: u32,
    limit: u32,
    limit_type: &str,
) -> Result<(u32, u32, f32, f32)> {
    if height == 0 || width == 0 {
        return Err(FukidashiError::InvalidInput(
            "image dimensions must be nonzero".into(),
        ));
    }
    let ratio: f64 = if limit_type == "max" {
        if height.max(width) > limit {
            f64::from(limit) / f64::from(height.max(width))
        } else {
            1.0
        }
    } else {
        if height.min(width) < limit {
            f64::from(limit) / f64::from(height.min(width))
        } else {
            1.0
        }
    };
    let h = (32 * round_ties_even(f64::from(height) * ratio / 32.0).max(1)) as u32;
    let w = (32 * round_ties_even(f64::from(width) * ratio / 32.0).max(1)) as u32;
    Ok((h, w, width as f32 / w as f32, height as f32 / h as f32))
}

pub fn ocr_letterbox(image: &RgbImage) -> Result<Vec<f32>> {
    if image.width() == 0 || image.height() == 0 {
        return Err(FukidashiError::InvalidInput(
            "OCR crop dimensions must be nonzero".into(),
        ));
    }
    let scale = (224.0 / image.width() as f32).min(224.0 / image.height() as f32);
    let width = ((image.width() as f32 * scale).floor() as u32).max(1);
    let height = ((image.height() as f32 * scale).floor() as u32).max(1);
    let resized = DynamicImage::ImageRgb8(image.clone())
        .resize_exact(width, height, FilterType::Triangle)
        .to_rgb8();
    let mut out = vec![1.0; 3 * 224 * 224];
    let ox = ((224 - width) / 2) as usize;
    let oy = ((224 - height) / 2) as usize;
    for (y, row) in resized.rows().enumerate() {
        for (x, px) in row.enumerate() {
            let i = (oy + y) * 224 + ox + x;
            out[i] = px[0] as f32 / 255.0;
            out[224 * 224 + i] = px[1] as f32 / 255.0;
            out[2 * 224 * 224 + i] = px[2] as f32 / 255.0;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detector_is_rgb_nchw_and_wh() {
        let mut img = RgbImage::new(1, 2);
        img.put_pixel(0, 0, image::Rgb([255, 0, 1]));
        let (x, size) = detector_tensor(&img);
        assert_eq!(size, [1, 2]);
        assert_eq!(x[0], 1.0);
        assert_eq!(x[640 * 640], 0.0);
        assert!((x[2 * 640 * 640] - 1.0 / 255.0).abs() < 1e-6);
    }
    #[test]
    fn ties_even_and_stride() {
        assert_eq!(round_ties_even(2.5), 2);
        assert_eq!(round_ties_even(3.5), 4);
        assert_eq!(round_ties_even(4.5), 4);
        assert_eq!(db_dimensions(100, 100, 960, "min").unwrap().0 % 32, 0);
    }
}
