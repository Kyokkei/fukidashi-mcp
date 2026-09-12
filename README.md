<div align="center">

# 吹 Fukidashi MCP

### Autonomous, 100% Local Manga Scanlation Studio for AI Agents

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Rust: 2024 Edition](https://img.shields.io/badge/Rust-2024_Edition-orange.svg)](https://www.rust-lang.org/)
[![Platform: Windows x64](https://img.shields.io/badge/Platform-Windows_x64-0078D6.svg)](#-quickstart)
[![Linux & macOS](https://img.shields.io/badge/Platform-Linux%20%7C%20macOS-FCC624.svg)](#-linux--macos-from-source)
[![Protocol: MCP v3.2](https://img.shields.io/badge/MCP-v3.2.0-purple.svg)](https://modelcontextprotocol.io/)
[![100% Local Vision Core](https://img.shields.io/badge/Vision_Core-100%25_Offline-success.svg)](#-battle-tested-hardware--verified-models)

**Fukidashi** turns Claude Desktop, Claude Code, Codex, Grok, and Antigravity into an autonomous, professional manga localization studio running on your local machine.

*Zero cloud subscriptions for vision. Zero cloud fees for cleaning. 100% private OCR, inpainting, and typesetting on your own GPU/CPU.*

---

</div>

## 📖 The Problem vs. The Fukidashi Solution

* **The Old Way**: You screenshot manga pages, upload them to ChatGPT, copy the translated text into Photoshop, spend 4 hours manually healing backgrounds with the clone stamp, and manually format text boxes.
* **The Fukidashi Way**: Tell your AI agent in one single prompt:
  > *"Hey, pull the latest chapter of Dandadan and translate it to Vietnamese using Patrick Hand."*
  
  Fukidashi auto-fetches the chapter from MangaDex (or any supported URL), detects speech bubbles, deeply inpaints backgrounds with LaMa, transcribes Japanese via local OCR, translates through your agent, typesets dialogue with dynamic box-fitting, and opens the native desktop QA editor for review and CBZ export.

```text
┌─────────────────┐       ┌─────────────────┐       ┌─────────────────┐
│  MangaDex Pull  │ ────► │  RT-DETR Model  │ ────► │     LaMa AI     │
│  or Local Raw   │       │ (Bubble Detect) │       │ (Deep Inpaint)  │
└─────────────────┘       └─────────────────┘       └────────┬────────┘
                                                             │
┌─────────────────┐       ┌─────────────────┐       ┌────────▼────────┐
│  Exported CBZ / │ ◄──── │   Native egui   │ ◄──── │  Local Baberu   │
│   Clean ZIP     │       │   QA Editor     │       │   Manga OCR     │
└─────────────────┘       └────────▲────────┘       └────────┬────────┘
                                   │                         │
                          ┌────────┴────────┐       ┌────────▼────────┐
                          │  Smart Comic    │ ◄──── │  Your AI Agent  │
                          │   Typesetting   │       │ (Claude/Grok)   │
                          └─────────────────┘       └─────────────────┘
```

---

## ✨ Features

* 📥 **MangaDex & Direct URL Ingest (`fukidashi_pull_chapter`)**: Native search and chapter acquisition. Prompt your agent to grab the latest release automatically or pass any webcomic URL via gallery-dl integration.
* 🎯 **RT-DETR Detection**: Neural bubble and vertical/horizontal text-line detection designed specifically for comic and manga layouts.
* 🧹 **LaMa Deep Inpainting**: Seamlessly wipes Japanese dialogue and sound effects while preserving delicate linework, screentones, and character hair.
* 🔍 **Multilingual Local OCR**: High-speed, local transcription for Japanese (Baberu Vision & Manga-OCR), Chinese, Korean, and Latin/English (PP-OCRv5/v6).
* ✍️ **Smart Typesetting**: Dynamic word-wrapping, box-fitting, automated font scaling, and full Vietnamese diacritic support via bundled fonts (*Patrick Hand* & *Comic Neue*), with Unicode emoji/symbol fallbacks (`❤`, `★`).
* 🖥️ **Hardware-Accelerated Desktop Editor (`fukidashi-editor`)**: Standalone, 60fps native Rust `egui` QA application. Panning, zooming, side-by-side comparison, inpainting brush, and real-time bubble editing.
* 🛡️ **Strict Lore Preservation**: Integrated `fukidashi_put_lore` system prevents pronoun drift and character name confusion across multi-page chapters. Use `{"schema":1,"characters":["Fuyu"],"pronouns":[],"glossary":[{"source":"proprietress","target":"bà chủ"}]}`; character strings are stored canonically with stable IDs.

---

## ⚡ Battle-Tested Hardware & Verified Models

Fukidashi was explicitly engineered and field-tested on budget, real-world consumer hardware:

* **Field Test Rig**: `Intel Core i5-4590 (Haswell) | NVIDIA GTX 1060 6GB (Pascal) | 16GB DDR3 RAM`
* *Verdict*: If it runs smoothly on a 10-year-old Haswell CPU with DDR3 RAM, it will fly on your machine.

### 🧠 Tested LLM Translation Engines

While Fukidashi's vision core (RT-DETR, LaMa inpainting, OCR, typesetting, and desktop editor) is **100% local and requires zero API keys**, the translation brain connects to your chosen agent harness:

| Model | Type | Field Notes |
| :--- | :--- | :--- |
| **Grok 4.6** | Cloud | 👑 **The Uncensored King for R18 / Doujinshi**. Zero euphemisms, raw dirty talk, natural slang. |
| **Claude Sonnet 4.6** | Cloud | 🎯 **Top-tier Literary Scanlation**. Nuanced character pacing, emotional depth, fluid dialogue. |
| **Claude Haiku 4.5** | Cloud | ⚡ Fast, highly cost-effective daily driver for standard chapters. |
| **HY3 & GLM 5.3 Flash** | Cloud | 🪙 Ultra-budget, high-speed translation. |
| **Gemini 3.8 Flash** | Cloud | ⚡ Instantaneous responses, great for bulk scanning. |
| **GPT 5.6 Luna** | Cloud | 📐 Precise syntactic localization and grammar consistency. |

### 🦙 Going 100% Offline (Local LLMs)

Prefer a completely offline setup with zero external cloud calls? You can wire **local LLMs** (via Ollama, vLLM, or llama.cpp) directly into Claude Code or Codex:

* **Recommended Minimum**: We strongly recommend models in the **20B+ parameter class** for Japanese manga grammar, idioms, and reading order nuances:
  * `gpt-oss:20B`
  * `gemma-4:26B`

### 🏆 Recommended Harnesses
1. **Claude Code** (Best agent workflow and autonomous tool chaining)
2. **Grok build** (Best for unrestricted scanlation & adult titles)

---

## 🚀 Quickstart

### 🪟 Windows (Recommended - 1-Click Installer)

1. Grab the latest **`Fukidashi-Setup.exe`** from [GitHub Releases](https://github.com/Kyokkei/fukidashi-mcp/releases).
2. Run the installer:
   * **No Admin rights needed** (`PrivilegesRequired=lowest`).
   * Choose any drive (`D:\`, `E:\`) to keep your `C:\` drive clean.
   * Hit **Next ➔ Next ➔ Finish**.
3. The installer automatically provisions all pre-trained ONNX models (~470MB) and wires Fukidashi into your detected AI clients.

### 🐧 Linux & macOS (From Source)

```bash
# 1. Clone and build the headless server & native egui editor
git clone https://github.com/Kyokkei/fukidashi-mcp.git
cd fukidashi-mcp
cargo build --release --features editor

# 2. Auto-wire into all detected AI clients
./target/release/fukidashi-mcp install --all
```

---

## 💬 Prompting Your AI Agent

Open **Claude Code**, **Claude Desktop**, **OpenAI Codex**, **Grok**, or **Antigravity**, and prompt:

```text
Translate the manga chapter in "D:\manga\chapter_01" to Vietnamese using Patrick Hand font.
Follow the fukidashi-comic-translation workflow.
```

The AI will:
1. Initialize the chapter and lock character lore/pronouns.
2. Run local OCR and wipe the speech bubbles.
3. Translate and typeset each page cleanly.
4. Launch the native **Fukidashi Editor** for you to inspect and approve the final result.

---

## 🔌 Supported AI Clients & Harnesses

Fukidashi provides native auto-wiring across all major developer and consumer AI environments:

| AI Client / Harness | Mode | Auto-Configuration |
| :--- | :--- | :--- |
| **Claude Code** | CLI Agent | ✅ `~/.claude.json` |
| **Claude Desktop** | Native Desktop App | ✅ `%APPDATA%\Claude\claude_desktop_config.json` |
| **OpenAI Codex** | CLI & Desktop IDE | ✅ `~/.codex/config.json` |
| **Google Antigravity (AGY)** | AGY 1.0 & AGY 2.0 | ✅ `~/.gemini/antigravity/mcp/` |
| **Grok Build** | API / Custom Harness | ✅ Native MCP v3.2.0 protocol endpoint |
| **Cursor** | AI Code Editor | ✅ `~/.cursor/mcp.json` |
| **VS Code** | Copilot / MCP Extension | ✅ `Code/User/mcp.json` |

---

## 🖥️ Native Desktop QA Editor

When a chapter translation finishes, Fukidashi triggers the native **Fukidashi Editor** (`fukidashi-editor.exe`):

* **Zero Web Overhead**: Built with pure Rust (`egui`/`eframe`) — no Chromium, no Electron, zero browser tabs.
* **Side-by-Side Comparison**: Instant toggle between **Source**, **Cleaned**, and **Rendered** views.
* **Live Bubble Tweaker**: Double-click any speech bubble to correct text, change font sizes, or re-center text boxes.
* **Canvas Correction Brush**: Touch up residual artifacts or redraw text masks directly on the canvas.
* **One-Click Export**: Approve and export your chapter directly to a standardized CBZ archive or high-res image folder.

---

<details>
<summary><b>🛠️ Developer Guide & Advanced Architecture (Click to expand)</b></summary>

### CLI Diagnostics
```bash
# Inspect system status, paths, and configured adapters
./target/release/fukidashi-mcp doctor

# Change storage location (models, jobs, cache)
./target/release/fukidashi-mcp config-set --storage-root "D:\Fukidashi"
```

### Compiling the Windows Installer
Run our automated packaging script:
```powershell
.\build_installer.ps1
```
This compiles `installer.iss` with Inno Setup and LZMA2 ultra compression into `dist\Fukidashi-Setup.exe`.

### Low-VRAM & GPU Memory Tuning
Fukidashi is tuned for consumer hardware (e.g. GTX 1060 6GB, 16GB RAM):
```bash
# Limit CUDA session arena memory (default: 2048 MiB)
set FUKIDASHI_GPU_MEMORY_LIMIT_MIB=2048

# Force CPU inference fallback if running on low-spec systems
set FUKIDASHI_PROVIDER=cpu
```

### Model Directory Structure
Fukidashi expects pre-trained ONNX models under `<storage_root>/models/`:
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

* **License**: This project is licensed under the [Apache-2.0 License](LICENSE).
* **Lineage**: Adapted preprocessing logic and neural model topologies are based on [Comic Translate](https://github.com/ogkalu2/comic-translate) (Apache-2.0).
* **Bundled Typography**:
  * *Patrick Hand* — SIL Open Font License 1.1 (Full Vietnamese diacritics coverage).
  * *Comic Neue* — SIL Open Font License 1.1.
  * *Noto Sans Symbols 2* — SIL Open Font License 1.1 (Unicode symbol/emoji fallback).
