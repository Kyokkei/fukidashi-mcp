<div align="center">

# 吹 Fukidashi MCP

### Autonomous, 100% Local Manga Scanlation Studio for AI Agents

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Rust: 2024 Edition](https://img.shields.io/badge/Rust-2024_Edition-orange.svg)](https://www.rust-lang.org/)
[![Platform: Windows x64](https://img.shields.io/badge/Platform-Windows_x64-0078D6.svg)](#-quickstart)
[![Linux & macOS](https://img.shields.io/badge/Platform-Linux%20%7C%20macOS-FCC624.svg)](#-linux--macos-from-source)
[![Protocol: MCP v3.2](https://img.shields.io/badge/MCP-v3.2.0-purple.svg)](https://modelcontextprotocol.io/)
[![100% Local Vision Core](https://img.shields.io/badge/Vision_Core-100%25_Offline-success.svg)](#-battle-tested-hardware--verified-models)

**Fukidashi** turns **OpenAI Codex**, **Claude Code**, **Claude Desktop**, and **Antigravity** into an autonomous, professional manga localization studio running on your local machine.

*Zero cloud subscriptions for vision. Zero cloud fees for cleaning. 100% private OCR, inpainting, and typesetting on consumer GPU/CPU.*

<br/>

<div align="center">
  <a href="#-demo-pipeline"><b>Demo</b></a> •
  <a href="#-the-problem-vs-the-fukidashi-solution"><b>Why Fukidashi</b></a> •
  <a href="#-system-architecture"><b>Architecture</b></a> •
  <a href="#-one-prompt-codex-scanlation"><b>Codex Prompt</b></a> •
  <a href="#-features"><b>Features</b></a> •
  <a href="#-quickstart"><b>Quickstart</b></a>
</div>

</div>

<br/>

> 🤖 **AI Agents & Crawlers:** Fukidashi is designed for autonomous tool chaining via Model Context Protocol (MCP v3.2). See the [Codex One-Shot Workflow](#-one-prompt-codex-scanlation) for prompt recipes.

---

## 📸 Demo Pipeline

From raw scan to publication-ready localized comic in one autonomous pipeline:

<div align="center">
  <img src="assets/pipeline-preview.png" alt="Fukidashi Pipeline Showcase: Before, Clean Bubbles, and Translated" width="100%">
  <p><i>1. Raw comic page ➔ 2. Neural RT-DETR detection & LaMa deep inpainting ➔ 3. Agentic translation with smart Vietnamese typesetting (Patrick Hand font).</i></p>
</div>

---

## 📖 The Problem vs. The Fukidashi Solution

* ❌ **The Old Way**: You manually take screenshots, feed them one by one into web chatbots, copy translated text into Photoshop, spend 4 hours clone-stamping screentones, and manually re-adjust text boxes.
* ⚡ **The Fukidashi Way**: Give your AI agent a single prompt:
  > *"Pull the latest chapter of Dandadan and localize it into Vietnamese using Patrick Hand."*
  
  Fukidashi auto-fetches the chapter, detects speech bubbles, deeply inpaints backgrounds with LaMa, transcribes dialogue via local OCR, translates via your agent, typesets with dynamic box-fitting, and opens the native desktop QA editor for review and CBZ export.

---

## 🏗️ System Architecture

Fukidashi orchestrates a zero-cloud local vision pipeline with your AI agent acting as the contextual translation brain:

```mermaid
flowchart TD
    RAW["📥 MangaDex Pull / Local Raw"] --> DET["🎯 RT-DETR (Bubble & Line Detection)"]

    %% Left Branch: Visual Cleaning
    DET -->|"Bubble Masks"| LAMA["🧹 LaMa AI Engine<br/>(Deep Inpainting)"]
    LAMA -->|"Clean Canvas"| GUI["🖥️ Native egui QA Editor<br/>(60fps Side-by-Side Review & Brush)"]

    %% Right Branch: Text Recognition, Translation & Typesetting
    DET -->|"Text Bounding Boxes"| OCR["🔍 Local Baberu / Manga-OCR<br/>(Multilingual Transcription)"]
    OCR --> CODEX["🧠 OpenAI Codex / Agent<br/>(Context-Aware Translation)"]
    LORE["🛡️ Lore & Pronouns<br/><code>fukidashi_put_lore</code>"] <--> CODEX
    CODEX --> TYPE["✍️ Smart Typesetting<br/>(Dynamic Box-Fitting & Vietnamese Fonts)"]
    TYPE -->|"Typeset Overlay"| GUI

    %% Final Export
    GUI --> OUT["📦 Export Localized CBZ / ZIP Archive"]
```

---

## 💬 One-Prompt Codex Scanlation

Open **OpenAI Codex**, **Claude Code**, or **Antigravity**, and paste:

```text
Translate the manga chapter in "D:\manga\chapter_01" to Vietnamese using Patrick Hand font.
Follow the fukidashi-comic-translation workflow:
1. Initialize the chapter & lock character lore/pronouns via fukidashi_put_lore.
2. Clean speech bubbles with local LaMa inpainting.
3. Transcribe with local OCR and localize dialogue.
4. Typeset dynamically with full Vietnamese diacritics.
5. Launch the native egui QA editor for final approval and export to CBZ.
```

---

## ✨ Features

| Capability | Technical Highlight | Value |
| :--- | :--- | :--- |
| 📥 **Automated Ingest** | `fukidashi_pull_chapter` with MangaDex & gallery-dl | Fetch any chapter or webcomic directly from prompt |
| 🎯 **Bubble Detection** | Neural RT-DETR model fine-tuned on comic layouts | Precise bounding box & vertical/horizontal text detection |
| 🧹 **Deep Inpainting** | LaMa ONNX engine running on local GPU/CPU | Erases Japanese text while preserving linework & screentones |
| 🔍 **Multilingual OCR** | Baberu Vision, Manga-OCR & PP-OCRv5/v6 | Zero-cloud transcription for Japanese, Korean, Chinese & English |
| ✍️ **Smart Typesetting** | Dynamic box-fitting, automated font scaling | Flawless Vietnamese diacritic support (*Patrick Hand* & *Comic Neue*) |
| 🖥️ **Native egui Editor** | Pure Rust 60fps desktop GUI (`fukidashi-editor`) | Zero Electron/browser bloat; inpainting brush & live bubble tweaker |
| 🛡️ **Strict Lore Guard** | Canonical character & pronoun synchronization | Prevents pronoun drift (`anh/em`, `cậu/tớ`) across chapters |

---

## ⚡ Battle-Tested Hardware & Verified Models

Fukidashi was engineered and field-tested on real-world consumer hardware:

* **Field Test Rig**: `Intel Core i5-4590 (Haswell) | NVIDIA GTX 1060 6GB (Pascal) | 16GB DDR3 RAM`
* *Verdict*: If it runs smoothly on a 10-year-old Haswell CPU with DDR3 RAM, it will fly on your machine.

### 🧠 Tested LLM Translation Engines

While Fukidashi's vision core (RT-DETR, LaMa inpainting, OCR, typesetting, and desktop editor) is **100% local and requires zero external API keys**, the translation brain connects to your chosen agent harness:

| Model | Type | Field Notes |
| :--- | :--- | :--- |
| **GPT-5.6** | Cloud | 📐 **High-Precision Logic**. Flawless tool-calling and structured translation JSON output. |
| **Grok 4.6** | Cloud | 👑 **Uncensored King for R18 / Doujinshi**. Zero euphemisms, raw dirty talk, natural slang. |
| **Claude Sonnet 4.6** | Cloud | 🎯 **Top-tier Literary Scanlation**. Nuanced character pacing, emotional depth, fluid dialogue. |
| **Claude Haiku 4.5** | Cloud | ⚡ Fast, highly cost-effective daily driver for standard chapters. |
| **Gemini 3.8 Flash** | Cloud | ⚡ Instantaneous responses, great for bulk scanning. |

### 🦙 100% Offline (Local LLMs)

Prefer a completely offline pipeline with zero network requests? Wire **local LLMs** (via Ollama, vLLM, or llama.cpp) directly into your harness:

* **Recommended Minimum**: Models in the **20B+ parameter class** for Japanese manga idioms and vertical reading nuances (`gpt-oss:20B`, `gemma-4:26B`).

---

## 🚀 Quickstart

### 🪟 Windows (Recommended - 1-Click Installer)

1. Download **`Fukidashi-Setup.exe`** from [GitHub Releases](https://github.com/Kyokkei/fukidashi-mcp/releases).
2. Run the installer:
   * **No Administrator rights required** (`PrivilegesRequired=lowest`).
   * Choose any drive (`D:\`, `E:\`) to keep your `C:\` drive clean.
   * Hit **Next ➔ Next ➔ Finish**.
3. The installer provisions all pre-trained ONNX models (~470MB) and automatically wires Fukidashi into every supported AI client.

### 🐧 Linux & macOS (From Source)

```bash
# 1. Clone and build the headless server & native egui editor
git clone https://github.com/Kyokkei/fukidashi-mcp.git
cd fukidashi-mcp
cargo build --release --features editor

# 2. Auto-wire into every supported AI client
./target/release/fukidashi-mcp install
```

---

## 🔌 Supported AI Clients & Harnesses

Fukidashi provides native auto-wiring across all major developer and consumer AI environments:

| AI Client / Harness | Mode | Auto-Configuration Target |
| :--- | :--- | :--- |
| **OpenAI Codex** | CLI & Desktop IDE | ✅ `~/.codex/config.json` |
| **Google Antigravity (AGY)** | AGY 1.0 & AGY 2.0 | ✅ `~/.gemini/antigravity/mcp/` |
| **Claude Code** | CLI Agent | ✅ `~/.claude.json` |
| **Claude Desktop** | Native Desktop App | ✅ `%APPDATA%\Claude\claude_desktop_config.json` |
| **Cursor** | AI Code Editor | ✅ `~/.cursor/mcp.json` |
| **VS Code** | Copilot / MCP Extension | ✅ `Code/User/mcp.json` |
| **Cline** | IDE Extension / CLI | ✅ `%USERPROFILE%\.cline\data\settings\cline_mcp_settings.json` |
| **OpenCode** | Terminal Agent / TUI | ✅ `%USERPROFILE%\.config\opencode\opencode.json` (or existing `.jsonc`) |
| **Grok Build** | API / Custom Harness | ✅ Native MCP v3.2.0 protocol endpoint |

---

## Cline and OpenCode on Windows

The installer registers Fukidashi as a local STDIO server. A standard install
with no `--client` flag configures every supported client and creates missing
user config directories/files. Use `--client` to limit a run, or `--all` as an
explicit equivalent of the default. From a release directory, configure only
Cline and OpenCode with:

```powershell
.\fukidashi-mcp.exe install --client cline,opencode
```

The command merges the `fukidashi` entry into each existing config, creates a
missing parent directory when needed, and keeps a backup under
`%LOCALAPPDATA%\Fukidashi\backups`. A conflicting entry with the same name is
left untouched and reported as an error so it cannot be silently replaced.

Cline's current shared config is
`%USERPROFILE%\.cline\data\settings\cline_mcp_settings.json` for the IDE
extension, CLI, and SDK. It uses `mcpServers.fukidashi.command` as a string with
a separate `args` array. Open **MCP Servers → Configure → Configure MCP
Servers** in the Cline panel to open the active file. Cline's MCP overview page
also mentions `%USERPROFILE%\.cline\mcp.json` for the CLI; the canonical config
page and current resolver use the shared `data\settings` path, which is the
path Fukidashi configures. `CLINE_MCP_SETTINGS_PATH` and `CLINE_DATA_DIR` are
honored when set:

```json
{
  "mcpServers": {
    "fukidashi": {
      "command": "C:\\Users\\YOU\\AppData\\Local\\Fukidashi\\bin\\fukidashi-mcp.exe",
      "args": []
    }
  }
}
```

OpenCode's documented global config is
`%USERPROFILE%\.config\opencode\opencode.json`; it also accepts
`opencode.jsonc`. Fukidashi writes OpenCode's V1-compatible direct
`mcp.fukidashi` entry. OpenCode's migration guide documents this form as
supported by both V1 and V2, and the numeric per-server timeout is honored by
the installed 1.x desktop runtime:

```jsonc
{
  "mcp": {
    "fukidashi": {
      "type": "local",
      "command": ["C:\\Users\\YOU\\AppData\\Local\\Fukidashi\\bin\\fukidashi-mcp.exe"],
      "timeout": 600000
    }
  }
}
```

The native V2 form is `mcp.servers.<name>` with a nested timeout object, but
OpenCode 1.18.x logs that nested timeout as unsupported. The installer removes
only an older Fukidashi entry under `mcp.servers` and preserves other servers.

The patcher reads JSON and JSONC (comments, trailing commas, and UTF-8 BOM),
then writes valid JSON after a successful merge. Existing content is backed up
before replacement. Fukidashi already speaks MCP over STDIO, so no HTTP port or
extra environment variables are required. The execution timeout gives CPU-only
OCR, cleaning, and typesetting enough time to return the structured page
response; if an older client still times out, wait for the server job to finish
and resume with its returned `job_id` rather than resubmitting the same token.
Verify either client with
`fukidashi-mcp status`, then restart the client and check that the Fukidashi
tools appear. See the [Cline MCP guide](https://github.com/cline/cline/blob/main/docs/mcp/mcp-overview.mdx),
[OpenCode MCP guide](https://opencode.ai/v2/docs/mcp-servers), and
[OpenCode config locations](https://dev.opencode.ai/docs/config/) for the
client-side schema and precedence rules. Cline's [configuration locations
guide](https://github.com/cline/cline/blob/main/docs/getting-started/config.mdx)
documents the shared `data\settings` file used by current IDE and CLI builds.

Strict translation errors are returned as JSON with the human-readable
`error`, plus `stage`, `code`, and a `diagnostic` object when cleaning rejects
an unsafe handoff. The diagnostic identifies the detector line, confidence,
bounding box, overlapping translation item, and `next_step`; this lets
OpenCode display the actionable cause instead of reducing it to `inference
failed`.

When resuming a checkpoint created by an older Fukidashi version, the server
reconciles detector lines before issuing a work token. Any line absent from the
translation handoff is added as `keep_source=true` with an
`auto_preserved_unrepresented_text` warning and an audit entry, so an omitted
client field cannot trigger a destructive-clean retry loop.

---

## 🖥️ Native Desktop QA Editor

When chapter processing completes, Fukidashi triggers the native **Fukidashi Editor** (`fukidashi-editor.exe`):

* **Zero Web Overhead**: Built with pure Rust (`egui`/`eframe`) — no Chromium, no Electron, zero browser tabs.
* **Side-by-Side Comparison**: Instant toggle between **Source**, **Cleaned**, and **Rendered** views.
* **Live Bubble Tweaker**: Double-click any speech bubble to edit text, resize fonts, or re-center bounding boxes.
* **Canvas Editing Tools**: Draw or resize bubbles, add text, and use the cover, restore, and eyedropper tools with zoomable, pannable canvas navigation.
* **Fast Chapter Review**: Virtualized page thumbnails, direct page-number navigation, dirty-page markers, and page-level cached rendering keep large chapters responsive.
* **Undo, Redo & Autosave**: Review edits persist automatically; use `Ctrl+Z` / `Ctrl+Y` to step through changes and `Ctrl+S` to save and render.
* **Translator Feedback**: Flag missing dialogue or request fixes from the editor, then return that review feedback to the translation agent.
* **Approve & Export**: Approve the reviewed chapter; the MCP export supports ZIP, EPUB, or standalone HTML (`html_monolith`).

---

<details>
<summary><b>🛠️ Developer Diagnostics & Memory Tuning (Click to expand)</b></summary>

### CLI Diagnostics
```bash
# Inspect system status, paths, and configured adapters
./target/release/fukidashi-mcp doctor

# Change storage location (models, jobs, cache)
./target/release/fukidashi-mcp config-set --storage-root "D:\Fukidashi"
```

### Compiling the Windows Installer
```powershell
.\build_installer.ps1
```
Compiles `installer.iss` with Inno Setup and LZMA2 ultra compression into `dist\Fukidashi-Setup.exe`.

### Low-VRAM & GPU Memory Tuning
Tuned for consumer hardware (e.g. GTX 1060 6GB, 16GB RAM):
```bash
# Limit CUDA session arena memory (default: 2048 MiB)
set FUKIDASHI_GPU_MEMORY_LIMIT_MIB=2048

# Force CPU inference fallback if running on low-spec systems
set FUKIDASHI_PROVIDER=cpu
```

### Model Directory Structure
Pre-trained ONNX models are located under `<storage_root>/models/`:
```text
models/
├── detection/
│   ├── detector-v4-s_int8.onnx
│   └── script_id/ (osd_lstm.onnx, osd_labels.json)
├── inpainting/
│   └── lama-manga-dynamic.onnx
└── ocr/
    ├── baberu-ocr/ (vision_int4, decoder_prefill, decoder_step, vocab)
    ├── manga-ocr-mobile-onnx/
    └── ppocr-v5-onnx / ppocr-v6-onnx
```

</details>

---

## 📜 Attribution & Licensing

* **License**: Licensed under the [Apache-2.0 License](LICENSE).
* **Lineage**: Preprocessing logic and neural model topologies adapted from [Comic Translate](https://github.com/ogkalu2/comic-translate) (Apache-2.0).
* **Bundled Typography**:
  * *Patrick Hand* — SIL Open Font License 1.1 (Full Vietnamese diacritics coverage).
  * *Comic Neue* — SIL Open Font License 1.1.
  * *Noto Sans Symbols 2* — SIL Open Font License 1.1 (Unicode symbol/emoji fallback).
  * Chinese, Korean, and Japanese glyphs use configured or installed system CJK fonts when available.
