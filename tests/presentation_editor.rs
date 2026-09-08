use fukidashi_mcp::domain::{Rect, TypesetPayload};
use fukidashi_mcp::editor::{serve_editor, serve_editor_with_allowed_sources, wait_for_review};
use fukidashi_mcp::workflow::Workflow;
use image::{ImageBuffer, Rgb};
use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use tempfile::tempdir;

fn request(host: &str, path: &str, method: &str, body: Option<&str>, request_host: &str) -> String {
    String::from_utf8(request_bytes(host, path, method, body, request_host)).unwrap()
}

fn request_bytes(
    host: &str,
    path: &str,
    method: &str,
    body: Option<&str>,
    request_host: &str,
) -> Vec<u8> {
    let mut stream = TcpStream::connect(host).unwrap();
    let bytes = body.unwrap_or("").as_bytes();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: {request_host}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", bytes.len()).unwrap();
    stream.write_all(bytes).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    response
}

#[test]
fn editor_requires_exact_host_and_persists_valid_state() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(4, 4, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let result = serve_editor(&image_path, json!({"schema_version":1,"pages":[]})).unwrap();
    let url = result["url"].as_str().unwrap();
    let endpoint = url.strip_prefix("http://").unwrap();
    let (host, suffix) = endpoint.split_once('/').unwrap();
    let token_path = format!("/{suffix}");
    let forbidden = request(host, &token_path, "GET", None, "localhost");
    assert!(forbidden.starts_with("HTTP/1.1 403"));
    let ok = request(host, &token_path, "GET", None, host);
    assert!(ok.starts_with("HTTP/1.1 200"));
    assert!(ok.contains("Fukidashi editor"));

    let save_path = format!("/{}/save", result["session_token"].as_str().unwrap());
    let state = json!({"schema_version":1,"pages":[],"bubbles":[{"bbox":{"x1":0,"y1":0,"x2":2,"y2":2},"text":"保存"}]});
    let saved = request(
        host,
        &save_path,
        "POST",
        Some(&serde_json::to_string(&state).unwrap()),
        host,
    );
    assert!(saved.starts_with("HTTP/1.1 200"));
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(result["persistence_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(persisted["bubbles"][0]["text"], "保存");
    let translations = dir.path().join("translations.json");
    assert!(translations.is_file());
}

#[test]
fn editor_gallery_variants_and_render_endpoint_are_constrained() {
    let dir = tempdir().unwrap();
    for name in ["page-1.png", "page-2.png"] {
        ImageBuffer::<Rgb<u8>, _>::from_pixel(8, 8, Rgb([255, 255, 255]))
            .save(dir.path().join(name))
            .unwrap();
    }
    let state = json!({
        "schema_version": 1,
        "pages": [
            {"id":"p1","image_path":"page-1.png","bubbles":[{"id":"b1","bbox":{"x1":1,"y1":1,"x2":5,"y2":5},"text":"Hello","translation":"Xin chào","confidence":0.9,"reading_order":0}]},
            {"id":"p2","image_path":"page-2.png","bubbles":[]}
        ]
    });
    let result = serve_editor(&dir.path().join("page-1.png"), state).unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, suffix) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();

    let page = request_bytes(
        host,
        &format!("/{token}/page/1/image?variant=source"),
        "GET",
        None,
        host,
    );
    assert!(String::from_utf8_lossy(&page).starts_with("HTTP/1.1 200"));
    let forbidden_variant = request(
        host,
        &format!("/{token}/page/0/image?variant=../secret"),
        "GET",
        None,
        host,
    );
    assert!(forbidden_variant.starts_with("HTTP/1.1 404"));
    let traversal = json!({"schema_version":1,"pages":[{"id":"bad","image_path":"../outside.png","bubbles":[]}]});
    let rejected = request(
        host,
        &format!("/{token}/save"),
        "POST",
        Some(&traversal.to_string()),
        host,
    );
    assert!(rejected.starts_with("HTTP/1.1 400"));
    let render_state = json!({
        "schema_version": 1,
        "pages": [{"id":"p1","image_path":"page-1.png","bubbles":[{"id":"b1","bbox":{"x1":1,"y1":1,"x2":5,"y2":5},"text":"Hello","translation":"Xin chào","confidence":0.9,"reading_order":0}]}]
    });
    let render_request = json!({"page_index": 0, "state": render_state});
    let render = request(
        host,
        &format!("/{token}/render"),
        "POST",
        Some(&render_request.to_string()),
        host,
    );
    assert!(render.starts_with("HTTP/1.1 400"));
    let html = request(host, &format!("/{suffix}"), "GET", None, host);
    for marker in [
        "renderGallery",
        "cleaned",
        "render",
        "translation",
        "flagged",
    ] {
        assert!(html.contains(marker), "editor HTML missing {marker}");
    }
    assert!(html.contains("fetch(endpoint('save'),"));
    assert!(html.contains("Fukidashi editor review"));
}

#[test]
fn stale_editor_state_cannot_erase_recovered_bubbles_or_render() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(12, 12, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let result = serve_editor(
        &image_path,
        json!({"schema_version":1,"pages":[{"id":"p1","image_path":"page.png","bubbles":[{"id":"b1","bbox":{"x1":1,"y1":1,"x2":5,"y2":5},"translation":"kept"}]}]}),
    )
    .unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, suffix) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let state_path = result["persistence_path"].as_str().unwrap();
    let recovered = json!({
        "schema_version": 1,
        "state_revision": 1,
        "pages": [{"id":"p1","image_path":"page.png","bubbles":[{"id":"b1","bbox":{"x1":1,"y1":1,"x2":5,"y2":5},"translation":"recovered"}] }]
    });
    fs::write(state_path, serde_json::to_vec_pretty(&recovered).unwrap()).unwrap();
    let stale = json!({
        "schema_version": 1,
        "state_revision": 0,
        "pages": [{"id":"p1","image_path":"page.png","bubbles":[]}]
    });
    let save = request(
        host,
        &format!("/{token}/save"),
        "POST",
        Some(&stale.to_string()),
        host,
    );
    assert!(
        save.starts_with("HTTP/1.1 409"),
        "unexpected stale save response: {save}"
    );
    let render = json!({"page_index":0,"state":stale});
    let render_response = request(
        host,
        &format!("/{token}/render"),
        "POST",
        Some(&render.to_string()),
        host,
    );
    assert!(render_response.starts_with("HTTP/1.1 409"));
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
    assert_eq!(persisted["state_revision"], 1);
    assert_eq!(
        persisted["pages"][0]["bubbles"][0]["translation"],
        "recovered"
    );
    let html = request(host, &format!("/{suffix}"), "GET", None, host);
    assert!(html.contains("Edit translation"));
    assert!(html.contains("Reload project"));
}

