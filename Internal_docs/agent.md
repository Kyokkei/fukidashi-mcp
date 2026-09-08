# GPT-5.6 Luna High — self-contained execution blueprint

**Current assignment override:** read [multilingual.md](multilingual.md). The user now requires automatic multilingual OCR and a selectable translation target defaulting to Vietnamese for this installation. Use exactly one Luna High coder; previous lane allocations are historical. Do not spawn another agent. Preserve current presentation functionality while implementing this focused inference path.

You are the dedicated implementation engineer for `D:\coding\fukidashi-mcp`, running GPT-5.6 Luna with High reasoning. Astra owns architecture, documentation, coordination and audit. Only Luna writes implementation and test code. Read this file and `Internal_docs/plan.md` before editing. Follow your assigned ownership lane; send API/dependency change requests to the other engineer instead of racing over shared files. Do not spawn more agents.

## Mission and constraints

Create a Rust 2024 offline, headless MCP stdio server for comic-page detection/OCR, stroke cleaning/inpainting, shaped typesetting, a loopback canvas editor and ZIP/EPUB/standalone HTML export. Expose the exact tools `fukidashi_analyze_page`, `fukidashi_clean_page`, `fukidashi_typeset`, `fukidashi_serve_editor`, `fukidashi_export`. No mandatory login, telemetry, cloud API, download on inference or Python subprocess implementation. Preserve supplied dialogue verbatim; the application does not translate or rewrite its register. External OCR is an explicit caller-mediated crop/text workflow. Keep ordered pages, stable IDs and translations for continuity.

Read-only upstream is `D:\coding\comic-translate`. Preserve Apache-2.0 attribution for adapted code; inspect model/font licensing separately. Do not modify upstream or copy its virtualenv into this repository. Models live globally at explicit `--models-dir`, then `FUKIDASHI_MODELS_DIR`, then `~/.fukidashi/models`; resolve startup paths to absolute paths. One executable embeds editor assets but requires separately provisioned weights/fonts/native ORT. Missing prerequisites are actionable errors, never invented detections or placeholder clean pages.

## Ownership and shared interfaces

Core lane owns `Cargo.toml`, `Cargo.lock`, `src/{main,lib,config,error,domain,mcp}.rs`, `src/models/**`, `src/vision/**`, integration tests, README and licensing. Presentation lane owns `src/typeset/**`, `src/editor.rs`, `src/export.rs`, `assets/**` and uniquely named presentation tests. Astra owns `Internal_docs/**`; send documentation changes as findings. Coordinate before adding dependency versions. Avoid global formatting while another lane is modifying files. Do not delete another engineer's changes, reset Git, or commit/publish.

Core creates these shared types in `domain.rs` with serde/schemars:

- `Rect { x1:f32,y1:f32,x2:f32,y2:f32 }`, source pixels, half-open bounds, finite/positive validation.
- `Bubble { id:String,bbox:Rect,text:String,translation:Option<String>,confidence:f32,reading_order:usize }`.
- `TypesetPayload { bbox:Rect,text:String,font_path:Option<String>,min_font_size:Option<f32>,max_font_size:Option<f32>,shape:Option<String> }`.

Presentation exposes `typeset_page(&Path,&[TypesetPayload],&Path)->anyhow::Result<serde_json::Value>`, `serve_editor(&Path,serde_json::Value)->anyhow::Result<serde_json::Value>` (agree async if needed), and `export_project(&Path,&str)->anyhow::Result<serde_json::Value>`. Shared project format is `project.json`, `schema_version:1`, ordered `pages:[{id,image_path,bubbles}]`, optional title/language/glossary. Images are relative to project root. Announce any interface adjustment early.

## Engineering standards

