use fukidashi_mcp::domain::{Rect, TypesetPayload};
use fukidashi_mcp::typeset::{fit_text, fit_text_with_geometry, typeset_page};
use image::{GenericImageView, ImageBuffer, Rgba};
use std::fs;
use tempfile::tempdir;

fn font_fixture() -> Option<(Vec<u8>, fontdue::Font)> {
    let candidates = [
        r"C:\Windows\Fonts\arial.ttf",
        r"/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = fs::read(path)
            && let Ok(font) =
                fontdue::Font::from_bytes(bytes.as_slice(), fontdue::FontSettings::default())
        {
            return Some((bytes, font));
        }
    }
    None
}

#[test]
fn narrow_tall_comic_bubble_fits_vietnamese_dialogue() {
    if fs::metadata(r"C:\Windows\Fonts\comic.ttf").is_err() {
        return;
    }
    let bbox = Rect {
        x1: 1_134.587_6,
        y1: 55.086_71,
        x2: 1_271.152_6,
        y2: 377.417_18,
    };
    let dir = tempdir().unwrap();
    let source = dir.path().join("page.png");
    let output = dir.path().join("rendered.png");
    ImageBuffer::<Rgba<u8>, _>::from_pixel(1400, 450, Rgba([255, 255, 255, 255]))
        .save(&source)
        .unwrap();
    let report = fukidashi_mcp::typeset::typeset_page_with_fallbacks(
        &source,
        &[TypesetPayload {
            bbox,
            bubble_bbox: None,
            text_bbox: None,
            padding: Some(3.0),
            text: "A lô—cô ơi♡".into(),
            font_path: Some(r"C:\Windows\Fonts\comic.ttf".into()),
            min_font_size: Some(1.0),
            max_font_size: Some(24.0),
            shape: Some("ellipse".into()),
        }],
        &[],
        &output,
    )
    .unwrap();
    assert_eq!(report["bubbles"][0]["font_fallback_used"], true);
    assert!(report["bubbles"][0]["font_size"].as_f64().unwrap() >= 1.0);
    let ink = &report["bubbles"][0]["ink_bbox"];
    let safe = &report["bubbles"][0]["safe_bbox"];
    assert!(ink["x1"].as_f64().unwrap() >= safe["x1"].as_f64().unwrap() - 1.0);
    assert!(ink["x2"].as_f64().unwrap() <= safe["x2"].as_f64().unwrap() + 1.0);
    assert!(ink["y1"].as_f64().unwrap() >= safe["y1"].as_f64().unwrap() - 1.0);
    assert!(ink["y2"].as_f64().unwrap() <= safe["y2"].as_f64().unwrap() + 1.0);
    assert!(output.is_file());
}

#[test]
fn shaped_layout_preserves_unicode_and_respects_half_pixel_upper_bound() {
    let Some((bytes, font)) = font_fixture() else {
        return;
    };
    let face = rustybuzz::Face::from_slice(&bytes, 0).unwrap();
    let result = fit_text(
        &face,
        &font,
        "Cafe\u{301} j",
        Rect {
            x1: 0.0,
            y1: 0.0,
            x2: 160.0,
            y2: 80.0,
        },
        "ellipse",
        8.0,
        12.3,
    )
    .unwrap();
    assert!(result.font_size <= 12.3 + 1e-4);
    assert_eq!(
        result
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<String>(),
        "Cafe\u{301} j"
    );
    assert!(
        result
            .lines
            .iter()
            .all(|line| line.ink_left <= line.ink_right)
    );
}

#[test]
fn long_vietnamese_text_stays_inside_inset_bubble_geometry() {
    let Some((bytes, font)) = font_fixture() else {
        return;
    };
    let face = rustybuzz::Face::from_slice(&bytes, 0).unwrap();
    let bubble = Rect {
        x1: 10.0,
        y1: 10.0,
        x2: 230.0,
        y2: 150.0,
    };
    let result = fit_text_with_geometry(
        &face,
        &font,
        "Sức khỏe yếu mà, đừng có ngã nhé? Sáng mai tôi sẽ mang nó trả ngay.",
        Rect {
            x1: 80.0,
            y1: 35.0,
            x2: 160.0,
            y2: 125.0,
        },
        Some(bubble),
        None,
        "ellipse",
        4.0,
        24.0,
        None,
    )
    .unwrap();
    assert!(result.safe_bbox.x1 > bubble.x1);
    assert!(result.safe_bbox.y1 > bubble.y1);
    assert!(result.safe_bbox.x2 < bubble.x2);
    assert!(result.safe_bbox.y2 < bubble.y2);
    let cx = result.placement_center.0;
    for line in &result.lines {
        let origin_x = cx - (line.ink_left + line.ink_right) * 0.5;
        assert!(origin_x + line.ink_left >= result.safe_bbox.x1 - 1.0);
        assert!(origin_x + line.ink_right <= result.safe_bbox.x2 + 1.0);
        assert!(line.top >= result.safe_bbox.y1 - 1.0);
        assert!(line.bottom <= result.safe_bbox.y2 + 1.0);
    }
}

#[test]
fn raster_output_is_png_and_does_not_modify_source() {
    let Some((_bytes, _font)) = font_fixture() else {
        return;
    };
    let font_path = if fs::metadata(r"C:\Windows\Fonts\arial.ttf").is_ok() {
        r"C:\Windows\Fonts\arial.ttf"
    } else {
        r"/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
    };
    let dir = tempdir().unwrap();
    let source = dir.path().join("page.png");
    let output = dir.path().join("rendered.png");
    let original = ImageBuffer::<Rgba<u8>, _>::from_pixel(200, 120, Rgba([240, 240, 240, 255]));
    original.save(&source).unwrap();
    let payload = TypesetPayload {
        bbox: Rect {
            x1: 20.0,
            y1: 20.0,
            x2: 180.0,
            y2: 100.0,
        },
        bubble_bbox: Some(Rect {
            x1: 20.0,
            y1: 20.0,
            x2: 180.0,
            y2: 100.0,
        }),
        text_bbox: Some(Rect {
            x1: 60.0,
            y1: 30.0,
            x2: 140.0,
            y2: 90.0,
        }),
        padding: Some(6.0),
        text: "Xin chao\nbonjour".into(),
        font_path: Some(font_path.into()),
        min_font_size: Some(8.0),
        max_font_size: Some(18.0),
        shape: Some("rectangle".into()),
    };
    let report = typeset_page(&source, &[payload], &output).unwrap();
    assert_eq!(report["bubbles"][0]["shape"], "rectangle");
    assert_eq!(report["bubbles"][0]["padding"], 6.0);
    assert!(report["bubbles"][0]["safe_bbox"].is_object());
    assert!(report["bubbles"][0]["line_count"].as_u64().unwrap() >= 1);
    assert_eq!(image::open(&source).unwrap().dimensions(), (200, 120));
    assert_eq!(image::open(&output).unwrap().dimensions(), (200, 120));
}
