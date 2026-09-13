//! In-process page re-render.
//!
//! Delegates to `fukidashi_mcp::editor::render_editor_page`, which runs the
//! *exact* same logic the loopback `/render` endpoint used — brush correction,
//! source-bubble restoration, bundled-font materialization, and the
//! `project.json` writeback — so the headless and native editors are
//! indistinguishable to downstream export.

use std::path::Path;

use fukidashi_mcp::editor::render_editor_page;

use super::state::EditorState;

/// Result returned by the renderer, including the server-normalized project
/// snapshot that was persisted alongside the PNG. The native editor applies
/// this value on the UI thread after the worker completes.
pub struct RerenderResult {
    pub rendered_image_path: std::path::PathBuf,
    pub state: serde_json::Value,
}

/// Re-render `page_index` and return the path to the freshly written PNG.
///
/// The function writes both the rendered PNG and an updated `project.json`
/// (bumping `state_revision`, clearing `render_dirty`). The caller should reload
/// the page texture from the returned path.
pub fn rerender_page(
    state: &EditorState,
    page_index: usize,
    image_path: &Path,
) -> anyhow::Result<std::path::PathBuf> {
    Ok(rerender_page_with_state(state, page_index, image_path)?.rendered_image_path)
}

/// Re-render a page and retain the normalized state returned by the renderer.
/// Keeping this separate from the legacy path-returning helper lets the
/// asynchronous native editor carry the incremented state revision forward
/// between serial page renders.
pub fn rerender_page_with_state(
    state: &EditorState,
    page_index: usize,
    image_path: &Path,
) -> anyhow::Result<RerenderResult> {
    let result = render_editor_page(&state.job_dir, image_path, &state.value, page_index)?;
    let rendered: std::path::PathBuf = result
        .get("rendered_image_path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("render produced no rendered_image_path"))?;
    let normalized_state = result
        .get("state")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("render produced no normalized editor state"))?;
    // `render_editor_page` already wrote project.json; resolve a job-relative
    // path to an absolute one for texture loading.
    let resolved = if rendered.is_absolute() {
        rendered
    } else {
        state.job_dir.join(&rendered)
    };
    Ok(RerenderResult {
        rendered_image_path: resolved,
        state: normalized_state,
    })
}