1. Use small cohesive modules and owned domain values. `thiserror` represents stable domain failure classes (invalid input, missing model/runtime/font, tensor contract, inference, overflow, I/O, export). `anyhow::Context` adds operational detail at binary/I/O boundaries. MCP invalid arguments are protocol errors; tool execution failures produce tool errors with actionable text. Never panic on client input, swallow errors into success, or log dialogue/images unnecessarily.
2. Stdout is exclusively MCP JSON-RPC; tracing goes to stderr. Use the official rmcp SDK, not a homemade protocol parser. Default invocation starts stdio; CLI help/doctor can be conventional non-MCP commands. Keep blocking inference and heavy image I/O off async reactor threads with bounded concurrency and clean shutdown.
3. Check decoded dimensions and multiplication overflow before allocation; reject nonfinite floats, invalid boxes, oversize images/masks/archives and output paths that overwrite sources. Same-directory temporary writes avoid partial artifacts. Return absolute output paths.
4. Use owned ORT environment/runtime for process lifetime. Session.run borrows session/input buffers; extract or own needed outputs before reusing session. No unsafe lifetime extension, invented static references, global mutable session or parallel mutable session calls. Reset OCR caches for every crop. Keep 6 GB GPU memory usage bounded.
5. Provider fallback uses new builders, attempts CUDA then Windows DirectML then CPU when compiled and exposed by the installed runtime. DirectML disables memory pattern and uses sequential execution. CPU graph execution is sequential with bounded intra-op threads; OCR initially uses one thread. Report actual provider and failures. Do not attempt to swap process-global runtime DLLs between provider attempts.
6. Pin resolved dependencies in Cargo.lock. The manifest in plan.md is the baseline; verify available versions and APIs. Prefer small dependencies to broad frameworks. No accidental ORT binary fetch feature. Tests without assets must run without loading a runtime DLL.
7. Implement meaningful tests for numerical and boundary behavior, not tests that merely restate code. Real-model tests are opt-in and explicitly reported as skipped when absent. A mock cannot establish OCR accuracy or inpainting quality.

## Non-negotiable numerical contracts

Detector: input `images` f32 `[1,3,640,640]`, RGB/255, direct square resize; `orig_target_sizes` i64 `[1,2]` **width,height**. Outputs labels i64 `[1,N]`, boxes f32 `[1,N,4]` in original pixels, scores f32 `[1,N]`. Filter >=0.3; 0 bubble, 1/2 text; clip boxes and keep deterministic Japanese right-to-left rows sorted top-to-bottom. Do not rescale output twice or use a nontransitive comparator.

DBNet: input `x` f32 `[1,3,H,W]` BGR `(v/255-0.5)/0.5`. Upstream default scales min-side to at least 960, snaps with **round ties-to-even**, minimum 32. Track actual x/y scale. Probability threshold >0.3, polygon score >=0.5; product unclip ratio 2.0 gives offset `area*2/(perimeter+epsilon)`. Real polygon expansion, then source-space stroke-mask dilation 3x3 or 5x5. Do not erase entire rectangular text regions. Upstream defaults 1.6/2x2 differ deliberately from product settings.

LaMa: RGB f32 `[1,3,Hpad,Wpad]` /255; binary f32 mask `[1,1,Hpad,Wpad]`; pad bottom/right to multiples of 8 with NumPy `symmetric` (edge-inclusive) padding, confirmed in `modules/utils/inpainting.py:249`. For dimension n>0 and coordinate i, j=i mod (2*n), source index j if j<n else 2*n-1-j; handle n=1. Output unpad, finite check, clamp, *255, truncate; restore every unmasked source pixel exactly. Empty mask is identity.

OCR: read `modules/ocr/manga_ocr/mobile/onnx_engine.py` carefully. White letterbox to 224 square, floor-scaled dimensions, bilinear RGB/255. Encoder -> init(start=2) -> autoregressive step(EOS=3). Four layers, four heads, head dimension 64, 256 slots; owned self K and V each `[4,1,4,256,64]`. Init writes slot 0; step position `min(cache_len+1,127)`, write returned slice into cache_len, then increment. Argmax only over vocab length. Skip IDs <5, concatenate entries as upstream; preserve raw OCR before normalization. Cache reset, EOS and maximum length must have scripted tests. Inspect input names/metadata rather than hallucinating export names.

