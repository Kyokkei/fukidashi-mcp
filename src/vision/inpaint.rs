use crate::error::{FukidashiError, Result};

pub fn modulo_padding(height: usize, width: usize, modulo: usize) -> (usize, usize) {
    (
        (modulo - height % modulo) % modulo,
        (modulo - width % modulo) % modulo,
    )
}

pub fn compose(original: &[u8], generated: &[u8], mask: &[u8], channels: usize) -> Result<Vec<u8>> {
    if original.len() != generated.len() || mask.len() * channels != original.len() {
        return Err(FukidashiError::TensorContract(
            "inpainting output or mask size mismatch".into(),
        ));
    }
    let mut out = original.to_vec();
    for (i, p) in mask.iter().enumerate() {
        if *p != 0 {
            for c in 0..channels {
                out[i * channels + c] = generated[i * channels + c];
            }
        }
    }
    Ok(out)
}

#[cfg(feature = "onnx")]
use image::{GrayImage, RgbImage};

#[cfg(feature = "onnx")]
const MAX_IMAGE_PIXELS: u64 = 64_000_000;

#[cfg(feature = "onnx")]
pub fn mask_from_text_regions(
    width: u32,
    height: u32,
    image: &RgbImage,
    regions: &[crate::domain::Rect],
    kernel: u8,
) -> Result<GrayImage> {
    if kernel == 0 || kernel > 15 || kernel.is_multiple_of(2) {
        return Err(FukidashiError::InvalidInput(
            "dilation must be an odd kernel size from 1 to 15".into(),
        ));
    }
    if image.width() != width || image.height() != height {
        return Err(FukidashiError::InvalidInput(
            "text-mask image dimensions do not match requested dimensions".into(),
        ));
    }
    let mut mask = vec![0u8; width as usize * height as usize];
    for region in regions {
        let Some(region) = region.clip(width as f32, height as f32) else {
            continue;
        };
        let x1 = region.x1.floor() as u32;
        let y1 = region.y1.floor() as u32;
        let x2 = region.x2.ceil().min(width as f32) as u32;
        let y2 = region.y2.ceil().min(height as f32) as u32;
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        let crop_width = (x2 - x1) as usize;
        let crop_height = (y2 - y1) as usize;
        let mut gray = vec![0u8; crop_width * crop_height];
        for y in 0..crop_height {
            for x in 0..crop_width {
                let pixel = image.get_pixel(x1 + x as u32, y1 + y as u32);
                gray[y * crop_width + x] = ((u32::from(pixel[0]) * 299
                    + u32::from(pixel[1]) * 587
                    + u32::from(pixel[2]) * 114)
                    / 1000) as u8;
            }
        }
        let threshold = otsu_threshold(&gray);
        let dark = gray
            .iter()
            .map(|&value| value <= threshold)
            .collect::<Vec<_>>();
        let light = gray
            .iter()
            .map(|&value| value > threshold)
            .collect::<Vec<_>>();
        let dark_count = dark.iter().filter(|&&value| value).count();
        let light_count = light.iter().filter(|&&value| value).count();
        let Some(candidate) = [(dark_count, dark), (light_count, light)]
            .into_iter()
            .filter(|(count, _)| *count > 0 && *count * 2 < gray.len())
            .min_by_key(|(count, _)| *count)
            .map(|(_, candidate)| candidate)
        else {
            continue;
        };
        let components = connected_components(&candidate, crop_width, crop_height);
        for component in components {
            let span = component.width.max(component.height);
            let density = component.area as f32 / (component.width * component.height) as f32;
            // Sparse long components are characteristic of balloon borders or
            // tails.  They may cross a detector box and must stay untouched.
            if span > 32 && density < 0.06 {
                continue;
            }
            for index in component.pixels {
                let x = index % crop_width;
                let y = index / crop_width;
                mask[(y1 as usize + y) * width as usize + x1 as usize + x] = 255;
            }
        }
    }
    dilate_mask(
        &GrayImage::from_raw(width, height, mask).expect("mask dimensions are allocated exactly"),
        kernel,
    )
}