#[test]
fn reopening_legacy_sidecars_preserves_recovered_bubbles_and_strokes() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(12, 12, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let state_path = dir.path().join("project.json");
    let recovered = json!({
        "schema_version": 1,
        "state_revision": 5,
        "pages": [{"id":"p1","image_path":"page.png","bubbles":[{"id":"bubble-1","bbox":{"x1":1,"y1":1,"x2":5,"y2":5},"translation":"recovered"}],"issues":[],"correction_strokes":[{"mode":"cover","size":4,"points":[{"x":3,"y":3}]}]}]
    });
    fs::write(&state_path, serde_json::to_vec_pretty(&recovered).unwrap()).unwrap();
    let legacy = json!({
        "schema_version": 1,
        "pages": [{"id":"p1","image_path":"page.png","bubbles":[],"issues":[],"correction_strokes":[]}]
    });
    let result = serve_editor(&image_path, legacy.clone()).unwrap();
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(
        persisted["pages"][0]["bubbles"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        persisted["pages"][0]["correction_strokes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, suffix) = endpoint.split_once('/').unwrap();
    let state_response = request(host, &format!("/{suffix}state"), "GET", None, host);
    assert!(state_response.contains("bubble-1"));
    let second = serve_editor(&image_path, legacy).unwrap();
    let _ = request(
        second["url"]
            .as_str()
            .unwrap()
            .strip_prefix("http://")
            .unwrap()
            .split_once('/')
            .unwrap()
            .0,
        &format!("/{}/state", second["session_token"].as_str().unwrap()),
        "GET",
        None,
        second["url"]
            .as_str()
            .unwrap()
            .strip_prefix("http://")
            .unwrap()
            .split_once('/')
            .unwrap()
            .0,
    );
    let persisted_again: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(
        persisted_again["pages"][0]["bubbles"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        persisted_again["pages"][0]["correction_strokes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn review_submit_wakes_matching_waiter_and_persists_audit() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(20, 20, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let state = json!({"schema_version":1,"pages":[{"id":"p1","image_path":"page.png","bubbles":[{"id":"b1","bbox":{"x1":2,"y1":2,"x2":10,"y2":10},"text":"source"}]}]});
    let result = serve_editor(&image_path, state).unwrap();
    let session = result["review_session_id"].as_str().unwrap().to_owned();
    let revision = result["review_revision"].as_u64().unwrap();
    let waiter = tokio::spawn({
        let session = session.clone();
        async move { wait_for_review(&session, revision, 10).await }
    });
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, _) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let submit = json!({
        "revision": revision,
        "action": "request_fixes",
        "feedback": [{"page":0,"bubble_id":"b1","bbox":{"x1":1,"y1":1,"x2":8,"y2":8},"issue_type":"font_or_layout","note":"too tight","corrected_text":null,"source_ocr":"source","current_translation":"old","link_status":"linked","origin":"image-pixels"}]
    });
    let response = request(
        host,
        &format!("/{token}/review/submit"),
        "POST",
        Some(&submit.to_string()),
        host,
    );
    assert!(response.starts_with("HTTP/1.1 200"));
    let value = waiter.await.unwrap().unwrap();
    assert_eq!(value["review_session_id"], session);
    assert_eq!(value["revision"], revision);
    assert_eq!(value["action"], "request_fixes");
    assert_eq!(value["feedback"][0]["source_ocr"], "source");
    assert_eq!(value["feedback"][0]["current_translation"], "old");
    assert_eq!(value["feedback"][0]["link_status"], "linked");
    assert!(dir.path().join("review.json").is_file());
    assert!(dir.path().join("review-audit.json").is_file());
}

#[test]
fn review_rejects_stale_revision_and_malformed_bbox() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(20, 20, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let result = serve_editor(
        &image_path,
        json!({"schema_version":1,"pages":[{"id":"p1","image_path":"page.png","bubbles":[]}]}),
    )
    .unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, _) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let stale = json!({"revision":0,"action":"request_fixes","feedback":[]});
    assert!(
        request(
            host,
            &format!("/{token}/review/submit"),
            "POST",
            Some(&stale.to_string()),
            host
        )
        .starts_with("HTTP/1.1 400")
    );
    let bad = json!({"revision":result["review_revision"],"action":"request_fixes","feedback":[{"page":0,"bbox":{"x1":-1,"y1":1,"x2":8,"y2":8},"issue_type":"custom","origin":"image-pixels"}]});
    assert!(
        request(
            host,
            &format!("/{token}/review/submit"),
            "POST",
            Some(&bad.to_string()),
            host
        )
        .starts_with("HTTP/1.1 400")
    );
}

#[test]
fn approval_rejects_flags_issues_and_dirty_render_state() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(20, 20, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let result = serve_editor(
        &image_path,
        json!({
            "schema_version": 1,
            "pages": [{
                "id": "p1",
                "image_path": "page.png",
                "render_dirty": true,
                "issues": [{"issue_type": "text_overflow", "origin": "image-pixels"}],
                "bubbles": [{
                    "id": "b1",
                    "bbox": {"x1": 2, "y1": 2, "x2": 10, "y2": 10},
                    "translation": "text",
                    "flagged": true
                }, {
                    "id": "b2",
                    "bbox": {"x1": 10, "y1": 2, "x2": 18, "y2": 10},
                    "source_text": "原文"
                }]
            }]
        }),
    )
    .unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, _) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let approve = json!({
        "revision": result["review_revision"],
        "action": "approve_export",
        "approved_pages": [0]
    });
    let response = request(
        host,
        &format!("/{token}/review/submit"),
        "POST",
        Some(&approve.to_string()),
        host,
    );
    assert!(response.starts_with("HTTP/1.1 400"));
    assert!(response.contains("render_dirty"));
    assert!(response.contains("flagged_bubble"));
    assert!(response.contains("missing_translation"));
    assert!(response.contains("issue"));
}

#[test]
fn removing_a_bubble_persists_tombstone_and_restores_source_pixels_on_render() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("page.png");
    let mut source_image = ImageBuffer::<Rgb<u8>, _>::from_pixel(32, 32, Rgb([255, 255, 255]));
    source_image.put_pixel(3, 3, Rgb([0, 0, 0]));
    source_image.save(&source).unwrap();
    let jobs = dir.path().join("jobs");
    let workflow = Workflow::new(jobs).unwrap();
    let registration = workflow.register_analysis(&source, None).unwrap();
    let cleaned = ImageBuffer::<Rgb<u8>, _>::from_pixel(32, 32, Rgb([255, 255, 255]));
    let mut mask = image::GrayImage::new(32, 32);
    mask.put_pixel(3, 3, image::Luma([255]));
    let (cleaned_path, _, _) = workflow
        .write_clean_artifact(&source, &cleaned, &mask, 3, "full")
        .unwrap();
    let rendered_path = workflow.page_artifacts_for_source(&source).unwrap().4;
    let font = workflow
        .materialize_bundled_font(
            &registration.job_dir,
            &fukidashi_mcp::fonts::COMIC_NEUE_REGULAR,
        )
        .unwrap();
    let payload = TypesetPayload {
        id: Some("b1".into()),
        source_text: Some("source".into()),
        kind: Some("dialogue".into()),
        preserve_by_default: Some(false),
        needs_review: Some(false),
        flagged: Some(false),
        bbox: Rect {
            x1: 1.0,
            y1: 1.0,
            x2: 20.0,
            y2: 20.0,
        },
        bubble_bbox: None,
        text_bbox: None,
        padding: None,
        text: "dịch".into(),
        font_path: Some(font.display().to_string()),
        min_font_size: Some(1.0),
        max_font_size: Some(8.0),
        shape: Some("rectangle".into()),
    };
    let report = fukidashi_mcp::typeset::typeset_page(
        &cleaned_path,
        std::slice::from_ref(&payload),
        &rendered_path,
    )
    .unwrap();
    let clean = workflow.validate_clean_input(&cleaned_path).unwrap();
    workflow
        .register_render(
            &rendered_path,
            &clean,
            json!({"request_bubbles":[payload],"report":report}),
            json!({}),
        )
        .unwrap();
    let state = workflow.editor_state(&rendered_path, None).unwrap();
    let result = serve_editor_with_allowed_sources(
        &rendered_path,
        state,
        vec![fs::canonicalize(&source).unwrap()],
    )
    .unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, _) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let mut edited: serde_json::Value =
        serde_json::from_slice(&fs::read(result["persistence_path"].as_str().unwrap()).unwrap())
            .unwrap();
    let bubble = edited["pages"][0]["bubbles"][0].clone();
    edited["pages"][0]["bubbles"] = json!([]);
    edited["pages"][0]["removed_bubbles"] = json!([{
        "id": "b1",
        "bbox": bubble["bbox"],
        "source_text": "source",
        "translation": "dịch",
        "removed_reason": "preserve_original"
    }]);
    edited["pages"][0]["render_dirty"] = json!(true);
    let save = request(
        host,
        &format!("/{token}/save"),
        "POST",
        Some(&edited.to_string()),
        host,
    );
    assert!(save.starts_with("HTTP/1.1 200"), "unexpected save: {save}");
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(result["persistence_path"].as_str().unwrap()).unwrap())
            .unwrap();
    let render = request(
        host,
        &format!("/{token}/render"),
        "POST",
        Some(&json!({"page_index":0,"state":saved}).to_string()),
        host,
    );
    assert!(
        render.starts_with("HTTP/1.1 200"),
        "unexpected render: {render}"
    );
    let restored = image::open(&rendered_path).unwrap().to_rgb8();
    assert_eq!(*restored.get_pixel(3, 3), Rgb([0, 0, 0]));
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(result["persistence_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert!(
        persisted["pages"][0]["bubbles"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(persisted["pages"][0]["removed_bubbles"][0]["id"], "b1");
    assert_eq!(persisted["pages"][0]["render_dirty"], false);
}

#[test]
fn export_requires_explicit_review_approval() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(8, 8, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let result = serve_editor(
        &image_path,
        json!({"schema_version":1,"pages":[{"id":"p1","image_path":"page.png","bubbles":[]}]}),
    )
    .unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, _) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let project =
        json!({"schema_version":1,"pages":[{"id":"p1","image_path":"page.png","bubbles":[]}]});
    assert!(
        request(
            host,
            &format!("/{token}/save"),
            "POST",
            Some(&project.to_string()),
            host
        )
        .starts_with("HTTP/1.1 200")
    );
    assert!(fukidashi_mcp::export::export_project(dir.path(), "zip").is_err());
    let approve = json!({"revision":result["review_revision"],"action":"approve_export","approved_pages":[0]});
    assert!(
        request(
            host,
            &format!("/{token}/review/submit"),
            "POST",
            Some(&approve.to_string()),
            host
        )
        .starts_with("HTTP/1.1 200")
    );
    assert!(fukidashi_mcp::export::export_project(dir.path(), "zip").is_ok());
}

#[tokio::test]
async fn consumed_review_approval_still_allows_export() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(8, 8, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let result = serve_editor(
        &image_path,
        json!({"schema_version":1,"pages":[{"id":"p1","image_path":"page.png","bubbles":[]}]}),
    )
    .unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, _) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let session = result["review_session_id"].as_str().unwrap().to_owned();
    let revision = result["review_revision"].as_u64().unwrap();
    let approve = json!({
        "revision": revision,
        "action": "approve_export",
        "approved_pages": [0]
    });
    assert!(
        request(
            host,
            &format!("/{token}/review/submit"),
            "POST",
            Some(&approve.to_string()),
            host,
        )
        .starts_with("HTTP/1.1 200")
    );
    let consumed = wait_for_review(&session, revision, 5).await.unwrap();
    assert_eq!(consumed["action"], "approve_export");
    assert!(fukidashi_mcp::export::export_project(dir.path(), "zip").is_ok());
}

#[test]
fn editor_persists_multiple_issues_and_normalizes_legacy_arrays() {
    let dir = tempdir().unwrap();
    let image_path = dir.path().join("page.png");
    ImageBuffer::<Rgb<u8>, _>::from_pixel(40, 30, Rgb([255, 255, 255]))
        .save(&image_path)
        .unwrap();
    let result = serve_editor(
        &image_path,
        json!({"schema_version":1,"pages":[{"id":"p1","image_path":"page.png"}]}),
    )
    .unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, _) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let edited = json!({
        "schema_version":1,
        "pages":[{"id":"p1","image_path":"page.png","correction_strokes":[{"mode":"cover","size":18,"points":[{"x":2,"y":3},{"x":4,"y":5}]}],"issues":[
            {"id":"i1","bbox":{"x1":1,"y1":2,"x2":8,"y2":10},"issue_type":"custom"},
            {"id":"i2","bbox":{"x1":12,"y1":14,"x2":20,"y2":22},"issue_type":"text_overflow"}
        ]}]
    });
    let response = request(
        host,
        &format!("/{token}/save"),
        "POST",
        Some(&edited.to_string()),
        host,
    );
    assert!(response.starts_with("HTTP/1.1 200"));
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("project.json")).unwrap()).unwrap();
    assert_eq!(persisted["pages"][0]["issues"].as_array().unwrap().len(), 2);
    assert!(persisted["pages"][0]["bubbles"].is_array());
    assert_eq!(
        persisted["pages"][0]["correction_strokes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let reloaded = request(host, &format!("/{token}/state"), "GET", None, host);
    let body = reloaded.split("\r\n\r\n").nth(1).unwrap();
    let reloaded: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(reloaded["pages"][0]["issues"].as_array().unwrap().len(), 2);
    assert!(reloaded["pages"][0]["bubbles"].is_array());
    assert_eq!(
        reloaded["pages"][0]["correction_strokes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn global_review_approval_requires_every_page_without_page_clicks() {
    let dir = tempdir().unwrap();
    let mut pages = Vec::new();
    for index in 0..140 {
        let name = format!("page-{index:03}.png");
        ImageBuffer::<Rgb<u8>, _>::from_pixel(2, 2, Rgb([255, 255, 255]))
            .save(dir.path().join(&name))
            .unwrap();
        pages.push(json!({"id":format!("p-{index}"),"image_path":name,"bubbles":[],"issues":[]}));
    }
    let first = dir.path().join("page-000.png");
    let result = serve_editor(&first, json!({"schema_version":1,"pages":pages})).unwrap();
    let endpoint = result["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap();
    let (host, _) = endpoint.split_once('/').unwrap();
    let token = result["session_token"].as_str().unwrap();
    let partial = json!({
        "revision":result["review_revision"],
        "action":"approve_export",
        "approved_pages":(0..139).collect::<Vec<_>>()
    });
    assert!(
        request(
            host,
            &format!("/{token}/review/submit"),
            "POST",
            Some(&partial.to_string()),
            host
        )
        .starts_with("HTTP/1.1 400")
    );
    let all = json!({
        "revision":result["review_revision"],
        "action":"approve_export",
        "approved_pages":(0..140).collect::<Vec<_>>()
    });
    assert!(
        request(
            host,
            &format!("/{token}/review/submit"),
            "POST",
            Some(&all.to_string()),
            host
        )
        .starts_with("HTTP/1.1 200")
    );
}
