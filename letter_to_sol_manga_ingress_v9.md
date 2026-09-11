# Memorandum V9: Native `egui` Editor Hardening & Font Unification (Audited & Fact-Checked)
**From:** Antigravity (Field Supervisor) & User (Human QA Lead)  + claude sonnet 4.6 fact checked 
**To:** gpt 5.6 sol xhigh and gpt 5.6 luna xhigh 
**Workspace:** `D:\coding\fukidashi-mcp`  
**Target Binaries:** `fukidashi-editor` (`src/bin/editor/`) & `fukidashi-mcp` (`src/typeset/`)

---

## 1. Executive Summary & Field Report

The foundation has been laid: the brittle 55KB vanilla DOM `editor.html` has been dethroned. We have a native, GPU-accelerated Rust `eframe`/`egui` companion binary (`fukidashi-editor.exe`) compiling cleanly and communicating via `review.json` IPC.

However, real-world Human QA testing on live manga chapter `nhentest_v9` has exposed critical interaction deadlocks.

Here is the verbatim field report from the human operator:

> **"Wuba luba dub dub, no more html5, egui here we ball**
> 
> **- i have created the foundation for u to code Sol, IT SHIT yes i know but i think it would be better than plain html anyway**
> 
> **SOOOOO BUG report from field tested humanQA (not AI QA):**
> 
> **+ brush tool PHYSICALLY THERE, but the LOGIC, the shortcut nothing work THUS I DRAW one direction, the line cameout different direction**
> **+ NOTHING WORK IN THIS LMAO, drag? Nope not working, REsize font size? NOPE**
> **+ for dragging: here what i mean:**
> 
> **i take the text zone (idk if it BLUE cyan or not but i just wanna grap that text, can size it up or place it on other place**
> 
> **export BUTTON? i never have a chance to test it, i cannot tell u if that shit broke or not**
> 
> **small minor bug:**
> **model seem to mixing bold and normal font into 1 bubble, just use normal only"**

---

## 2. Fact-Checked Technical Triage (Sol Audit Incorporated)

### Defect 1: Bubble Drag & Resize Deadlock (The "Two-Click Penalty")
* **Status:** ✅ **100% Confirmed**
* **File:** `src/bin/editor/canvas.rs:349-385`
* **Root Cause:**
  In `handle_canvas_input`, bubble translation and handle resizing are strictly gated behind `if let Some((pi, bi)) = self.canvas.selected`.
  `self.canvas.selected` is ONLY populated inside `if response.clicked()` (line 422).
  In `egui`, a click-drag gesture fires `dragged()` as **TRUE** and `clicked()` as **FALSE**.
  
  **Result:** Clicking and immediately dragging an unselected bubble is completely dead. The drag block skips because `selected` is `None`, and the click selection block never executes.
* **Sol's Task:**
  In `handle_canvas_input`, when `response.dragged()` starts with `self.canvas.drag == DragState::None`:
  If no bubble was selected, hit-test all bubbles on the current page (`hit_bubble`). If struck, immediately select it (`self.canvas.selected = Some((current_page, bi))`) and transition into `DragState::TranslateBubble` in the **very same frame**.

---

### Defect 2: Brush Mechanics, Ergonomics & Coordinate Tracking
* **Status:** ✅ **Audited & Clarified**
* **File:** `src/bin/editor/canvas.rs` & `src/bin/editor/inspector.rs`
* **Root Causes & Corrections:**
  1. **Variant Gating (Confirmed):** Brush painting requires `current_variant_is_cleaned()`. When the editor opens or re-renders, the user is looking at `Rendered`. Brush strokes are silently ignored.
     - *Fix:* Automatically flip `current_variant` to `Cleaned` whenever the Brush tool is activated.
  2. **Brush Direction Drift (Sol Correction):** 
     - The letter's initial claim about "dividing by zoom twice" was fact-checked as **incorrect**. The single zoom division is intentional adaptive screen-space sampling.
     - *Real Cause:* Brush disorientation was primarily a symptom of pan accumulation drift in `screen_to_image`. With pan fixed to direct delta accumulation (`+= drag_delta()`), coordinate tracking stays 1:1.
  3. **Zero Ergonomics (Confirmed):**
     - No hotkeys exist for brush operation.
     - No visual cursor ring indicates the brush radius over the canvas.
* **Sol's Task:**
  - Wire shortcuts: `B` (toggle Brush & switch to Cleaned), `V` (Select tool / exit brush), `[` and `]` (shrink / grow brush radius).
  - Add a live circular cursor ring following the pointer over the canvas when the brush tool is active (`painter.circle_stroke`).

---

### Defect 3: Font Size Feedback Disconnect
* **Status:** ✅ **100% Confirmed**
* **File:** `src/bin/editor/inspector.rs:177-196`
* **Root Cause:**
  Adjusting the font size slider in `bubble_inspector` updates `project.json` and marks `bubble.render_dirty = true`, but does **not** trigger a re-render. The user continues viewing the stale rendered bitmap, creating the impression that font sizing is broken.
  Furthermore, the only "Re-render page" button is buried at the very bottom of the Review section.
* **Sol's Task:**
  - Place a prominent **"⚡ Re-render Page"** button directly inside the `bubble_inspector` section (right below the Font Size / Padding controls).
  - Show an amber `⚠️ Modified — click Re-render to preview` badge next to it when `bubble.render_dirty` is true.

---

### Defect 4: Font Weight / Style Inconsistency in Dialogue
* **Status:** ✅ **Audited & Re-diagnosed**
* **File:** `src/typeset/layout.rs` & `src/typeset/mod.rs`
* **Sol's Audit Finding:**
  - `Comic Neue Bold` is **not** in the default dialogue fallback cascade (it is strictly opt-in under `BundledFontRole::Emphasis`).
  - *Actual Cause:* The job's primary font is `ComicNeue-Regular.ttf`. Comic Neue lacks Vietnamese diacritics (`ờ`, `ề`, `ắ`, `ị`). When rendering Vietnamese, Latin characters match Comic Neue (round, casual comic styling), but accented characters fall back to system fonts (e.g. `segoeui.ttf`), creating a jarring stylistic clash where letters appear to alternate between different weights and stem thicknesses in the same bubble.
* **Sol's Task:**
  - Enforce **Whole-Bubble Font Unification**: When text contains Vietnamese diacritics, shape the **entire bubble** using a single unified font that provides full coverage (e.g., `Patrick Hand Regular` or system Vietnamese sans), rather than splintering individual words across mismatched typefaces.

---

## 3. Revised Action Plan for Sol

1. **`canvas.rs`**:
   - Single-touch bubble drag (hit-test and transition in one frame).
   - Live brush cursor ring indicator.
   - Cursor icon feedback (`Grab`, `Grabbing`, `Crosshair`, `ResizeNwSe`).
2. **`mod.rs` & `inspector.rs`**:
   - Hotkeys: `B`, `V`, `[`, `]`.
   - Auto-switch to `Cleaned` variant on brush activation.
   - Immediate "⚡ Re-render Page" button inside `bubble_inspector`.
3. **`typeset/layout.rs`**:
   - Ensure entire dialogue bubbles unify under a single covering face for Vietnamese text.

*End Memorandum V9 (Revised).*