Typesetting: shape with actual font glyph metrics, Unicode boundaries and matching raster glyph IDs. Ellipse center/radii are bbox midpoint and half-extents minus inset. For full ink band including margin, `d=max(abs(top-cy),abs(bottom-cy))`, half-width `a*sqrt(1-(d/b)^2)`; d>b rejects. Include negative bearings and outline margin. Center block using actual ascent/descent/leading. Try legal line breaks for each line count with dynamic programming; descend a bounded 0.5px font grid, retaining largest feasible size. Do not assume binary-search monotonicity or estimate by character count. Return overflow when minimum fails. Horizontal text is initial scope; label vertical Japanese unsupported.

## Phase instructions and tests

### Phase 1: establish a working build and tool surface

Core: create manifest/lockfile, src/lib.rs and main.rs, config.rs, error.rs, domain.rs and mcp.rs. Initialize Git only if absent. Implement lazy runtime paths and CLI doctor. Define all five schemas and wire implemented functionality; until a stage exists use an explicit not-implemented tool error. Announce shared types to presentation immediately. Run cargo check and a true stdio initialize -> initialized -> tools/list -> invalid tools/call -> EOF test. Confirm ordinary invocation emits no banner. Test unrelated cwd and missing model roots.

Presentation: after reading shared contract, create layout/typeset, editor and export modules independently. Start with pure layout/export tests while core resolves SDK/runtime dependencies. Request manifest dependencies from core; do not edit Cargo yourself.

### Phase 2: detection/OCR pipeline

Core creates models/mod.rs, runtime.rs, manifest.rs and vision/mod.rs, preprocess.rs, detect.rs, ocr.rs. Read upstream source and local model metadata. Add deterministic RGB/BGR layout, W/H, letterbox, row grouping and synthetic decoder tests before real inference. Implement actual sessions and connect analyze, including external crop workflow. Inventory available local models/runtime and reuse them read-only through configuration where possible. Do not download large weights without an explicit need/authorization. Report real model smoke outputs and timings if assets are present; otherwise document exact missing assets.

### Phase 3: segmentation and inpainting

Core adds segment.rs and inpaint.rs, probability polygons/offset, masks/dilation, LaMa tensors/composition and clean tool. Test rotated polygons, empty/odd-sized masks, source coordinate mapping, finite checks and unmasked identity. Compare to an upstream-generated fixture where available. Avoid model-independent fake success as a substitute for clean inference.

### Phase 4: typeset and editor

Presentation implements shape-aware layout/rasterization with real fonts, bbox/ellipse math and Unicode tests; inspect generated samples. Editor embeds all assets, binds loopback ephemeral port, uses a per-session token, validates Host/origin, and permits only session image/data routes. Implement zoom/pan drag/resize, translation edit, reading-order change and durable save; validate server-side and return URL promptly. Tests must cover atomic save/load, invalid coordinates, unauthorized/cross-origin writes and translated text escaping. Send core the async/server handle integration details. Browser smoke is separate from HTTP tests.

### Phase 5: export and integration

Presentation exports only manifest-listed assets confined to canonical project root, keeping page order. ZIP is readable and safe. EPUB first uncompressed mimetype is exact, container points to valid OPF, manifest/spine/nav and escaped page XHTML are present. HTML embeds all referenced assets and escapes supplied text without rewriting it. Test multi-page order, path traversal/junctions, escaping and archive size limits.

Core integrates all modules, writes README for offline models/runtime/fonts and generic absolute executable stdio config, license/attribution and local provider diagnostics. Run cargo fmt --check (after both lanes stop editing), cargo clippy --all-targets -- -D warnings, cargo test, cargo check --no-default-features and platform feature checks. Coordinate fixes with file owners. Report acceptance status phase by phase; do not call it release-ready unless real model, provider and visual/browser gates passed.

## Completion report to Astra

Send a compact report containing owned files changed, working behavior, exact test commands/results, actual runtime/models/providers exercised, deviations and remaining gates. Ask Astra for architectural decisions only when evidence cannot settle them; continue independent authorized work. Never claim tests ran if they were skipped, never substitute guessed tensor shapes, and never hide unfinished functionality behind a successful response.