/// Crop-mode mask builder. The additional scale-aware expansion covers
/// antialiased glyph fringes while retaining the component filters that reject
/// sparse balloon outlines and tails.
#[cfg(feature = "onnx")]
pub fn adaptive_mask_from_text_regions(
    width: u32,
    height: u32,
    image: &RgbImage,
    regions: &[crate::domain::Rect],
    requested_kernel: u8,
) -> Result<GrayImage> {
    let tallest = regions
        .iter()
        .map(|region| (region.y2 - region.y1).max(0.0))
        .fold(0.0_f32, f32::max);
    let adaptive = if tallest >= 32.0 {
        requested_kernel.max(5)
    } else {
        requested_kernel.max(3)
    };
    let adaptive = adaptive.min(15) | 1;
    let candidate = mask_from_text_regions(width, height, image, regions, adaptive)?;
    // Keep the original detector result as the anchor.  The larger crop-mode
    // kernel is only allowed to recover pixels in a tiny neighbourhood around
    // those seeds; a dark drawing that merely happens to be inside a broad
    // OCR rectangle cannot become an inpainting target by itself.
    let seed = mask_from_text_regions(width, height, image, regions, requested_kernel)?;
    restrict_mask_to_seed_neighborhood(&candidate, &seed, (adaptive / 2 + 1).min(7))
}

/// Intersect a candidate text mask with a small neighbourhood of the original
/// detector seed mask.  This is deliberately separate from dilation so the
/// invariant is easy to test: every output pixel must be close to a seed.
#[cfg(feature = "onnx")]
pub fn restrict_mask_to_seed_neighborhood(
    candidate: &GrayImage,
    seed: &GrayImage,
    radius: u8,
) -> Result<GrayImage> {
    if candidate.dimensions() != seed.dimensions() {
        return Err(FukidashiError::InvalidInput(
            "candidate and seed masks must have matching dimensions".into(),
        ));
    }
    let allowed = dilate_mask(seed, radius.saturating_mul(2).saturating_add(1))?;
    let mut output = candidate.clone();
    for (pixel, allowed_pixel) in output.pixels_mut().zip(allowed.pixels()) {
        if allowed_pixel[0] == 0 {
            pixel[0] = 0;
        }
    }
    Ok(output)
}

/// Build bounded, padded LaMa crops from text geometry, or from connected mask
/// components when geometry is unavailable. Overlapping crops are merged and
/// dimensions are expanded to the requested model stride.
#[cfg(feature = "onnx")]
pub fn crop_regions_from_mask(
    mask: &GrayImage,
    text_regions: &[crate::domain::Rect],
    padding: u32,
    minimum_size: u32,
    stride: u32,
) -> Result<Vec<crate::domain::Rect>> {
    if stride == 0 || minimum_size == 0 {
        return Err(FukidashiError::InvalidInput(
            "crop minimum size and stride must be positive".into(),
        ));
    }
    let mut seeds = text_regions
        .iter()
        .filter_map(|region| region.clip(mask.width() as f32, mask.height() as f32))
        .collect::<Vec<_>>();
    if seeds.is_empty() {
        let binary = mask.pixels().map(|pixel| pixel[0] != 0).collect::<Vec<_>>();
        seeds.extend(
            connected_components(&binary, mask.width() as usize, mask.height() as usize)
                .into_iter()
                .map(|component| crate::domain::Rect {
                    x1: component.min_x as f32,
                    y1: component.min_y as f32,
                    x2: (component.max_x + 1) as f32,
                    y2: (component.max_y + 1) as f32,
                }),
        );
    }
    let mut crops = seeds
        .into_iter()
        .map(|region| {
            expand_crop(
                region,
                mask.width(),
                mask.height(),
                padding,
                minimum_size,
                stride,
            )
        })
        .collect::<Vec<_>>();
    crops.sort_by(|a, b| a.y1.total_cmp(&b.y1).then_with(|| a.x1.total_cmp(&b.x1)));
    let mut merged: Vec<crate::domain::Rect> = Vec::new();
    for crop in crops {
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| rectangles_touch(**existing, crop))
        {
            *existing = expand_crop(
                crate::domain::Rect {
                    x1: existing.x1.min(crop.x1),
                    y1: existing.y1.min(crop.y1),
                    x2: existing.x2.max(crop.x2),
                    y2: existing.y2.max(crop.y2),
                },
                mask.width(),
                mask.height(),
                0,
                minimum_size,
                stride,
            );
        } else {
            merged.push(crop);
        }
    }
    Ok(merged)
}

