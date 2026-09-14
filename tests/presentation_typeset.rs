use fukidashi_mcp::domain::{Rect, TypesetPayload};
use fukidashi_mcp::typeset::{fit_text, fit_text_with_geometry, typeset_page};
use image::{GenericImageView, ImageBuffer, Rgba};
use std::fs;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use unicode_segmentation::UnicodeSegmentation;

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

fn ink_fixture(
    fill: Rgba<u8>,
    center_disk: bool,
    text_color: Option<&str>,
) -> (image::RgbaImage, serde_json::Value) {
    let dir = tempdir().unwrap();
    let font_path = dir.path().join("ComicNeue-Regular.ttf");
    fs::write(&font_path, fukidashi_mcp::fonts::COMIC_NEUE_REGULAR.bytes).unwrap();
    let source = dir.path().join("source.png");
    let output = dir.path().join("rendered.png");
    let mut original = ImageBuffer::<Rgba<u8>, _>::from_pixel(64, 64, fill);
    if center_disk {
        for y in 24..40 {
            for x in 24..40 {
                original.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
    }
    original.save(&source).unwrap();
    let report = typeset_page(
        &source,
        &[TypesetPayload {
            id: Some("ink-fixture".into()),
            source_text: Some("ABC".into()),
            kind: Some("dialogue".into()),
            preserve_by_default: Some(false),
            needs_review: None,
            flagged: None,
            preserve_source: Some(false),
            fallback_font_paths: Vec::new(),
            bbox: Rect {
                x1: 4.0,
                y1: 4.0,
                x2: 60.0,
                y2: 60.0,
            },
            bubble_bbox: Some(Rect {
                x1: 4.0,
                y1: 4.0,
                x2: 60.0,
                y2: 60.0,
            }),
            text_bbox: None,
            padding: Some(4.0),
            text: "ABC".into(),
            font_path: Some(font_path.display().to_string()),
            min_font_size: Some(8.0),
            max_font_size: Some(18.0),
            text_color: text_color.map(str::to_owned),
            shape: Some("rectangle".into()),
        }],
        &output,
    )
    .unwrap();
    (
        image::open(&output).unwrap().to_rgba8(),
        report["bubbles"][0].clone(),
    )
}

#[test]
fn auto_ink_uses_clean_center_median_and_explicit_overrides() {
    let (black_page, black_report) = ink_fixture(Rgba([0, 0, 0, 255]), false, None);
    let black_changed = black_page.pixels().filter(|pixel| pixel[0] > 180).count();
    assert!(
        black_changed > 0,
        "black balloon should receive white glyph pixels"
    );
    assert_eq!(black_report["resolved_text_color"], "white");
    assert_eq!(black_report["sampled_luminance"], 0);

    let (white_page, white_report) = ink_fixture(Rgba([255, 255, 255, 255]), false, None);
    let white_changed = white_page.pixels().filter(|pixel| pixel[0] < 80).count();
    assert!(
        white_changed > 0,
        "white balloon should receive black glyph pixels"
    );
    assert_eq!(white_report["resolved_text_color"], "black");
    assert_eq!(white_report["sampled_luminance"], 255);

    let (mixed_page, mixed_report) = ink_fixture(Rgba([255, 255, 255, 255]), true, None);
    let mixed_changed = mixed_page.pixels().filter(|pixel| pixel[0] > 180).count();
    assert!(
        mixed_changed > 0,
        "central dark balloon should receive white glyph pixels"
    );
    assert_eq!(mixed_report["resolved_text_color"], "white");

    let (_override_page, override_report) =
        ink_fixture(Rgba([255, 255, 255, 255]), false, Some("white"));
    assert_eq!(override_report["requested_text_color"], "white");
    assert_eq!(override_report["resolved_text_color"], "white");
    assert!(override_report["sampled_luminance"].is_null());

    let (black_override_page, black_override_report) =
        ink_fixture(Rgba([0, 0, 0, 255]), false, Some("black"));
    assert!(black_override_page.pixels().all(|pixel| pixel[0] == 0));
    assert_eq!(black_override_report["resolved_text_color"], "black");
    assert!(black_override_report["sampled_luminance"].is_null());
}

#[test]
fn gray_screentone_hole_uses_the_detector_ellipse_envelope() {
    let dir = tempdir().unwrap();
    let font_path = dir.path().join("PatrickHand-Regular.ttf");
    fs::write(&font_path, fukidashi_mcp::fonts::PATRICK_HAND_REGULAR.bytes).unwrap();
    let source = dir.path().join("screentone.png");
    let output = dir.path().join("screentone-rendered.png");
    let bubble = Rect {
        x1: 10.0,
        y1: 10.0,
        x2: 270.0,
        y2: 390.0,
    };
    let mut image = ImageBuffer::from_pixel(280, 400, Rgba::<u8>([148, 148, 148, 255]));
    for y in 10..390 {
        for x in 10..270 {
            if (x + y) % 5 == 0 {
                image.put_pixel(x, y, Rgba([126, 126, 126, 255]));
            }
        }
    }
    for y in 180..220 {
        for x in 120..160 {
            image.put_pixel(x, y, Rgba([255, 255, 255, 255]));
        }
    }
    image.save(&source).unwrap();
    let report = fukidashi_mcp::typeset::typeset_page(
        &source,
        &[TypesetPayload {
            id: Some("screentone".into()),
            source_text: Some("source".into()),
            kind: Some("dialogue".into()),
            preserve_by_default: Some(false),
            needs_review: None,
            flagged: None,
            preserve_source: Some(false),
            fallback_font_paths: Vec::new(),
            bbox: bubble,
            bubble_bbox: Some(bubble),
            text_bbox: Some(Rect {
                x1: 105.0,
                y1: 170.0,
                x2: 175.0,
                y2: 230.0,
            }),
            padding: Some(8.0),
            text: "ĐẶC BIỆT".into(),
            font_path: Some(font_path.display().to_string()),
            min_font_size: Some(8.0),
            max_font_size: Some(72.0),
            text_color: None,
            shape: Some("ellipse".into()),
        }],
        &output,
    )
    .unwrap();
    let bubble_report = &report["bubbles"][0];
    assert_eq!(bubble_report["mask_used"], false);
    assert!(bubble_report["font_size"].as_f64().unwrap() > 16.0);
    assert!(bubble_report["safe_bbox"]["x2"].as_f64().unwrap() > 220.0);
    assert!(bubble_report["safe_bbox"]["y2"].as_f64().unwrap() > 330.0);
}

#[test]
fn overlapping_dark_and_colored_bubbles_use_shape_ownership_fallback() {
    let dir = tempdir().unwrap();
    let font_path = dir.path().join("ComicNeue-Regular.ttf");
    fs::write(&font_path, fukidashi_mcp::fonts::COMIC_NEUE_REGULAR.bytes).unwrap();
    let source = dir.path().join("overlap.png");
    let output = dir.path().join("overlap-rendered.png");
    let areas = [
        Rect {
            x1: 20.0,
            y1: 10.0,
            x2: 130.0,
            y2: 110.0,
        },
        Rect {
            x1: 90.0,
            y1: 10.0,
            x2: 210.0,
            y2: 110.0,
        },
        Rect {
            x1: 170.0,
            y1: 10.0,
            x2: 280.0,
            y2: 110.0,
        },
    ];
    let mut image = ImageBuffer::<Rgba<u8>, _>::from_pixel(300, 120, Rgba([72, 72, 72, 255]));
    let fills = [
        Rgba([72, 72, 72, 255]),
        Rgba([34, 34, 34, 255]),
        Rgba([42, 66, 74, 255]),
    ];
    for (area, fill) in areas.iter().zip(fills) {
        let center = ((area.x1 + area.x2) * 0.5, (area.y1 + area.y2) * 0.5);
        let radius = ((area.x2 - area.x1) * 0.5, (area.y2 - area.y1) * 0.5);
        for y in area.y1 as u32..area.y2 as u32 {
            for x in area.x1 as u32..area.x2 as u32 {
                let dx = (x as f32 + 0.5 - center.0) / radius.0;
                let dy = (y as f32 + 0.5 - center.1) / radius.1;
                if dx * dx + dy * dy <= 1.0 {
                    image.put_pixel(x, y, fill);
                }
            }
        }
    }
    image.save(&source).unwrap();
    let payloads = areas
        .into_iter()
        .enumerate()
        .map(|(index, bbox)| TypesetPayload {
            id: Some(format!("overlap-{index}")),
            source_text: Some("source".into()),
            kind: Some("dialogue".into()),
            preserve_by_default: Some(false),
            needs_review: None,
            flagged: None,
            preserve_source: Some(false),
            fallback_font_paths: Vec::new(),
            bbox,
            bubble_bbox: Some(bbox),
            text_bbox: None,
            padding: Some(4.0),
            text: format!("BUBBLE {index}"),
            font_path: Some(font_path.display().to_string()),
            min_font_size: Some(8.0),
            max_font_size: Some(20.0),
            text_color: Some(if index == 1 { "black" } else { "white" }.into()),
            shape: Some("ellipse".into()),
        })
        .collect::<Vec<_>>();
    let report = typeset_page(&source, &payloads, &output).unwrap();
    let rendered = image::open(&output).unwrap().to_rgba8();
    let bubble_reports = report["bubbles"].as_array().unwrap();
    assert!(
        bubble_reports
            .iter()
            .all(|bubble| bubble["mask_used"] == true)
    );
    assert!(bubble_reports[0]["safe_mask_bbox"]["x2"].as_f64().unwrap() <= 113.0);
    assert!(bubble_reports[1]["safe_mask_bbox"]["x1"].as_f64().unwrap() >= 112.0);
    assert!(bubble_reports[1]["safe_mask_bbox"]["x2"].as_f64().unwrap() <= 189.0);
    assert!(bubble_reports[2]["safe_mask_bbox"]["x1"].as_f64().unwrap() >= 187.0);
    assert!(bubble_reports[0]["safe_bbox"]["x2"].as_f64().unwrap() <= 113.0);
    assert!(bubble_reports[1]["safe_bbox"]["x1"].as_f64().unwrap() >= 112.0);
    assert!(bubble_reports[1]["safe_bbox"]["x2"].as_f64().unwrap() <= 189.0);
    assert!(bubble_reports[2]["safe_bbox"]["x1"].as_f64().unwrap() >= 187.0);

    assert!(rendered.pixels().any(|pixel| pixel[0] > 220));
    // White glyphs from the first and third bubbles cannot enter the overlap
    // strips owned by the middle/neighboring detector centres.
    for (x, y, pixel) in rendered.enumerate_pixels() {
        if pixel[0] > 220 {
            assert!(
                !(113..188).contains(&x),
                "white ink invaded middle owner at {x},{y}"
            );
            assert!(
                !(170..188).contains(&x),
                "white ink invaded right owner at {x},{y}"
            );
        }
    }
    for (x, _y, pixel) in rendered.enumerate_pixels() {
        if pixel[0] < 10 {
            assert!(
                (113..188).contains(&x),
                "black ink escaped middle owner at {x}"
            );
        }
    }
}

#[test]
fn text_color_is_legacy_optional_and_rejects_unknown_values() {
    let legacy: TypesetPayload = serde_json::from_value(serde_json::json!({
        "bbox": {"x1": 1.0, "y1": 1.0, "x2": 8.0, "y2": 8.0},
        "text": "ABC"
    }))
    .unwrap();
    assert_eq!(legacy.text_color, None);
    let mut invalid = legacy;
    invalid.text_color = Some("red".into());
    assert!(invalid.validate_text_color().is_err());
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
            id: None,
            source_text: None,
            kind: None,
            preserve_by_default: None,
            needs_review: None,
            flagged: None,
            preserve_source: None,
            fallback_font_paths: Vec::new(),
            bbox,
            bubble_bbox: None,
            text_bbox: None,
            padding: Some(3.0),
            text: "A lô—cô ơi♡".into(),
            font_path: Some(r"C:\Windows\Fonts\comic.ttf".into()),
            min_font_size: Some(1.0),
            max_font_size: Some(24.0),
            text_color: None,
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
fn long_afterword_typesetting_is_bounded_and_preserves_hard_newlines() {
    let Some((bytes, font)) = font_fixture() else {
        return;
    };
    let face = rustybuzz::Face::from_slice(&bytes, 0).unwrap();
    let paragraph = "Afterword: Cafe\u{301} readers kept every detail, and the quiet ending left room for tomorrow. ";
    let mut text = String::from("AFTERWORD\n");
    while text.graphemes(true).count() < 1_800 {
        text.push_str(paragraph);
    }
    assert!(text.graphemes(true).count() >= 1_500);
    assert!(text.graphemes(true).count() <= 3_000);

    let started = Instant::now();
    let result = fit_text_with_geometry(
        &face,
        &font,
        &text,
        Rect {
            x1: 0.0,
            y1: 0.0,
            x2: 1_200.0,
            y2: 900.0,
        },
        None,
        None,
        "rectangle",
        8.0,
        24.0,
        None,
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "long afterword took {elapsed:?}; bounded layout regressed"
    );

    match result {
        Ok(layout) => {
            assert_eq!(
                layout.lines.first().map(|line| line.text.as_str()),
                Some("AFTERWORD")
            );
            assert!(layout.lines.iter().all(|line| !line.text.contains('\n')));
        }
        Err(error) => {
            let message = error.to_string().to_lowercase();
            assert!(
                message.contains("overflow") || message.contains("fit"),
                "long afterword failed without an explicit fit/overflow result: {error}"
            );
        }
    }
}

#[test]
fn medium_prose_boxes_use_bounded_typesetting() {
    let Some((bytes, font)) = font_fixture() else {
        return;
    };
    let face = rustybuzz::Face::from_slice(&bytes, 0).unwrap();
    let paragraph = "Readers kept the measured afterword close, carrying each quiet detail into the next page. ";
    let mut boxes = Vec::new();
    for index in 0..3 {
        let mut text = format!("BOX {index}\n");
        while text.graphemes(true).count() < 320 {
            text.push_str(paragraph);
        }
        assert!(text.graphemes(true).count() >= 200);
        assert!(text.graphemes(true).count() <= 400);
        boxes.push(text);
    }

    let started = Instant::now();
    for (index, text) in boxes.iter().enumerate() {
        let layout = fit_text_with_geometry(
            &face,
            &font,
            text,
            Rect {
                x1: 0.0,
                y1: 0.0,
                x2: 900.0,
                y2: 500.0,
            },
            None,
            None,
            "rectangle",
            8.0,
            24.0,
            None,
        )
        .expect("medium prose box should fit");
        let expected_first_line = format!("BOX {index}");
        assert_eq!(
            layout.lines.first().map(|line| line.text.as_str()),
            Some(expected_first_line.as_str())
        );
        assert!(layout.lines.iter().all(|line| !line.text.contains('\n')));
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(8),
        "medium prose boxes took {elapsed:?}; recursive layout escaped its work bound"
    );
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
        id: None,
        source_text: None,
        kind: None,
        preserve_by_default: None,
        needs_review: None,
        flagged: None,
        preserve_source: None,
        fallback_font_paths: Vec::new(),
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
        text_color: None,
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

#[test]
fn preserved_and_empty_payloads_skip_layout_fitting() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("page.png");
    let output = dir.path().join("rendered.png");
    let original = ImageBuffer::<Rgba<u8>, _>::from_pixel(48, 32, Rgba([240, 240, 240, 255]));
    original.save(&source).unwrap();

    let report = fukidashi_mcp::typeset::typeset_page_with_fallbacks(
        &source,
        &[
            TypesetPayload {
                id: Some("text-sfx".into()),
                source_text: Some("クス".into()),
                kind: Some("unmatched_text".into()),
                preserve_by_default: None,
                needs_review: None,
                flagged: None,
                preserve_source: None,
                fallback_font_paths: Vec::new(),
                bbox: Rect {
                    x1: 20.0,
                    y1: 10.0,
                    x2: 10.0,
                    y2: 10.0,
                },
                bubble_bbox: None,
                text_bbox: None,
                padding: None,
                text: "クス".into(),
                font_path: None,
                min_font_size: None,
                max_font_size: None,
                text_color: None,
                shape: None,
            },
            TypesetPayload {
                id: Some("empty".into()),
                source_text: Some("source".into()),
                kind: Some("dialogue".into()),
                preserve_by_default: Some(false),
                needs_review: None,
                flagged: None,
                preserve_source: Some(false),
                fallback_font_paths: Vec::new(),
                bbox: Rect {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 0.0,
                    y2: 0.0,
                },
                bubble_bbox: None,
                text_bbox: None,
                padding: None,
                text: "".into(),
                font_path: None,
                min_font_size: None,
                max_font_size: None,
                text_color: None,
                shape: None,
            },
        ],
        &[],
        &output,
    )
    .unwrap();

    assert_eq!(report["bubbles"][0]["skipped"], true);
    assert_eq!(
        report["bubbles"][0]["skip_reason"],
        "structural_unmatched_text"
    );
    assert_eq!(report["bubbles"][1]["skipped"], true);
    assert_eq!(report["bubbles"][1]["skip_reason"], "empty_text");
    assert_eq!(fs::read(&source).unwrap(), fs::read(&output).unwrap());
}
