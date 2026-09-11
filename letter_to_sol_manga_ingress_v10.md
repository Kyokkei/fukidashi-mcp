# Memorandum V10: Fukidashi Editor UX Overhaul, Interaction Hardening & Typesetter Safeguards
**From:** Antigravity (Field Supervisor) & User (Human QA Lead)  
**To:** GPT-5.6 Sol (xhigh) & Luna  
**Workspace:** `D:\coding\fukidashi-mcp`  
**Target Binaries:** `fukidashi-editor` (`src/bin/editor/`) & `fukidashi-mcp` (`src/typeset/`)

---

## 1. Executive Summary & Human Field Report

The V9.1 sprint successfully integrated the native Photopea-style left toolbar and Patrick Hand font fallback into `fukidashi-editor`. However, real-world Human QA testing on live chapter `nhentest_v10` immediately surfaced **critical interaction deadlocks**, **invisible UI assets**, and a **fatal typesetter crash** that completely locks the operator out of approving or exporting.

### Verbatim Field Report from Human QA:

> **"QA report from me:**
> 
> **# Screenshot 1:**
> - *Oh god, if the button asset is black, AT LEAST let the left bar color something else (dont touch the grid preview thing) CYAN, why not cyan but black?*
> - *Please get rid of the buttons: Zoom (we already have Ctrl + Scroll); Pan (remove this, NOT even working, not worth fixing).*
> - *Ctrl+Z while dragging doesn't work (nor Ctrl+Y).*
> - *If we can ADD text box, PLEASE let me remove it, resize it, drag it too 💀 APPLY same for bubble tool (the one we draw the dialogue box). Mf I can create it but can't undo or even delete or resize it!*
> - *Remove the font path box on the right side (don't even use it at all, it makes the egui harder to edit for newbies).*
> 
> **# Screenshots 2 & 3:**
> - *Error: render page 0 failed: fit text for bubble 1*
> - *Error: approval blocked until all dirty pages render successfully: page 1 could not be re-rendered: fit text for bubble 1*
> 
> **Pack these to V10 letter, ima give this to Sol. Bug is heavy."**

---

## 2. Technical Root Causes & Mathematical Proofs

### Defect 1: The "Black Hole" Toolbar Icons (Multiplicative Tint Fallacy)
* **File:** `src/bin/editor/toolbar.rs:38-50, 95-99`
* **Root Cause:**
  The Photopea asset icons (`move.png`, `brush.png`, `htype.png`, etc.) are transparent PNGs with **pure black RGB channels** `(0, 0, 0, Alpha)`.
  In `toolbar.rs:98`, `ui.painter().image` passes a tint color:
  ```rust
  ui.painter().image(tex.id(), icon_rect, uv, if active { Color32::WHITE } else { Color32::from_gray(190) });
  ```
  In graphics pipelines, texture tinting is **multiplicative**:
  $$\text{Rendered RGB} = \text{Texture RGB} \times \text{Tint RGB} = 0 \times \text{Tint} = 0$$
  The rendered pixels remain pitch black regardless of the tint. Because the editor uses a dark background (`#141414`), the icons are completely invisible dark-on-dark silhouettes.
* **Architectural Fix:**
  In `load_png_texture`, convert the texture mask upon loading: normalize all non-zero alpha pixel RGB channels to `255` (pure white mask). 
  Once the texture is white `(255, 255, 255, A)`, egui's tint parameter will accurately colorize the icons:
  - Inactive: `Color32::from_gray(200)` (crisp, readable silver-white).
  - Active: `Color32::from_rgb(0, 229, 255)` (vibrant Cyan) on a subtle cyan-highlighted background tile (`Color32::from_rgba_unmultiplied(0, 229, 255, 40)`).

---

### Defect 2: Toolbar Pruning — Purge Redundant Zoom & Pan
* **File:** `src/bin/editor/toolbar.rs` & `src/bin/editor/canvas.rs`
* **Root Cause:**
  1. Zoom is fully handled by `Ctrl + Scroll` and canvas pinch gestures. The toolbar button is redundant screen clutter.
  2. The dedicated Pan tool button is non-functional and fighting with Middle-Click Drag / Space+Drag.
* **Architectural Fix:**
  - Remove `ActiveTool::Pan` and `ActiveTool::Zoom` from `ActiveTool` enum in `canvas.rs`.
  - Remove `hand.png` and `zoom.png` from `ICONS` and `TOOLS` in `toolbar.rs`.
  - Retain `Ctrl + Scroll` for canvas zoom and Middle-Click / Space-Drag for viewport pan in `canvas.rs`.

---

### Defect 3: Missing Bubble / Text Lifecycle (No Delete, No Resize, No Undo)
* **File:** `src/bin/editor/state.rs`, `src/bin/editor/canvas.rs`, & `src/bin/editor/inspector.rs`
* **Root Cause:**
  1. **Zero Deletion Mechanism:** `push_new_bubble` was added in V9.1, but **no corresponding `delete_bubble` or `remove_bubble` helper exists anywhere in `EditorState`**. Once a bubble is placed, it is permanently etched into `project.json`.
  2. **No Undo for Placement:** `Ctrl+Z` only calls `undo_stroke()` (which only pops brush strokes). Adding a bubble or text box does not record an undo history entry.
  3. **Resize / Drag Deadlock on New Bubbles:** Newly created bubbles do not always establish the 8 resize handles properly in the frame following creation, preventing the user from resizing or repositioning them immediately.
* **Architectural Fix:**
  1. Add `EditorState::delete_bubble(&mut self, page_index: usize, bubble_index: usize)`:
     - Removes the bubble from `pages[page_index]["bubbles"]`.
     - Appends a tombstone record to `pages[page_index]["removed_bubbles"]` (preserving OCR provenance and cleanly restoring background pixels on render).
     - Calls `self.mark_page_dirty(page_index)`.
  2. Add a prominent **`🗑 Delete Bubble`** button in `bubble_inspector` (in `inspector.rs`).
  3. Listen for `Key::Delete` and `Key::Backspace` in `mod.rs` (when `!text_focus` and a bubble is selected) to delete the selected bubble.
  4. Allow `Key::Escape` or `Ctrl+Z` while dragging (`DragState::DrawNewBubble`) to abort creation immediately without writing to state.

---

### Defect 4: Inspector Decluttering — Remove Job-Level Font Path
* **File:** `src/bin/editor/inspector.rs:37-58`
* **Root Cause:**
  The `Font Path` input box and file browse dialog at the top of the inspector are never used by operators, crowd the panel, and confuse beginners who expect font selection to be automatic or per-bubble.
* **Architectural Fix:**
  Remove the `Font Path` UI section completely from `inspector.rs`. The underlying bundled Patrick Hand / Comic Neue fallbacks in the MCP backend handle full Unicode without manual file picking.

---

### Defect 5: FATAL TYPESETTER OVERFLOW & EXPORT LOCKOUT (`fit text for bubble N`)
* **Files:** `src/typeset/layout.rs:880-972`, `src/typeset/mod.rs:150-162`, `src/bin/editor/state.rs`
* **Field Evidence:**
  In `nhentest_v10` page 0, bubble 1 was inspected directly:
  ```json
  "bbox": { "x1": 1128.74, "y1": 869.10, "x2": 1130.74, "y2": 871.10 },
  "bubble_bbox": { "x1": 1128.74, "y1": 869.10, "x2": 1130.74, "y2": 871.10 },
  "translation": "slave"
  ```
  **$\text{Width} = 2.0\text{px}$, $\text{Height} = 2.0\text{px}$.**
* **The Crash Chain:**
  1. An accidental click or malformed detector artifact created a 2×2 pixel collapsed bounding box.
  2. `fit_text_with_font_candidates_masked` runs `inset_rect(area, padding)`. Padding clamps to $4.0\text{px}$. An inset of $4\text{px}$ on a $2\text{px}$ box yields negative geometry, or the iterative font solver fails because `"slave"` cannot fit into 2px at minimum font size ($12\text{px}$).
  3. The layout engine throws `TextOverflow: supplied text does not fit at minimum font size 12.0px`.
  4. In `fukidashi-editor`, `rerender_page_at` fails with `Error: render page 0 failed: fit text for bubble 1`.
  5. When the user clicks `Approve & Export`, `approve_and_export` blocks with:
     > `Error: approval blocked until all dirty pages render successfully: page 1 could not be re-rendered: fit text for bubble 1`
  6. Because Defect 3 prevented deleting bubbles, the user was **permanently locked out from exporting the chapter**.
* **Architectural Fix (Two-Tier Defense):**
  1. **UI Layer Creation Guard (`state.rs` & `canvas.rs`):**
     In `push_new_bubble` and `DragState::DrawNewBubble` release, enforce minimum bubble dimensions:
     $$\text{Width} \ge 30.0\text{px}, \quad \text{Height} \ge 20.0\text{px}$$
     If a drag release yields $< 15\text{px}$ in either axis, discard it as an accidental misclick.
  2. **Typesetter Robustness (`src/typeset/mod.rs` & `layout.rs`):**
     If a bubble's bounding box is degenerate ($w < 8.0$ or $h < 8.0$) or text fitting fails with `TextOverflow`:
     Do **NOT** crash the entire page render! Log an error warning, skip rasterizing the overflowing text (or render a zero-size ink box), mark the bubble as an issue/warning, and allow the remaining valid bubbles and pages to render cleanly so export is not held hostage.

---

## 3. Implementation Blueprint for Sol

### Component 1: Invert Mask for Black PNG Icons (`toolbar.rs`)
```rust
fn load_png_texture(ctx: &egui::Context, name: &str, bytes: &[u8]) -> TextureHandle {
    match image::load_from_memory(bytes).map(|img| img.to_rgba8()) {
        Ok(mut rgba) => {
            // Photopea PNG icons are black with alpha (0, 0, 0, A).
            // Convert RGB to 255 so egui multiplicative tinting works cleanly.
            for pixel in rgba.chunks_exact_mut(4) {
                pixel[0] = 255;
                pixel[1] = 255;
                pixel[2] = 255;
            }
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color_image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            ctx.load_texture(name, color_image, TextureOptions::LINEAR)
        }
        Err(_) => {
            let color_image = ColorImage::from_rgba_unmultiplied([1, 1], &[120, 120, 120, 255]);
            ctx.load_texture(name, color_image, TextureOptions::LINEAR)
        }
    }
}
```

### Component 2: Pruned Tool Strip & Cyan Highlights (`toolbar.rs`)
```rust
static TOOLS: &[ToolDef] = &[
    ToolDef { tool: super::canvas::ActiveTool::Select,      icon: "move",       tooltip: "Select / Move / Resize", hotkey: "V" },
    ToolDef { tool: super::canvas::ActiveTool::DrawBubble,  icon: "ellipse",    tooltip: "Draw Bubble (Drag)",     hotkey: "O" },
    ToolDef { tool: super::canvas::ActiveTool::AddText,     icon: "htype",      tooltip: "Add Text (Click)",       hotkey: "T" },
    ToolDef { tool: super::canvas::ActiveTool::Brush,       icon: "brush",      tooltip: "Brush (Cover)",          hotkey: "B" },
    ToolDef { tool: super::canvas::ActiveTool::Eraser,      icon: "eraser",     tooltip: "Eraser (Restore)",       hotkey: "E" },
    ToolDef { tool: super::canvas::ActiveTool::Eyedropper,  icon: "eyedropper", tooltip: "Eyedropper (Sample)",    hotkey: "I" },
];

const ACTIVE_COLOR: Color32 = Color32::from_rgb(0, 229, 255); // Neon Cyan

// In toolbar_ui:
let icon_tint = if active {
    Color32::from_rgb(0, 229, 255)
} else if response.hovered() {
    Color32::WHITE
} else {
    Color32::from_gray(180)
};
```

### Component 3: Bubble Deletion Helper (`state.rs`)
```rust
pub fn delete_bubble(&mut self, page_index: usize, bubble_index: usize) -> bool {
    let Some(page) = self
        .value
        .get_mut("pages")
        .and_then(|p| p.as_array_mut())
        .and_then(|p| p.get_mut(page_index))
    else {
        return false;
    };

    let Some(bubbles) = page.get_mut("bubbles").and_then(|b| b.as_array_mut()) else {
        return false;
    };

    if bubble_index >= bubbles.len() {
        return false;
    }

    let removed = bubbles.remove(bubble_index);

    // Record tombstone so renderer can restore original source pixels if needed
    let removed_entry = serde_json::json!({
        "id": removed.get("id").cloned().unwrap_or(serde_json::Value::Null),
        "bbox": removed.get("bbox").cloned().unwrap_or(serde_json::Value::Null),
        "source_text": removed.get("text").cloned().unwrap_or(serde_json::Value::Null),
        "translation": removed.get("translation").cloned().unwrap_or(serde_json::Value::Null),
        "removed_reason": "operator_deleted"
    });

    let removed_array = page
        .entry("removed_bubbles")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut();

    if let Some(arr) = removed_array {
        arr.push(removed_entry);
    }

    self.mark_page_dirty(page_index);
    true
}
```

### Component 4: Delete UI & Key Listener (`inspector.rs` & `mod.rs`)
In `src/bin/editor/inspector.rs` (`bubble_inspector`):
```rust
ui.horizontal(|ui| {
    if ui.button("Apply").clicked() || changed {
        // ... apply translation ...
    }
    if ui.button("⚡ Re-render Page").clicked() {
        self.rerender_current_page();
    }
    if ui.button("🗑 Delete Bubble").clicked() {
        if let Some(state) = self.state.as_mut() {
            state.delete_bubble(page_index, bubble_index);
        }
        self.canvas.selected = None;
        self.schedule_save();
    }
});
```

In `src/bin/editor/mod.rs` key handler:
```rust
egui::Key::Delete | egui::Key::Backspace
    if tool_shortcuts_allowed(text_focus) && *modifiers == egui::Modifiers::NONE =>
{
    if let Some((pi, bi)) = self.canvas.selected {
        if let Some(state) = self.state.as_mut() {
            state.delete_bubble(pi, bi);
        }
        self.canvas.selected = None;
        self.schedule_save();
    }
}
```

### Component 5: Typesetter Safeguard on Degenerate Rects (`src/typeset/mod.rs`)
In `typeset_page`:
```rust
let w = rect.x2 - rect.x1;
let h = rect.y2 - rect.y1;
if w < 8.0 || h < 8.0 {
    eprintln!("Warning: skipping degenerate bubble {index} (size: {w:.1}x{h:.1})");
    continue;
}

let layout = match fit_text_with_font_candidates_masked(...) {
    Ok(l) => l,
    Err(err) => {
        eprintln!("Warning: text fitting failed for bubble {index}: {err}. Skipping text rasterization.");
        continue;
    }
};
```

---

## 4. Files to Touch

| File | Changes Required |
|---|---|
| [`src/bin/editor/toolbar.rs`](file:///D:/coding/fukidashi-mcp/src/bin/editor/toolbar.rs) | Invert black PNG RGB to 255; add Cyan active tint; remove `Pan` and `Zoom` definitions. |
| [`src/bin/editor/canvas.rs`](file:///D:/coding/fukidashi-mcp/src/bin/editor/canvas.rs) | Remove `ActiveTool::Pan` and `ActiveTool::Zoom`; enforce min $30\times 20$ size on `DrawNewBubble`; fix handle hit-testing on new bubbles. |
| [`src/bin/editor/state.rs`](file:///D:/coding/fukidashi-mcp/src/bin/editor/state.rs) | Add `delete_bubble` method with tombstone tracking; clamp minimum bbox size in `push_new_bubble`. |
| [`src/bin/editor/inspector.rs`](file:///D:/coding/fukidashi-mcp/src/bin/editor/inspector.rs) | Delete `Font Path` controls; add `🗑 Delete Bubble` button to `bubble_inspector`. |
| [`src/bin/editor/mod.rs`](file:///D:/coding/fukidashi-mcp/src/bin/editor/mod.rs) | Bind `Key::Delete` / `Backspace` to `delete_bubble`; cancel drag on Escape / Ctrl+Z. |
| [`src/typeset/mod.rs`](file:///D:/coding/fukidashi-mcp/src/typeset/mod.rs) | Skip degenerate bubbles (<8px); do not abort entire page typeset on single-bubble `TextOverflow`. |

---

## 5. Verification Protocol

```powershell
# 1. Compile and run test suite
cargo check --features editor --bin fukidashi-editor
cargo test --features editor

# 2. Release build and deployment
cargo build --release --features editor
Copy-Item "target\release\fukidashi-mcp.exe", "target\release\fukidashi-editor.exe" "$env:LOCALAPPDATA\Fukidashi\bin\" -Force

# 3. Acceptance test on live failure case:
fukidashi-editor E:\Fukidashi\jobs\nhentest_v10--5e9777852472fea7
# - Verify toolbar icons are sharp white/cyan (no black-on-black).
# - Verify Zoom & Pan icons are gone.
# - Click bubble 1 (the 2x2 "slave" box) -> Click "🗑 Delete Bubble" (or press Delete).
# - Hit Ctrl+S (Save & Render) -> Page 0 renders cleanly with zero errors!
# - Hit Approve & Export -> Approval completes without blocking!
```
