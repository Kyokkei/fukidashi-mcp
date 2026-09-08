use fukidashi_mcp::{
    config::Config,
    mcp::{EditorRequest, FukidashiServer},
};
use fukidashi_mcp::{
    domain::{Bubble, Project, ProjectPage, Rect},
    vision::{inpaint, preprocess, segment},
};
use rmcp::{ClientHandler, ServiceExt, model::ClientInfo};
use std::path::PathBuf;

#[test]
fn rect_validation_and_clipping_are_finite_and_half_open() {
    let rect = Rect {
        x1: -2.0,
        y1: 4.0,
        x2: 11.0,
        y2: 12.0,
    };
    let clipped = rect.clip(10.0, 10.0).expect("nonempty clipped rectangle");
    assert_eq!(
        clipped,
        Rect {
            x1: 0.0,
            y1: 4.0,
            x2: 10.0,
            y2: 10.0
        }
    );
    assert!(
        Rect {
            x1: 1.0,
            y1: 1.0,
            x2: 1.0,
            y2: 2.0
        }
        .validate()
        .is_err()
    );
    assert!(
        Rect {
            x1: f32::NAN,
            y1: 1.0,
            x2: 2.0,
            y2: 2.0
        }
        .validate()
        .is_err()
    );
}

#[test]
fn project_schema_and_bubble_validation() {
    let project = Project {
        schema_version: 1,
        pages: vec![ProjectPage {
            id: "p1".into(),
            image_path: "pages/1.png".into(),
            bubbles: vec![Bubble {
                id: "b1".into(),
                bbox: Rect {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 10.0,
                    y2: 10.0,
                },
                text: "こんにちは".into(),
                translation: Some("Hello".into()),
                confidence: 0.9,
                reading_order: 0,
            }],
        }],
        title: None,
        language: Some("en".into()),
        glossary: None,
    };
    project.validate().unwrap();
    assert!(
        serde_json::to_value(&project).unwrap()["pages"][0]["bubbles"][0]["text"] == "こんにちは"
    );
}

#[test]
fn preprocessing_preserves_layout_contracts() {
    assert_eq!(preprocess::round_ties_even(2.5), 2);
    assert_eq!(preprocess::round_ties_even(3.5), 4);
    let dims = preprocess::db_dimensions(960, 480, 960, "min").unwrap();
    assert_eq!((dims.0, dims.1), (1920, 960));
}

#[test]
fn mask_dilation_and_inpainting_preserve_unmasked_bytes() {
    let mask = vec![0, 0, 0, 0, 1, 0, 0, 0, 0];
    assert_eq!(segment::dilate(&mask, 3, 3, 1), vec![1; 9]);
    assert_eq!(inpaint::modulo_padding(17, 18, 8), (7, 6));
    let original = vec![1, 2, 3, 4, 5, 6];
    let generated = vec![9, 9, 9, 8, 8, 8];
    let result = inpaint::compose(&original, &generated, &[0, 1], 3).unwrap();
    assert_eq!(result, vec![1, 2, 3, 8, 8, 8]);
}