#[cfg(feature = "onnx")]
pub fn composite_masked_crop(
    output: &mut RgbImage,
    generated: &RgbImage,
    crop_mask: &GrayImage,
    x: u32,
    y: u32,
) -> Result<()> {
    if generated.dimensions() != crop_mask.dimensions()
        || x.saturating_add(generated.width()) > output.width()
        || y.saturating_add(generated.height()) > output.height()
    {
        return Err(FukidashiError::TensorContract(
            "generated crop, mask, or destination bounds mismatch".into(),
        ));
    }
    for yy in 0..generated.height() {
        for xx in 0..generated.width() {
            if crop_mask.get_pixel(xx, yy)[0] != 0 {
                output.put_pixel(x + xx, y + yy, *generated.get_pixel(xx, yy));
            }
        }
    }
    Ok(())
}

#[cfg(feature = "onnx")]
fn rectangles_touch(a: crate::domain::Rect, b: crate::domain::Rect) -> bool {
    a.x1 <= b.x2 && b.x1 <= a.x2 && a.y1 <= b.y2 && b.y1 <= a.y2
}

#[cfg(feature = "onnx")]
fn expand_crop(
    region: crate::domain::Rect,
    width: u32,
    height: u32,
    padding: u32,
    minimum_size: u32,
    stride: u32,
) -> crate::domain::Rect {
    let cx = (region.x1 + region.x2) * 0.5;
    let cy = (region.y1 + region.y2) * 0.5;
    let desired_width = ((region.x2 - region.x1).ceil() as u32 + 2 * padding)
        .max(minimum_size)
        .div_ceil(stride)
        * stride;
    let desired_height = ((region.y2 - region.y1).ceil() as u32 + 2 * padding)
        .max(minimum_size)
        .div_ceil(stride)
        * stride;
    let crop_width = desired_width.min(width);
    let crop_height = desired_height.min(height);
    let x1 = (cx - crop_width as f32 * 0.5)
        .round()
        .clamp(0.0, (width - crop_width) as f32);
    let y1 = (cy - crop_height as f32 * 0.5)
        .round()
        .clamp(0.0, (height - crop_height) as f32);
    crate::domain::Rect {
        x1,
        y1,
        x2: x1 + crop_width as f32,
        y2: y1 + crop_height as f32,
    }
}

