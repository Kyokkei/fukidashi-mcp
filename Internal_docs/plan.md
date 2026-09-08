# Fukidashi MCP: master engineering architecture and roadmap

**Current user correction:** automatic multilingual OCR, selectable translation target (this user's default: `vi`), and exactly one Luna High coder. [Multilingual amendment](multilingual.md) overrides the Japanese-only product scope and earlier two-coder allocation below. The original tensor contracts remain relevant to their particular models.

Status: implementation blueprint, 2026-09-05. Owner: Astra (architecture/audit); implementation: GPT-5.6 Luna, High. Repository: `D:\coding\fukidashi-mcp`. Upstream, read-only: `D:\coding\comic-translate`, inspected HEAD `8f13ae5c4bab567c12b9383f085ba32d98b43348` (local modifications must be tracked separately from this revision).

## 1. Product and delivery contract

Build a Rust 2024 headless executable exposing five MCP tools over stdio. Detection, Japanese OCR, cleaning, typesetting, editing, and export run locally without accounts, telemetry, or automatic network calls. Translation belongs to the calling model/user; the server preserves supplied text and does not rewrite its register. Stable page/bubble IDs, source text, translations, glossary and ordered pages carry narrative context across a project. An external OCR option means returning crops for a caller to inspect and accepting supplied text; no automatic cloud dispatch.

One application executable embeds editor assets; ONNX weights, fonts and native ONNX Runtime libraries remain separately provisioned global assets. Do not claim a DLL-dependent GPU build is a literally self-contained executable. CPU execution also needs a compatible runtime. Cargo dependency acquisition at development time is separate from offline application operation.

Initial inspection found an empty `Internal_docs` directory and **no Git metadata**, contrary to the supplied brief. Rust 1.97.1 and Cargo 1.97.1 are installed. The implementation engineer may initialize Git, but must not publish, commit credentials, or change upstream files.

Local validation assets were subsequently found (presence verified; graph behavior and hashes still require testing):

- Model root `C:\Users\Yozora\AppData\Local\ComicTranslate\models` contains `detection/detector-v4-s_int8.onnx`, `inpainting/lama-manga-dynamic.onnx`, `ocr/ppocr-v5-onnx/ch_PP-OCRv5_mobile_det.onnx`, and `ocr/manga-ocr-mobile-onnx/{encoder.onnx,decoder_init.onnx,decoder_step.onnx,vocab.txt}`.
- Native `onnxruntime.dll` and CUDA/shared provider libraries exist in `D:\coding\comic-translate\.venv\Lib\site-packages\onnxruntime\capi`; installed Python package declares version 1.29.0. Presence does not prove Rust ABI/provider compatibility. Reuse via explicit absolute configuration, read-only; do not hardcode these personal paths as production defaults.
- Upstream has local changes in `imkit/morphology.py` and `imkit/transforms.py`, plus untracked `launch.bat` and `src/`. Therefore upstream HEAD is provenance context, not a byte-for-byte identity for every local preprocessing operation.

## 2. Source audit and decisions

Read these upstream files before porting the corresponding behavior:

| File relative to upstream | Verified behavior / decision |
| --- | --- |
| `modules/detection/rtdetr_v2_onnx.py` | RGB /255, direct 640 square resize, width-before-height int64 sizes; labels 0 versus 1/2; threshold 0.3. Upstream also slices tall pages; preserve this as an explicit later parity gate. |
| `modules/ocr/ppocr/preprocessing.py` | BGR, normalization to [-1,1]. **Nearest rounding**, Python ties-to-even, stride 32 and minimum 32; default min-side 960 upscale. Brief's floor expression is not upstream behavior. |
| `modules/ocr/ppocr/postprocessing.py` | Default unclip is 1.6 and initial dilation 2x2. Product explicitly chooses unclip **2.0** and final 3x3/5x5 halo dilation; this is a deliberate override, not an upstream default. |
| `modules/inpainting/lama.py`, `modules/inpainting/base.py`, `modules/utils/inpainting.py` | RGB float input/output, binary mask, modulo-8 padding and output crop; preserve unmasked source pixels by compositing. Inspect helper for exact padding boundary mode. |
| `modules/ocr/manga_ocr/mobile/onnx_engine.py` | White 224-square letterbox, RGB /255, no ImageNet normalization. Four layers, four heads, 64-dimensional heads, fixed 256 cache slots. CPU-only one-thread upstream default; keep OCR CPU preferred until measurements justify GPU. |
| `LICENSE`, `modules/utils/download.py` | Apache-2.0 upstream code; model catalog is evidence for paths, not permission to redistribute every weight. Record each model/font license independently. |

Retain upstream license text and create attribution documenting adapted files, source revision (when available), and changes. Do not invent model hashes or license grants. Record unresolved asset provenance. Local code is authoritative over assumptions; report any new mismatch before changing tensor contracts.

## 3. Modules, ownership and data flow

Use one crate with a library plus a thin executable:

```text
src/main.rs                 CLI, stderr tracing, shutdown
src/lib.rs                  module exports
src/config.rs               absolute global asset paths and limits
src/error.rs                domain errors and MCP mapping
src/domain.rs               shared serde/schemars types and validation
src/mcp.rs                  five tools, bounded blocking execution
src/models/{mod,runtime,manifest}.rs
src/vision/{mod,preprocess,detect,segment,inpaint,ocr}.rs
src/typeset/{mod,layout}.rs  shaped glyph layout and raster composition
src/editor.rs               loopback editor lifecycle and save API
src/export.rs               ZIP, EPUB, standalone HTML
assets/editor.html          embedded HTML/CSS/JS, no CDN
tests/                     deterministic and optional real-asset tests
```

Coordinator owns `Internal_docs`. Luna core owns Cargo, shared modules, MCP, model and vision modules, README/license setup and integration tests. Luna presentation owns only `src/typeset/**`, `src/editor.rs`, `src/export.rs`, `assets/**` and its uniquely named tests. Communicate API changes; do not edit another lane's files or run global formatting while the other lane writes.

Flow: MCP request -> validate paths/limits -> decode RGB -> acquire bounded inference worker -> lazy-load session -> preprocess/run/postprocess -> domain JSON or atomic output -> structured tool response. Typesetting, editor and export must work when ONNX assets are absent. Every output includes absolute artifact paths; tool text may mirror structured JSON for clients with basic support. Stdout contains MCP frames only.

### Shared public contract

Domain `Rect { x1:f32, y1:f32, x2:f32, y2:f32 }`: top-left origin, source-image pixels, half-open edges. Reject nonfinite values and nonpositive extents; clip detector boxes to source bounds before cropping. `Bubble { id:String, bbox:Rect, text:String, translation:Option<String>, confidence:f32, reading_order:usize }`. Retain text-line detections separately where needed rather than promoting them silently into bubbles.

`TypesetPayload { bbox:Rect, text:String, font_path:Option<String>, min_font_size:Option<f32>, max_font_size:Option<f32>, shape:Option<String> }`; default ellipse, explicit rectangle supported. Additional fields are allowed after coordination. Missing fonts and missing glyphs must be actionable errors or explicit warnings, never invisible success.

Presentation API: `typeset::typeset_page(image_path:&Path, bubbles:&[TypesetPayload], output_path:&Path) -> anyhow::Result<serde_json::Value>`; `editor::serve_editor(image_path:&Path, json_data:serde_json::Value) -> anyhow::Result<serde_json::Value>` may be async if agreed; `export::export_project(project_dir:&Path, format:&str) -> anyhow::Result<serde_json::Value>`. Core adapts MCP to these functions. Editor shutdown handle ownership must be explicit and tied to process lifetime.

Tool schemas retain requested minimum parameters:

1. `fukidashi_analyze_page(image_path)` returns dimensions and ordered bubbles; optional `ocr_mode=local|external|disabled`. External mode returns crop paths and `needs_ocr`, not invented OCR.
2. `fukidashi_clean_page(image_path)` returns cleaned PNG and mask paths; optional explicit mask/regions and dilation size. No mask means no pixels to erase, not whole-box erasure.
3. `fukidashi_typeset(image_path,bubbles)` returns rendered path, fitted sizes and overflow/missing-glyph diagnostics.
4. `fukidashi_serve_editor(image_path,json_data)` returns loopback URL and persistence location immediately; does not block the MCP transport until the browser closes.
5. `fukidashi_export(project_dir,format)` accepts exactly `zip`, `epub`, `html_monolith`; exports manifest-ordered pages and translations.

Persist `project.json` with `schema_version:1`, ordered `pages` (each has `id`, relative `image_path`, and `bubbles`), optional `title`, `language`, `glossary`, and project metadata. No directory-name sort may override explicit page order. Writers use same-directory temporary files and atomic promotion, avoiding replacement of source images. Generated outputs default to a `fukidashi-output` directory beside the input, with unique names.

## 4. Cargo specification

This is the complete starting manifest specification; Luna must resolve it, check crate APIs and commit a lockfile locally as a file (no Git commit required). Versions are intentional baseline constraints, not claims that every newest version is required. If a version is unavailable or incompatible, select a verified compatible release and report the adjustment. Unused dependencies may be removed after implementation. Native dependency `clipper2` can require a C++ toolchain; verify it before choosing a polygon-offset alternative.

```toml
[package]
name = "fukidashi-mcp"
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"
description = "Local comic-page analysis, cleaning, typesetting and editing over MCP"

[features]
default = ["onnx"]
onnx = ["dep:ort"]
cuda = ["onnx", "ort/cuda"]
directml = ["onnx", "ort/directml"]

[dependencies]
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "1"
rmcp = { version = "=3.2.0", default-features = false, features = ["server", "macros", "schemars", "transport-io"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-std", "io-util", "sync", "signal", "net", "time", "fs"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
clap = { version = "4", features = ["derive"] }
dirs = "6"
image = { version = "0.25", default-features = false, features = ["png", "jpeg", "webp"] }
imageproc = "0.25"
ndarray = "0.17"
ort = { version = "=2.0.0-rc.13", optional = true, default-features = false, features = ["std", "ndarray", "load-dynamic", "api-22"] }
clipper2 = "0.5"
rustybuzz = "0.20"
fontdue = "0.9"
unicode-segmentation = "1"
unicode-linebreak = "0.1"
axum = { version = "0.8", default-features = false, features = ["http1", "json", "tokio"] }
uuid = { version = "1", features = ["v4", "serde"] }
base64 = "0.22"
zip = { version = "4", default-features = false, features = ["deflate"] }
tempfile = "3"
sha2 = "0.10"

[dev-dependencies]
approx = "0.5"

[profile.release]
lto = "thin"
codegen-units = 1
strip = "debuginfo"
```

Default ORT features are off to avoid automatic native-runtime downloads. Pin an API level compatible with the tested native runtime, validate at startup, and document it. CUDA and DirectML compilation enables registration code; it does not install a working provider or guarantee GPU coverage. Non-Windows code must cfg-gate DirectML. `--no-default-features` builds protocol/presentation and pure preprocessing without ONNX, returning explicit unavailable errors for inference.

The MCP SDK provides the server and stdio transport; verify the pinned API against its documentation. [rmcp documentation](https://docs.rs/rmcp/latest/rmcp/). Dynamic loading, CUDA and DirectML are explicit ORT features. [ort feature reference](https://docs.rs/crate/ort/2.0.0-rc.13/features).

## 5. Runtime, portability and execution providers

Resolve model root once: explicit CLI `--models-dir` > `FUKIDASHI_MODELS_DIR` > user home `~/.fukidashi/models`. Relative explicit roots become absolute at process startup; defaults never depend on cwd. Store filenames in a manifest, with OCR in a dedicated subdirectory. Runtime root or explicit `ORT_DYLIB_PATH` similarly resolves to an absolute trusted library path. Never search an arbitrary client cwd for native libraries. Missing files identify expected paths and offline installation instructions; do not download on first inference.

Initialize ORT once, before sessions, disable telemetry, keep environment/library alive for process lifetime. Attempt provider configuration/session construction in order CUDA -> Windows DirectML -> CPU. Each attempt uses a fresh builder. Report actual chosen provider and prior failures on stderr; unsupported nodes may still execute on CPU. A runtime binary may expose only a subset of providers: falling back to CPU inside it is supported, but switching globally loaded runtime DLLs in-process is not. Do not promise CUDA-to-DirectML fallback if the installed runtime lacks DirectML.

DirectML uses sequential graph execution and disables memory-pattern optimization; serialize calls on a session. [DirectML constraints](https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html). CPU uses sequential graph execution with bounded intra-op threads (detector baseline 4, inter-op 1); OCR baseline 1/1. Sequential graph execution is compatible with a CPU thread pool for individual operators. One active heavy inference job by default prevents GPU oversubscription on 6 GB cards. Do not multiply Tokio, Rayon and ORT worker pools.

Session-construction failure can retry another provider. Runtime failure can retry once on CPU for recognized provider/device/OOM failures; malformed tensors are errors, not reasons to loop. Allocate input tensors before `run`, borrow them only for that call, extract/copy outputs needed after `SessionOutputs` drops. No fabricated `'static`, unsafe transmute, raw-pointer ownership tricks, or concurrent mutable session use. Keep OCR caches owned and reset per crop. Release GPU sessions before memory-intensive competing stages when necessary.

## 6. Tensor mathematics and postprocessing

### Detector

Resize RGB to 640x640 (match PIL resampling explicitly; record any backend pixel differences). Flat offset for `[1,C,H,W]` is `c*H*W+y*W+x`; write `pixel[c]/255`. Supply int64 `[[original_width,original_height]]`. Validate names, dtypes, ranks and matching N for labels `[1,N]`, boxes `[1,N,4]`, scores `[1,N]`; reject nonfinite boxes/scores, retain scores >=0.3. Boxes are already source pixels: no second scaling or sigmoid. Clip and discard degenerate boxes. Preserve full floating-point boxes in JSON and floor/ceil at crop boundaries.

Associate text boxes with bubble by contained center plus maximum intersection fraction; preserve unmatched text. Reading order: construct deterministic row bands from vertical overlap with a fixed anchor (e.g. >=0.5 of shorter height), sort bands top-to-bottom, then items right-to-left, breaking ties by y/id. Do not use a nontransitive pairwise overlap comparator. Complex panel layouts are heuristic and manually reorderable. Tall-page slicing requires offset restoration and overlap deduplication before ordering.

### DBNet

For default min-limit mode, `r=max(1,960/min(H,W))`; for bounded max-limit mode, `r=min(1,L/max(H,W))`. Dimensions `H'=max(32,32*round_ties_even(H*r/32))`, likewise W. Record independently `sx=W/W'`, `sy=H/H'`. BGR channel order and `(v/255-0.5)/0.5` are mandatory. Enforce pixel budgets after snapping; permit an explicit bounded resize mode rather than silently allocating an extreme panorama.

Threshold probability map at `p>0.3`; score candidate polygons by mean original probability over their interior and retain >=0.5. Unclip offset distance is `d=area(P)*2.0/(perimeter(P)+epsilon)`, using a real round-join polygon offset (Clipper/Vatti family); an expanded axis-aligned rectangle is not equivalent. Handle empty, degenerate and multiple offset polygons explicitly. Scale map coordinates back by actual output-map dimensions, not assumed input dimensions. Distinguish text-region polygons from text-stroke pixels: use accepted regions to gate probability-derived stroke masks, then nearest-neighbor map to source pixels and dilate by 3x3 (default) or 5x5. Return the mask so over-erasure can be reviewed. Do not erase every pixel inside each rectangular text region.

### LaMa

`pad_h=(8-H%8)%8`, `pad_w=(8-W%8)%8`; pad only bottom/right using NumPy `symmetric` behavior (edge pixels repeated, distinct from `reflect`), verified in `modules/utils/inpainting.py:249`. For a positive dimension n, map a padded coordinate i with `j=i mod (2*n)`, then `j` if j<n else `2*n-1-j`; this also handles n=1 and padding wider than the source. Apply to both image and mask. Image `[1,3,Hpad,Wpad]` RGB /255, mask `[1,1,Hpad,Wpad]` `(mask>0)?1:0`. Validate output shape and finite values, unpad to H,W, clamp to [0,1], multiply 255 and truncate to u8. Composite only masked pixels into the original RGB image. Empty mask returns an unchanged copy without invoking LaMa. Oversized pages require bounded region crops with context or a clear resource-limit error, never unconditional full-page allocation.

### Manga OCR Mobile

Scale `s=min(224/w,224/h)`; `dw=max(1,floor(w*s))`, `dh=max(1,floor(h*s))`, bilinear resize and center on white with integer floor offsets. NCHW float RGB /255. Discover and validate actual model metadata; ordered input binding is permitted only for the exact audited export with verified arity/shapes.

Encoder output goes to init with start token int64 `[1,1]` value 2. Init outputs logits, self K, self V, cross K, cross V. Own zeroed self caches `[4,1,4,256,64]`; copy init K/V into slot zero. Restrict argmax to vocabulary length, terminate at EOS 3. Step inputs are encoder hidden, current token, position `[1,1]`, full self K/V, cross K/V. Set position `min(cache_len+1,127)` before the step; append returned K/V into exactly `cache_len`, increment, then select next token. Stop at EOS or 256 slots/token bound and report truncation. Each self cache is 1,048,576 bytes of f32 storage; two are 2 MiB, excluding cross caches and model activations.

Ignore token IDs <5 and concatenate vocabulary entries as upstream does; do not silently replace this with generic WordPiece `##` rules. Keep raw token text alongside normalized OCR. Port normalization only from a correctly decoded UTF-8 source; existing terminal output showed mojibake in some substitutions, so audit literals before copying. No translation normalization of user-supplied text.

## 7. Exact text fitting and ellipse wrapping

Fit actual shaped glyphs, not character counts. Use rustybuzz shaping, font units converted by `scale=f/units_per_em`, advances plus glyph offsets, and fontdue rasterization using matching glyph IDs/font face. Preserve grapheme clusters, explicit newlines, Unicode break opportunities and CJK punctuation restrictions. Re-shape each selected line: shaping a full paragraph once may give wrong boundary kerning/ligatures. Missing glyphs need a known font fallback or error.

Given bbox `(x1,y1,x2,y2)` and inset p, center `cx=(x1+x2)/2`, `cy=(y1+y2)/2`, radii `a=(x2-x1)/2-p`, `b=(y2-y1)/2-p`; reject `a<=0` or `b<=0`. Interior satisfies `((x-cx)/a)^2+((y-cy)/b)^2<=1`. A horizontal centerline alone is insufficient for glyph containment.

For a line's ink band `[yt,yb]`, include outline/antialias margin q and let `d=max(abs(yt-q-cy),abs(yb+q-cy))`. Reject d>b. The safe half-width is `h=a*sqrt(max(0,1-(d/b)^2))`; available interval is `[cx-h+q,cx+h-q]`. Require the shaped ink bounds, including negative left bearings, to fit this interval; center the ink bounds, not merely the advance width. This conservative band check contains the full glyph rectangles. Rectangle mode substitutes constant inner edges.

For candidate font size f, obtain ascent A, positive descent D and nonnegative leading G in pixels. For n lines, block height is `n*(A+D)+(n-1)*G`; center the block vertically and place baseline i at `cy-block_height/2+A+i*(A+D+G)`. Determine each band's available width and use dynamic programming over legal break positions to place all text in n lines. State `(line_index,text_break_index)` advances only if the shaped substring fits that band's interval. Permit empty lines only for explicit newlines. Enumerate feasible n, reject total height >2b, choose a complete solution minimizing raggedness and excessive splits. For a simple initial greedy wrapper, document that it may miss a feasible solution and retain the DP as the acceptance target.

Find the largest fitting size on a finite grid (default 0.5 px) from max down to min. Do not assume font hinting and Unicode wrapping make the predicate globally monotonic; binary search alone can skip valid candidates. Cache shaping measurements per text/font/size. No fit at minimum returns `TextOverflow` with the affected bubble; do not clip or truncate dialogue. Verify transformed glyph ink bounds before raster composition. Initial scope is horizontal text; vertical Japanese writing is an explicit later feature, not silently approximated.

## 8. Editor and exports

Embed assets with `include_str!`/`include_bytes!`; bind `127.0.0.1:0`. Return an unguessable per-session capability URL, validate Host and same-origin writes, restrict endpoints to that session's image/JSON, cap request sizes, and reject arbitrary filesystem paths. No CDN, account, telemetry or permissive CORS. Browser has canvas zoom/pan, draggable/resizable boxes, text editing, ordering and save; persist atomically and validate coordinates server-side. Use natural image dimensions when converting pointer coordinates through zoom/pan. Saving JSON must survive editor restart. Render text through DOM textContent/canvas, never interpolate raw translation into executable HTML.

ZIP uses safe relative entry names, manifest and explicitly referenced page assets. Reject references outside project root, including symlinks/junctions, and do not recursively sweep unrelated files. EPUB has uncompressed `mimetype` as first entry containing `application/epub+zip`, `META-INF/container.xml`, package OPF with manifest/spine, navigation XHTML and ordered page XHTML/images; escape XML and assign correct media types. HTML monolith embeds image data URIs, escaped text, inline style/scripts and no external resources. Limits bound decoded pixels, export input bytes and base64 expansion. Atomic output avoids partially valid archives. Round-trip textual content tests include punctuation, newlines and non-ASCII dialogue.

## 9. Five-phase roadmap and acceptance gates

### Phase 1 — foundation and MCP

Create Cargo/library/executable, config, errors, domain, schemas and five tool routes. Implement CLI help/version/doctor and lazy runtime loading. Unsupported execution must return structured errors, never placeholder success. Tests: build with/without ONNX, initialize/initialized/tools-list/call protocol exchange over newline-delimited stdio, EOF shutdown, stderr-only diagnostics, malformed arguments and missing assets, launch from an unrelated cwd. MCP framing follows the [transport specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports).

### Phase 2 — detection and OCR

Implement pure tensor helpers, provider/session factory, detector decoding/ordering, OCR letterbox and KV loop, then connect analyze. Tests: 1x1 RGB sentinel proves channel order and layout; asymmetric size proves W/H order; ties-to-even stride examples; invalid outputs/NaNs; synthetic row ordering; scripted decoder proves EOS, position clamp, cache-slot writes, cache reset and truncation. Optional real-model smoke must state model/runtime/provider and compare boxes/text to upstream reference. External OCR mode returns usable crops. No real weights means real-inference gate remains unverified.

### Phase 3 — masks and cleaning

Implement polygon confidence/offset, stroke masks/dilation, modulo-8 padding and LaMa composition. Tests: diagonal/rotated polygon distinguishes true offset from bbox inflation; 3x3 versus 5x5 dilation; tiny/odd sizes; mask binary values; empty-mask fast path; unmasked pixels byte-identical; known tensor output unpad and clamp. Real-page mask and clean PNG inspection is required for quality acceptance, not merely successful inference.

### Phase 4 — typesetting and interactive editing

Implement font discovery/config, shaped layout, ellipse containment, raster output, editor drag/resize/text/reorder/save. Test long text, CJK, combining Vietnamese accents, negative glyph bearings, narrow ellipses, explicit newlines, impossible fits and missing glyphs. Inspect representative PNGs in addition to numeric tests. Exercise editor HTTP isolation/save/load and browser coordinate behavior at non-unit zoom. Record browser checks separately if a browser is unavailable.

### Phase 5 — export, packaging and release verification

Implement all formats, ordered project workflow, license notices, offline provisioning README and example generic stdio client configuration. Test ZIP entries and path escapes, EPUB mimetype/container/spine, HTML offline assets/escaping, multi-page text round trips and export byte limits. Run fmt, clippy with warnings denied, unit/integration tests, default and no-default builds; check CUDA and DirectML feature compilation where supported. Launch binary from another directory. Real CPU and supported GPU results, asset/runtime sizes, cold/warm latency and peak RAM/VRAM are measured release evidence, not estimates. No release-ready claim while real-asset/provider/browser gates are missing.

## 10. Audit and reporting

For every milestone report files, exact commands/outcomes, assumptions/deviations and remaining acceptance gates. Distinguish implemented, compiled, unit-tested, real-model-tested and visually inspected. Coordinator reviews architecture/math and test evidence; only Luna edits implementation or test code. Maintain concise progress in `Internal_docs` without inflating the original brief into a claim of completed model validation.

### Initial delegation scope

The user's immediate goal is documentation followed by 1–2 Luna High coders. Both documents are delivered and two Luna High agents have been assigned. Their first run is bounded to a coherent verified foundation: core Phase 1 and pure preprocessing contracts, plus a typesetting milestone where ready. The five phases above remain the complete engineering roadmap; initial scaffolding is not completion of all five. Subsequent inference/editor/export work must clear its own gates. The coordinator reviews and requests fixes, but does not write Rust, JavaScript, or test implementation.

### Verified handoff state — 2026-09-05

Both delegated agents ran with model `gpt-5.6-luna`, reasoning `high`: `luna_core` and `luna_presentation`. Both subsequently terminated with the account usage-limit error. No implementation edits were made by the coordinator. Preserve their existing files and resume through Luna when capacity is available; do not restart the project or mistake an old binary for current validated behavior.

Coordinator reran these commands after the agents stopped:

| Command | Observed result |
| --- | --- |
| `cargo check --locked` | Passed against current integrated source. |
| `cargo check --locked --no-default-features` | Passed. |
| `cargo fmt --check` | Passed. |
| `cargo test --locked --lib --test core_contract` | Passed: 2 library and 5 core integration tests. Protocol test uses in-process duplex transport, not a child executable's stdio. |
| `cargo test --locked` | Failed during test compilation: E0499 in `tests/presentation_exports.rs`; the entry borrowed at line 63 remains alive while archive is borrowed again at lines 67, 73 and 75. Luna must end the first entry's borrow before subsequent archive access, then rerun. |

All five MCP routes currently return explicit unavailable/error results for normal processing; presentation modules exist but are not wired into those routes. The runtime module is preparatory code, not actual ONNX session initialization. Real detector/OCR/DBNet/LaMa inference, GPU fallback, external OCR crops, executable stdio behavior, editor/browser operation, visual layout quality and complete export correctness have **not** been established. Git metadata is still absent. The documentation-and-delegation deliverable is complete; the software roadmap is not.

Resume order: fix the export test borrow lifetime in Luna's presentation lane; run full tests and clippy; address earlier review findings (shared-schema export roundtrip and metadata preservation, complete glyph ink containment and legal Unicode line breaks); connect tested presentation modules through bounded tool execution; then proceed through inference and real-asset acceptance gates above. Reinspect current files before acting because this status records a checkpoint, not a substitute for source review.

### Resumed checkpoint — 2026-09-05, after usage refill

The same two Luna High agents resumed their saved lanes. This checkpoint supersedes the preceding build/test failure and unavailable presentation-route status; it does not supersede the full roadmap.

- Typeset, editor-server and export modules are now connected to their MCP routes. A shared one-slot semaphore bounds blocking jobs, and the owned permit stays inside the worker until its work finishes.
- Export test borrow errors are repaired. Tests cover ordered ZIP, EPUB first stored mimetype, escaped HTML content, and shared-schema/metadata roundtrips. Typesetting tests cover a combining accent, font upper bound and PNG/source preservation. These limited fixtures do not prove broad visual quality or all language cases.
- Editor HTTP tests cover Host restrictions and state persistence. The embedded UI currently provides zoom/pan, box display and save; full box dragging/resizing, translation editing and reading-order controls remain pending. Browser interaction has not been visually tested.
- Executable stdio test now uses a real generated PNG and temporary unrelated working directory. It exercises initialize, tool listing, an invalid call, editor-server creation and EOF exit. Read/wait timeouts and child kill-on-drop prevent the test from leaving an orphan process on failure.
- Coordinator independently reran `cargo test --locked --all-targets` (13 passed), `cargo clippy --locked --all-targets -- -D warnings` (passed), `cargo check --locked --no-default-features` (passed), and `cargo fmt --check` (passed).
- Analyze and clean remain explicit unavailable results. No real ONNX inference, GPU fallback, external OCR workflow or model-quality acceptance has been implemented/verified by this resumed pass. Next implementation work remains the documented inference stages and complete editor interactions.

All implementation/test changes in this resumed pass were made by Luna. The coordinator only inspected, ran checks, requested corrections and updated documentation.

### Multilingual RT-DETR checkpoint — 2026-09-05

One Luna High agent implemented the real local inference slice. `fukidashi_analyze_page` now uses RT-DETR for bubble/text-line detection, associates lines to bubbles, performs cached ONNX multilingual OCR routing for Han/Japanese, Korean and Latin/English, and returns an explicit pending translation handoff whose target defaults to Vietnamese. DB text detection remains a fallback when RT-DETR has no usable detections.

The five user-supplied English test pages were processed read-only through the MCP executable. Grouped results remained 14, 8, 4, 10 and 3 bubbles, with no unmatched text lines. The dedicated English graph now handles page 1 throughout (`en`); pages 2–5 remain on the shared Latin graph (`en|latin`). No page reports `mixed`, no Han recognizer is selected, and the prior mojibake is absent. The mobile recognizer still misreads stylized text and emits some blanks, so OCR quality remains an open product gate. Exact evidence is recorded in `Internal_docs/multilingual.md`.

After the final implementation edit, coordinator checks passed: formatting, the default all-target test suite (27 tests), and the no-default-feature suite (19 tests). Warnings-denied Clippy passed with and without default features. No commit was created.

### LaMa cleaning checkpoint — 2026-09-05

The same Luna High coder implemented `fukidashi_clean_page` with the installed dynamic LaMa ONNX graph. Automatic masks now come from retained text-line detections; whole bubble boxes are a fallback only when no line detections exist. The path includes mask/dimension limits, odd dilation validation, modulo-8 symmetric padding, tensor validation, unpadding and compositing that copies generated pixels only under the binary mask.

Both explicit-mask and automatic no-mask runs completed against the read-only first supplied page. The automatic path exercised analyze -> text lines -> mask -> LaMa and changed 85,984 pixels under an 89,254-pixel mask, with zero changes outside it. Coordinator visual inspection confirmed broad successful text removal with small residual artifacts in one bubble. Artifacts remain ignored under `target/test-artifacts`.

Coordinator reran the settled gates: `cargo fmt --check`; `cargo test --locked --all-targets` (19 tests: 8 library and 11 integration); `cargo test --locked --no-default-features` (15 tests); and warnings-denied Clippy with and without default features. All passed, as did `git diff --check`. No commit was created. OCR accuracy and mask-shape refinement remain quality gates. Browser editor expansion is intentionally deprioritized because the user's requested product is headless.

### Headless end-to-end checkpoint — 2026-09-05

The same Luna High coder exercised the actual stdio MCP tools on page 1: automatic analysis with Vietnamese target, client-side correction/translation of weak mobile OCR, automatic text-line cleaning, Vietnamese typesetting and ordered ZIP export. The run recorded `manual_source_correction: true` rather than presenting corrected transcription as OCR output. Vietnamese combining marks rendered in the 1000x1400 result, the export manifest language is `vi`, archive order is `project.json` then `pages/page-0000.png`, and the original source SHA-256 remained unchanged.

Generated artifacts and the workflow report live only under ignored `target/test-artifacts/headless-workflow`. Coordinator visually inspected both the cleaned and typeset PNGs. The headless path works, but the sample also shows current quality limits: broad rectangular masks, a few residual source marks, weak automatic transcription on stylized lettering, and simplistic placement where corrected translations do not match full source dialogue. These are refinement tasks rather than missing MCP plumbing.