#[tokio::test]
async fn rmcp_surface_lists_tools_and_reports_tool_errors() -> anyhow::Result<()> {
    let config = Config {
        storage_root: PathBuf::from(r"C:\storage"),
        models_dir: PathBuf::from(r"C:\models"),
        ort_dylib: None,
        config_file: PathBuf::from(r"C:\storage\config.json"),
        configured_jobs_dir: None,
        configured_cache_dir: None,
        configured_temp_dir: None,
        configured_runtime_dir: None,
        configured_exports_dir: None,
        configured_font_dirs: Vec::new(),
        configured_provider: None,
        storage_source: "test".into(),
        models_source: "test".into(),
        ort_source: "test".into(),
    };
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        FukidashiServer::new(config)?
            .serve(server_io)
            .await?
            .waiting()
            .await?;
        anyhow::Ok(())
    });
    let client = TestClient.serve(client_io).await?;
    let tools = client.list_tools(Default::default()).await?;
    let names: Vec<_> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert!(names.contains(&"fukidashi_analyze_page"));
    assert!(names.contains(&"fukidashi_clean_page"));
    assert!(names.contains(&"fukidashi_release_models"));
    assert!(names.contains(&"fukidashi_typeset"));
    assert!(names.contains(&"fukidashi_translation_start"));
    assert!(names.contains(&"fukidashi_translation_submit"));
    assert!(names.contains(&"fukidashi_serve_editor"));
    assert!(names.contains(&"fukidashi_wait_for_review"));
    assert!(names.contains(&"fukidashi_export"));
    for tool in &tools.tools {
        let schema = serde_json::to_value(&tool.input_schema)?;
        assert_no_boolean_schema_nodes(&schema, tool.name.as_ref());
    }
    let editor_schema = tools
        .tools
        .iter()
        .find(|tool| tool.name == "fukidashi_serve_editor")
        .expect("editor tool schema")
        .input_schema
        .clone();
    let editor_schema = serde_json::to_value(editor_schema)?;
    assert_eq!(editor_schema["properties"]["json_data"]["type"], "object");
    assert!(editor_schema["properties"]["job_path"].is_object());
    assert!(editor_schema["properties"]["job_id"].is_object());
    let strict_submit = tools
        .tools
        .iter()
        .find(|tool| tool.name == "fukidashi_translation_submit")
        .expect("strict submit tool schema");
    let strict_submit_schema = serde_json::to_value(strict_submit.input_schema.clone())?;
    assert!(strict_submit_schema["properties"]["work_token"].is_object());
    assert!(strict_submit_schema["properties"]["translations"].is_object());
    let editor_request: EditorRequest = serde_json::from_value(serde_json::json!({
        "image_path": "C:/page.png",
        "json_data": {"schema_version": 1, "flags": [true, false], "nested": {"value": null}}
    }))?;
    assert_eq!(editor_request.json_data.unwrap()["flags"][0], true);
    let editor_without_state: EditorRequest = serde_json::from_value(serde_json::json!({
        "image_path": "C:\\jobs\\rendered.png"
    }))
    .unwrap();
    assert!(editor_without_state.json_data.is_none());
    let reopen: EditorRequest = serde_json::from_value(serde_json::json!({
        "job_path": "E:\\ComicTranslate\\jobs\\1b5c2f14ee4a4b469353b3786fdf1025"
    }))?;
    assert!(reopen.image_path.is_none());
    assert_eq!(
        reopen.job_path.as_deref(),
        Some("E:\\ComicTranslate\\jobs\\1b5c2f14ee4a4b469353b3786fdf1025")
    );
    let reopen_by_id: EditorRequest = serde_json::from_value(serde_json::json!({
        "job_id": "1b5c2f14ee4a4b469353b3786fdf1025"
    }))?;
    assert_eq!(
        reopen_by_id.job_id.as_deref(),
        Some("1b5c2f14ee4a4b469353b3786fdf1025")
    );
    let release = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("fukidashi_release_models")
                .with_arguments(serde_json::Map::new()),
        )
        .await?;
    assert_eq!(release.is_error, Some(false));
    let result = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("fukidashi_analyze_page").with_arguments(
                serde_json::json!({"image_path":"relative.png"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert_eq!(result.is_error, Some(true));
    client.cancel().await?;
    server_task.await??;
    Ok(())
}

fn assert_no_boolean_schema_nodes(value: &serde_json::Value, tool_name: &str) {
    match value {
        serde_json::Value::Bool(_) => {
            panic!("tool {tool_name} contains a boolean JSON Schema node")
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_no_boolean_schema_nodes(item, tool_name);
            }
        }
        serde_json::Value::Object(properties) => {
            for item in properties.values() {
                assert_no_boolean_schema_nodes(item, tool_name);
            }
        }
        serde_json::Value::Null | serde_json::Value::Number(_) | serde_json::Value::String(_) => {}
    }
}

#[derive(Debug, Clone, Default)]
struct TestClient;
impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}