#[cfg(feature = "onnx")]
pub fn validate_stroke_mask(mask: &GrayImage) -> Result<()> {
    let foreground = mask.pixels().filter(|pixel| pixel[0] != 0).count();
    if foreground == 0 {
        return Err(FukidashiError::InvalidInput(
            "text stroke mask is empty; refusing destructive fallback".into(),
        ));
    }
    let binary = mask.pixels().map(|pixel| pixel[0] != 0).collect::<Vec<_>>();
    for component in connected_components(&binary, mask.width() as usize, mask.height() as usize) {
        let density = component.area as f32 / (component.width * component.height) as f32;
        if component.width >= 16 && component.height >= 16 && density > 0.90 {
            return Err(FukidashiError::InvalidInput(
                "text stroke mask contains a filled region; refusing destructive bubble-box mask"
                    .into(),
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "onnx")]
#[derive(Debug)]
struct Component {
    pixels: Vec<usize>,
    area: usize,
    width: usize,
    height: usize,
    min_x: usize,
    min_y: usize,
    max_x: usize,
    max_y: usize,
}

#[cfg(feature = "onnx")]
fn connected_components(binary: &[bool], width: usize, height: usize) -> Vec<Component> {
    let mut seen = vec![false; binary.len()];
    let mut components = Vec::new();
    for start in 0..binary.len() {
        if !binary[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        let mut queue = vec![start];
        let mut pixels = Vec::new();
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0;
        let mut max_y = 0;
        while let Some(index) = queue.pop() {
            let x = index % width;
            let y = index / width;
            pixels.push(index);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            let y0 = y.saturating_sub(1);
            let y1 = (y + 1).min(height.saturating_sub(1));
            let x0 = x.saturating_sub(1);
            let x1 = (x + 1).min(width.saturating_sub(1));
            for yy in y0..=y1 {
                for xx in x0..=x1 {
                    let neighbor = yy * width + xx;
                    if binary[neighbor] && !seen[neighbor] {
                        seen[neighbor] = true;
                        queue.push(neighbor);
                    }
                }
            }
        }
        components.push(Component {
            area: pixels.len(),
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
            min_x,
            min_y,
            max_x,
            max_y,
            pixels,
        });
    }
    components
}

#[cfg(feature = "onnx")]
fn otsu_threshold(gray: &[u8]) -> u8 {
    let mut histogram = [0u64; 256];
    for &value in gray {
        histogram[value as usize] += 1;
    }
    let total = gray.len() as f64;
    let sum = histogram
        .iter()
        .enumerate()
        .map(|(value, &count)| value as f64 * count as f64)
        .sum::<f64>();
    let mut background_weight = 0.0;
    let mut background_sum = 0.0;
    let mut best = 0.0;
    let mut threshold = 0;
    for (value, &count) in histogram.iter().enumerate() {
        background_weight += count as f64;
        if background_weight == 0.0 {
            continue;
        }
        let foreground_weight = total - background_weight;
        if foreground_weight == 0.0 {
            break;
        }
        background_sum += value as f64 * count as f64;
        let means = background_sum / background_weight - (sum - background_sum) / foreground_weight;
        let between = background_weight * foreground_weight * means * means;
        if between > best {
            best = between;
            threshold = value as u8;
        }
    }
    threshold
}

#[cfg(feature = "onnx")]
pub fn dilate_mask(mask: &GrayImage, kernel: u8) -> Result<GrayImage> {
    if kernel == 0 || kernel > 15 || kernel.is_multiple_of(2) {
        return Err(FukidashiError::InvalidInput(
            "dilation must be an odd kernel size from 1 to 15".into(),
        ));
    }
    let values = crate::vision::segment::dilate(
        mask.as_raw(),
        mask.width() as usize,
        mask.height() as usize,
        usize::from(kernel / 2),
    );
    Ok(GrayImage::from_raw(
        mask.width(),
        mask.height(),
        values
            .into_iter()
            .map(|value| if value == 0 { 0 } else { 255 })
            .collect(),
    )
    .expect("mask dimensions are unchanged"))
}

#[cfg(feature = "onnx")]
pub fn validate_mask(mask: &GrayImage, width: u32, height: u32) -> Result<()> {
    if mask.width() != width || mask.height() != height {
        return Err(FukidashiError::InvalidInput(format!(
            "mask dimensions {}x{} do not match image {}x{}",
            mask.width(),
            mask.height(),
            width,
            height
        )));
    }
    Ok(())
}

#[cfg(feature = "onnx")]
pub fn pad_symmetric_rgb(image: &RgbImage, pad_h: usize, pad_w: usize) -> Vec<f32> {
    let (width, height) = (image.width() as usize, image.height() as usize);
    let hp = height + pad_h;
    let wp = width + pad_w;
    let mut output = vec![0.0; 3 * hp * wp];
    for y in 0..hp {
        let sy = symmetric_index(y, height);
        for x in 0..wp {
            let sx = symmetric_index(x, width);
            let pixel = image.get_pixel(sx as u32, sy as u32);
            let i = y * wp + x;
            output[i] = f32::from(pixel[0]) / 255.0;
            output[hp * wp + i] = f32::from(pixel[1]) / 255.0;
            output[2 * hp * wp + i] = f32::from(pixel[2]) / 255.0;
        }
    }
    output
}

#[cfg(feature = "onnx")]
pub fn pad_symmetric_mask(mask: &GrayImage, pad_h: usize, pad_w: usize) -> Vec<f32> {
    let (width, height) = (mask.width() as usize, mask.height() as usize);
    let hp = height + pad_h;
    let wp = width + pad_w;
    let mut output = vec![0.0; hp * wp];
    for y in 0..hp {
        let sy = symmetric_index(y, height);
        for x in 0..wp {
            let sx = symmetric_index(x, width);
            output[y * wp + x] = if mask.get_pixel(sx as u32, sy as u32)[0] > 0 {
                1.0
            } else {
                0.0
            };
        }
    }
    output
}

#[cfg(feature = "onnx")]
fn symmetric_index(index: usize, size: usize) -> usize {
    let period = size.saturating_mul(2).max(1);
    let folded = index % period;
    if folded < size {
        folded
    } else {
        period - 1 - folded
    }
}

#[cfg(feature = "onnx")]
pub fn output_rgb(
    values: &[f32],
    shape: &[usize],
    width: usize,
    height: usize,
) -> Result<RgbImage> {
    if shape.len() != 4
        || shape[0] != 1
        || shape[1] != 3
        || shape[2] != height
        || shape[3] != width
        || values.len() != 3 * width * height
    {
        return Err(FukidashiError::TensorContract(format!(
            "LaMa output must be [1,3,{height},{width}], got {shape:?}"
        )));
    }
    if values.iter().any(|value| !value.is_finite()) {
        return Err(FukidashiError::TensorContract(
            "LaMa output contains nonfinite values".into(),
        ));
    }
    let plane = width * height;
    let mut output = RgbImage::new(width as u32, height as u32);
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            output.put_pixel(
                x as u32,
                y as u32,
                image::Rgb([
                    (values[i].clamp(0.0, 1.0) * 255.0) as u8,
                    (values[plane + i].clamp(0.0, 1.0) * 255.0) as u8,
                    (values[2 * plane + i].clamp(0.0, 1.0) * 255.0) as u8,
                ]),
            );
        }
    }
    Ok(output)
}

#[cfg(feature = "onnx")]
pub fn image_pixel_limit(width: u32, height: u32) -> Result<()> {
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        Err(FukidashiError::ResourceLimit(
            "inpainting image exceeds pixel limit".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(all(test, feature = "onnx"))]
mod tests {
    use super::{
        adaptive_mask_from_text_regions, composite_masked_crop, crop_regions_from_mask,
        mask_from_text_regions, restrict_mask_to_seed_neighborhood, validate_stroke_mask,
    };
    use crate::domain::Rect;
    use image::{Rgb, RgbImage};

    #[test]
    fn text_mask_keeps_balloon_outline_near_text_bbox() {
        let mut image = RgbImage::from_pixel(100, 80, Rgb([255, 255, 255]));
        for x in 10..90 {
            image.put_pixel(x, 20, Rgb([0, 0, 0]));
        }
        for y in 35..45 {
            for x in 45..50 {
                image.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let mask = mask_from_text_regions(
            image.width(),
            image.height(),
            &image,
            &[Rect {
                x1: 35.0,
                y1: 30.0,
                x2: 60.0,
                y2: 50.0,
            }],
            3,
        )
        .unwrap();
        assert_eq!(mask.get_pixel(30, 20)[0], 0);
        assert_eq!(mask.get_pixel(47, 40)[0], 255);
    }

    #[test]
    fn filled_rectangle_mask_is_rejected() {
        let mask = image::GrayImage::from_pixel(32, 32, image::Luma([255]));
        let error = validate_stroke_mask(&mask).unwrap_err().to_string();
        assert!(error.contains("filled region"));
    }

    #[test]
    fn crop_regions_merge_overlap_snap_stride_and_stay_in_bounds() {
        let mask = image::GrayImage::from_pixel(400, 300, image::Luma([0]));
        let crops = crop_regions_from_mask(
            &mask,
            &[
                Rect {
                    x1: 2.0,
                    y1: 3.0,
                    x2: 80.0,
                    y2: 60.0,
                },
                Rect {
                    x1: 70.0,
                    y1: 45.0,
                    x2: 150.0,
                    y2: 100.0,
                },
            ],
            24,
            128,
            8,
        )
        .unwrap();
        assert_eq!(crops.len(), 1);
        let crop = crops[0];
        assert!(crop.x1 >= 0.0 && crop.y1 >= 0.0);
        assert!(crop.x2 <= 400.0 && crop.y2 <= 300.0);
        assert_eq!((crop.x2 - crop.x1) as u32 % 8, 0);
        assert_eq!((crop.y2 - crop.y1) as u32 % 8, 0);
    }

    #[test]
    fn adaptive_mask_expands_large_antialiased_text_without_touching_remote_border() {
        let mut image = RgbImage::from_pixel(120, 100, Rgb([255, 255, 255]));
        for x in 5..115 {
            image.put_pixel(x, 10, Rgb([0, 0, 0]));
        }
        image.put_pixel(60, 50, Rgb([0, 0, 0]));
        image.put_pixel(61, 50, Rgb([110, 110, 110]));
        let region = Rect {
            x1: 40.0,
            y1: 25.0,
            x2: 80.0,
            y2: 90.0,
        };
        let mask = adaptive_mask_from_text_regions(120, 100, &image, &[region], 3).unwrap();
        assert_eq!(mask.get_pixel(60, 50)[0], 255);
        assert_eq!(mask.get_pixel(63, 50)[0], 255);
        assert_eq!(mask.get_pixel(60, 10)[0], 0);
    }

    #[test]
    fn seed_neighborhood_rejects_disconnected_art_inside_broad_text_bbox() {
        let mut candidate = image::GrayImage::from_pixel(128, 80, image::Luma([0]));
        let mut seed = image::GrayImage::from_pixel(128, 80, image::Luma([0]));

        // A small text stroke near the left side of the detector rectangle.
        for y in 28..52 {
            for x in 18..23 {
                candidate.put_pixel(x, y, image::Luma([255]));
                seed.put_pixel(x, y, image::Luma([255]));
            }
        }
        // Disconnected circular/line art in that same broad rectangle.  It is
        // present in the candidate image mask but has no detector seed.
        for y in 26..54 {
            for x in 72..100 {
                let dx = x as i32 - 86;
                let dy = y as i32 - 40;
                if (dx * dx + dy * dy - 11 * 11).abs() < 22 {
                    candidate.put_pixel(x, y, image::Luma([255]));
                }
            }
        }

        let restricted = restrict_mask_to_seed_neighborhood(&candidate, &seed, 2).unwrap();
        assert_eq!(restricted.get_pixel(20, 40)[0], 255);
        assert_eq!(restricted.get_pixel(86, 29)[0], 0);
        assert_eq!(restricted.get_pixel(86, 51)[0], 0);
    }

    #[test]
    fn crop_composite_changes_only_masked_pixels() {
        let mut output = RgbImage::from_pixel(5, 5, Rgb([10, 20, 30]));
        let generated = RgbImage::from_pixel(3, 3, Rgb([200, 210, 220]));
        let mut mask = image::GrayImage::from_pixel(3, 3, image::Luma([0]));
        mask.put_pixel(1, 1, image::Luma([255]));
        composite_masked_crop(&mut output, &generated, &mask, 1, 1).unwrap();
        assert_eq!(output.get_pixel(2, 2), &Rgb([200, 210, 220]));
        assert_eq!(output.get_pixel(1, 1), &Rgb([10, 20, 30]));
        assert_eq!(output.get_pixel(4, 4), &Rgb([10, 20, 30]));
    }
}
