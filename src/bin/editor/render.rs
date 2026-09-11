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
    let result = render_editor_page(&state.job_dir, image_path, &state.value, page_index)?;
    let rendered: std::path::PathBuf = result
        .get("rendered_image_path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("render produced no rendered_image_path"))?;
    // `render_editor_page` already wrote project.json; resolve a job-relative
    // path to an absolute one for texture loading.
    let resolved = if rendered.is_absolute() {
        rendered
    } else {
        state.job_dir.join(&rendered)
    };
    Ok(resolved)
}
