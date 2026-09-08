# Fukidashi MCP

Fukidashi MCP is an offline, headless Rust 2024 server for local comic page analysis, cleaning, typesetting, editing, review, and export. It exposes twelve MCP tools, including the strict-v1 `fukidashi_translation_start`/`fukidashi_translation_submit` pair and the single-call `fukidashi_review_and_export` review gate, read-only `fukidashi_get_config`, and persistent `fukidashi_configure` so each user can choose storage from any working directory. Supplied dialogue and translations are passed through verbatim.

The current foundation builds with Rust 1.97 and Cargo. The default feature enables dynamic ONNX Runtime loading through `ort` 2.0.0-rc.13; `--no-default-features` builds protocol and pure preprocessing without native inference. The executable never downloads models, runtime libraries, or fonts.

Comic Neue Regular, Comic Neue Bold, and Patrick Hand Regular are bundled in
the executable for typesetting. Their TTF bytes are embedded at compile time,
then copied atomically into each managed job's `fonts/` directory, so the
release works from any current directory without a font download. Comic Neue
Regular is the default dialogue face; explicit callers can use Comic Neue Bold
for emphasis, and Patrick Hand is selected when Vietnamese or another required
glyph is absent from Comic Neue. See `assets/fonts/README.md` for upstream
provenance, SIL Open Font License notices, attribution, and hashes.

Model assets are provisioned separately. At startup, paths resolve in this order: explicit CLI option, environment variable, saved user config, then a platform default. The default storage namespace is one `Fukidashi` directory: `%LOCALAPPDATA%\Fukidashi` on Windows, `$XDG_DATA_HOME/Fukidashi` (or `~/.local/share/Fukidashi`) on Linux, and the native Application Support location on macOS. It contains `models/`, `jobs/`, `cache/`, `temp/`, `runtime/`, `fonts/`, and `exports/`; `config.json` is stored in the native per-user config directory. The server creates the configured `cache/`, `temp/`, `runtime/`, and `exports/` directories at startup. Atomic artifact writes stage beside their final file so a temp directory on another volume cannot break replacement. The expected model files under `models/` are:

```text
detection/detector-v4-s_int8.onnx
inpainting/lama-manga-dynamic.onnx
ocr/ppocr-v5-onnx/ch_PP-OCRv5_mobile_det.onnx
detection/script_id/osd_lstm.onnx
detection/script_id/osd_labels.json
ocr/ppocr-v6-onnx/PP-OCRv6_small_rec.onnx
ocr/ppocr-v6-onnx/PP-OCRv6_small_rec.txt
ocr/ppocr-v5-onnx/korean_PP-OCRv5_rec_mobile_infer.onnx
ocr/ppocr-v5-onnx/ppocrv5_korean_dict.txt
ocr/ppocr-v5-onnx/latin_PP-OCRv5_rec_mobile_infer.onnx
ocr/ppocr-v5-onnx/ppocrv5_latin_dict.txt
ocr/ppocr-v5-onnx/en_PP-OCRv5_rec_mobile_infer.onnx
ocr/ppocr-v5-onnx/ppocrv5_en_dict.txt
ocr/manga-ocr-mobile-onnx/encoder.onnx
ocr/manga-ocr-mobile-onnx/decoder_init.onnx
ocr/manga-ocr-mobile-onnx/decoder_step.onnx
ocr/manga-ocr-mobile-onnx/vocab.txt
ocr/baberu-ocr/vision_int4.onnx
ocr/baberu-ocr/decoder_prefill_int8.onnx
ocr/baberu-ocr/decoder_step_int8.onnx
ocr/baberu-ocr/vocab.json
ocr/baberu-ocr/tokenizer_config.json
```

Use `fukidashi-mcp --models-dir <absolute-root> doctor` to inspect these paths. Local automatic OCR requires the RT-DETR detector, OSD graph plus `osd_labels.json`, and the PP-OCRv6 Han, PP-OCRv5 Korean and PP-OCRv5 Latin graphs with their matching dictionaries. RT-DETR label `0` is a bubble and labels `1`/`2` are text lines; line boxes are associated with bubbles before OCR, while all OCRed line regions are returned in `text_lines` and unmatched lines are also returned in `unmatched_text` for captions or other page text. The DB detector remains a fallback when RT-DETR returns no usable objects. `ORT_DYLIB_PATH` or `--ort-dylib` may point to an explicitly provisioned absolute ONNX Runtime library. Set `ORT_GPU_DEPS_DIR` to the directory containing the platform's CUDA/cuDNN dependencies when it is outside the configured `runtime/` directory. The checked-in `ort` feature set does not fetch a native runtime; provider availability must be verified against the installed runtime at startup. Every ONNX session uses ORT's API-22 GPU-preferred device policy, explicitly preloads CUDA dependencies when available, then retries session construction with an explicit CPU-preferred policy for provider/device failures. Startup reports the EP devices actually discovered by the loaded runtime on stderr, so a GPU driver alone is not treated as proof of GPU execution.

For bounded heterogeneous page runs, use `fukidashi-mcp --models-dir <models> --ort-dylib <onnxruntime.dll> benchmark-pages --fixture <directory> --mode cpu-only|gpu-only|cpu-gpu`. The `cpu-gpu` mode owns one CPU pipeline and one explicit CUDA pipeline, takes work from a bounded queue, and restores output order by page index. Each result records the worker and actual provider label; a CUDA worker that falls back is reported as `CPUExecutionProvider (GPU worker fallback)`.

The benchmark command supports CPU-only, GPU-only, and bounded CPU+GPU scheduling when an optional provider is installed. CPU is the safe default; acceleration is opt-in and reported with the actual provider used. The command prints machine-readable benchmark evidence to stdout; callers can save it wherever they keep test artifacts.

For model clients that tend to wander through managed files, use the strict-v1 loop: call `fukidashi_translation_start` with exactly one `image_path`, `job_path`, or `job_id`, then submit the exact stable ID set through `fukidashi_translation_submit`. The submit request contains only `work_token` and `translations`; each item supplies `translation` or explicit `keep_source: true`, with `needs_review: true` preserving uncertainty through the render. `sfx_mode` defaults to `preserve`: structurally unmatched `text-*` items are labeled `unmatched_text`, returned as preserved audit data, and excluded from required translation, cleaning, and typesetting; preserve-mode covers and SFX-only pages receive an explicit server-owned pass-through clean/render stage and advance automatically. Use `replace` explicitly to include unmatched items; a replace-mode page with no items is reported as `needs_manual_scope` and is never fabricated. The server owns page selection, analysis, clean, typeset, stage reuse, model release, and advancement. Tokens bind the current page and analysis hash, so stale or duplicate submissions fail closed. A final response returns `review_ready` and the exact `fukidashi_review_and_export` job argument; that call opens the editor and remains pending until review, returning fixes or exporting after approval. `fukidashi_serve_editor` plus `fukidashi_wait_for_review` remain compatibility tools. Never shell-read managed manifests/checkpoints or construct artifact paths.

`fukidashi_analyze_page` detects source scripts automatically for Japanese/Chinese, Korean and Latin/English regions. Omit `source_language` (or set it to `auto`) and optionally pass `target_language`; the target falls back to `FUKIDASHI_TARGET_LANGUAGE`, then Vietnamese (`vi`). Strong English evidence uses the dedicated English PP-OCR graph; other Latin text remains on the shared Latin graph. The original full response remains the default. Set `response_detail: "compact"` for long runs to receive one nonduplicated `translation_items` list with stable IDs, source text, confidence, and bboxes. Every analysis is atomically saved at the returned managed `workflow.analysis_path` under `jobs/<source-name>--<stable-id>/pages/0001/analysis.json`. Omit `checkpoint_path` on the first call; an optional checkpoint path is accepted only as a mirror inside that allocated job, after which the returned canonical path is authoritative. A vision-capable client may send optional `corrected_source_text: {"id": "corrected text"}` in a follow-up request. The calling MCP client supplies translations; local OCR does not silently translate or require hosted credentials.

When the complete optional `ocr/baberu-ocr` bundle is present, Baberu is the primary recognizer for bubble and associated text crops in its supported Japanese, Chinese and English families. It uses the upstream 224-pixel RGB ImageNet preprocessing, 14,630-character vocabulary, BOS/EOS IDs, six-layer KV cache, position start at 257, greedy decoding, repetition penalty 1.2 and content-run cap 12. Missing or invalid Baberu assets leave the existing MangaOCR, Han, English and Latin routes active. The client-side vision correction contract and non-vision `ocr_text` path are unchanged.

Configure once from any directory through MCP: call `fukidashi_configure` with `{"storage_root":"<absolute-path>/Fukidashi","provider":"cpu"}`, then restart the MCP. On Windows use a drive-qualified path such as `D:/Fukidashi`; on Linux use `/data/Fukidashi` or a path below `$HOME`. Set `FUKIDASHI_CACHE_DIR` or `FUKIDASHI_TEMP_DIR` when those writable namespaces should live outside the storage root; they are created during startup. `fukidashi_get_config {}` reports effective paths, their source, missing models, runtime discovery, and whether a restart is required. The same setup is available from the CLI: `fukidashi-mcp config-set --storage-root <absolute-path> --provider cpu`, followed by `fukidashi-mcp config-show`.

Install one native MCP engine for all supported clients with `fukidashi-mcp install`. The installer auto-detects Codex CLI/Desktop/IDE, Claude Code, Antigravity, Gemini CLI, Cursor, and VS Code; use `--client codex,claude` for an explicit selection or `--all` to configure every adapter. It stores the executable, workflow context, install manifest, and recoverable config backups in the platform data directory (`%LOCALAPPDATA%\\Fukidashi` on Windows or `$XDG_DATA_HOME/Fukidashi` on Linux). `fukidashi-mcp status` reports adapter state, `doctor` includes adapter diagnostics, `uninstall` removes only the Fukidashi entries and owned files, and `rollback` restores backups only when a client config is unchanged since installation. Client config files are updated atomically and unrelated MCP servers are preserved. The installer never downloads models or performs inference. Set `FUKIDASHI_DATA_DIR` to choose a different installer data directory. VS Code uses the conventional `Code/User/mcp.json` profile; additional VS Code profiles may need their own adapter entry.

Windows PowerShell can use explicit overrides when keeping an existing layout:

```powershell
$env:FUKIDASHI_STORAGE_ROOT = 'D:\Fukidashi'
$env:FUKIDASHI_JOBS_ROOT = '<jobs-root>' # selects the jobs root for managed jobs
$env:FUKIDASHI_PROVIDER = 'cpu'                       # default; use cuda/auto only when installed
fukidashi-mcp
```

Linux shell equivalent:

```sh
export FUKIDASHI_STORAGE_ROOT="$HOME/.local/share/Fukidashi"
export FUKIDASHI_PROVIDER=cpu
fukidashi-mcp
```

No path change moves, deletes, or downloads existing models or jobs. A legacy jobs root such as `<jobs-root>` remains usable through `FUKIDASHI_JOBS_ROOT`; migration is a separate, explicit dry-run operation. New jobs use `jobs/<source-name>--<short-id>/` with `pages/0001/`, `fonts/`, `backups/`, `review/`, and `output/` directories. The server never reads arbitrary paths outside managed roots.

All generated artifacts are placed under the effective jobs directory (default `<storage_root>/jobs`), in a server-created job directory. Source folders are never used as an output directory. Clean and rendered files have JSON sidecars that record the stage, source path, SHA-256 hashes, and QA information.

The server uses a single heavy-operation permit, so concurrent page requests from a model are queued rather than loading multiple OCR, LaMa, or typeset jobs at once. This protects GPU and RAM even when a client fires a whole batch in parallel.

The managed `job.json` marker captures the expected image inventory on the first analysis call and records each page as `pending`, `analyzed`, `cleaned`, or `rendered`. Existing `.fukidashi-job.json` markers are accepted for legacy jobs and are updated in place; new jobs write only `job.json`. Review and MCP export refuse to proceed until every expected page is rendered exactly once. For a cover or bounded test, pass `scope: {"start_page": 2, "end_page": 5}` (one-based natural filename order), or `scope: {"include_paths": ["..."]}` on the first analysis call; the scope is persisted and later calls must use the same inventory. The dark-pixel reduction metric is a deterministic cleaning signal; it catches unchanged/raw inputs, while the gallery remains the final visual check for faint residue or artwork damage.

`fukidashi_clean_page` runs the local LaMa graph and writes a new PNG plus its grayscale mask into the server-owned job directory. The server checks dimensions, rejects an empty or unchanged mask, and verifies that masked source pixels actually changed before returning `cleaned_image_path`. An explicit `mask_path` is validated against the source dimensions and rejected when it contains a dense filled region. Without one, RT-DETR/DB text-line geometry supplies the search regions and pixel connected components supply the stroke mask; bubble boxes are never filled as a cleaning fallback. `dilation` is an odd mask kernel size from 1 to 15 and defaults to 3. Unmasked source pixels are preserved byte-for-byte. The detector supplies geometry, DB supplies text-line evidence, and Baberu supplies transcription only.

For bounded pages, set `mode: "crop"` and pass `analysis_path` (or explicit `text_regions`) to clean only padded, stride-aligned LaMa crops. Crop mode keeps the original detector mask as a seed and permits antialias expansion only within a small seed neighbourhood, so disconnected dark artwork inside a broad OCR box remains untouched. Checkpoint text-line boxes below 0.5 confidence are ignored because they are frequently art or effect false positives. The response includes `crop_count` and the crop rectangles for review. Use `mode: "full"` for compatibility with page-sized inference.

If CUDA LaMa execution fails with a provider/device allocation error, cleaning drops the CUDA LaMa session and retries that same mask once on CPU. It does not reload OCR. The result reports `provider`, `provider_fallback`, and a compact `provider_fallback_reason`; the full ORT error remains on stderr. Non-provider tensor/graph errors are returned directly and are not hidden by a retry.

### Stability-first long runs

The defaults favor a 16 GiB machine over throughput. Variable-size ORT memory patterns are disabled, CUDA uses same-as-requested arena growth, heuristic convolution selection, a 2048 MiB arena limit, and no maximum convolution workspace. The limit covers the CUDA arena; CUDA runtime and provider allocations can still add overhead. Every completed analyze or clean call drops all cached model sessions by default. Cleaning also drops the OCR model family before loading LaMa, so their session arenas do not coexist.

Tune only after measuring a bounded sample:

```text
FUKIDASHI_GPU_MEMORY_LIMIT_MIB=2048
FUKIDASHI_SESSION_RECYCLE_PAGES=1
FUKIDASHI_RETAIN_STAGE_SESSIONS=0
```

`FUKIDASHI_SESSION_RECYCLE_PAGES=0` disables automatic recycling. `FUKIDASHI_RETAIN_STAGE_SESSIONS=1` allows OCR and LaMa sessions to overlap and raises peak memory. `fukidashi_release_models {}` explicitly drops both model families between pages or chunks.

For Claude or another agent, start meaningful content at page 2 when page 1 is a cover by setting the scope on the first call. Process one page per fresh call, store the full result under a per-page checkpoint path, and keep only the compact response in conversation. Persist the cleaned/rendered paths and status after each page. Start a fresh model conversation every 5–10 pages and resume from the first page without a completed checkpoint. Never paste all prior OCR payloads into the next chunk. A safe analyze request is:

```json
{
  "image_path": "<source-root>/2.webp",
  "ocr_mode": "local",
  "source_language": "auto",
  "target_language": "vi",
  "response_detail": "compact",
  "scope": {"start_page": 2}
}
```

`fukidashi_typeset` only accepts the exact `cleaned_image_path` returned by `fukidashi_clean_page`; passing the original source image is rejected even when the path is otherwise valid. The server checks the clean sidecar and source/clean hashes, so a model cannot accidentally overlay translations on raw artwork. It writes the render into the same server-owned job and emits a render sidecar containing the request bubbles and fit report for the review editor. It accepts the original placement `bbox` plus optional `bubble_bbox`, `text_bbox`, and `padding`. Request-level `font_path`, `padding`, `min_font_size`, `max_font_size`, and `shape` values fill missing bubble fields; an explicit value on a bubble wins. `fallback_font_paths` supplies ordered whole-font fallbacks. If no configured/platform font covers every character, the call fails with the missing Unicode code points; clients must preserve symbols such as `❤` and choose a fallback rather than deleting them. A valid bubble box defines the containing area, while the text box is only an anchor. The renderer derives an inset safe rectangle, wraps and shrinks shaped glyphs until their bounds fit, and returns page-coordinate preview metadata including the safe box, padding, font size, line breaks, ink bounds, and placement centre. Post-render QA reports source-clean confidence, overflow, and missing ink bounds. A vision client can use that metadata to adjust text, font limits, padding, or geometry and submit another typeset request.

`fukidashi_review_and_export` is the preferred review gate after strict-v1: pass the exact returned `job_id` and optional `format` (default `zip`), and the server opens the local editor, keeps the MCP call pending for one review submission, returns feedback for the AI when fixes are requested, or exports after approval. `fukidashi_serve_editor` can reopen an existing job without artifact path guessing for compatibility. Use exactly `{"job_path":"<jobs-root>/<job-id>"}` (or `{"job_id":"<job-id>"}`); the server verifies the managed `job.json` (or an existing legacy marker), selects a valid rendered page, and builds the complete gallery from all expected pages and render sidecars. `image_path` remains supported for a known rendered artifact. `json_data` is optional metadata only. If validation fails, follow the returned example and do not shell-search or guess paths. The server builds the complete gallery pages from the managed manifest and render sidecars. Legacy sidecars produce valid pages with `bubbles: []` and `issues: []`, so missing model fields cannot crash the UI. It atomically persists the complete `project.json` before returning its authenticated loopback URL, project path, review session ID, revision, and review state path. The local gallery presents human-readable Page 1, Page 2, and so on, with a responsive thumbnail grid, Source/Cleaned/Rendered views, cyan OCR bubbles, and orange issue regions. Use **Preserve original / remove translation box** for an untranslated bubble; the tombstone survives reload, source pixels are restored during render, and **Restore original bubble** undoes it. Empty translations never fall back to stale source text. Draw as many issue rectangles as needed on any page; move or resize them with handles, edit their notes, undo or clear them, and let autosave restore the state after reload. Cyan flags are actionable feedback with the bubble ID, bbox, source OCR, current translation, and origin; Send marked issues to AI is enabled for orange issues and cyan flags. Bubble, delete, and brush edits mark pages render-dirty; approval rerenders dirty pages first, while the backend rejects unresolved issues, flags, or dirty pages. There are no per-page approval clicks. Draft review data is atomically persisted in `review.json` with an audit copy in `review-audit.json`. `fukidashi_wait_for_review` waits asynchronously for the matching revision's Submit fixes or Approve/export action without taking the heavy inference semaphore. A submitted fix response contains the session ID, revision, stable bubble ID when known, pixel bbox, issue type, note, corrected text, and project artifact paths. MCP export requires a managed job that has been served through the editor and rejects the job until every page has been explicitly approved through the current revision. Rendering uses an explicit project-local `font_path` when one is supplied. If a bubble and the request both omit it, the server materializes the bundled Comic Neue Regular face automatically; its bundled Patrick Hand and Comic Neue Bold whole-font fallbacks are appended after caller-supplied fallbacks. This keeps the endpoint from reading arbitrary filesystem paths while allowing normal Vietnamese text to render without external font setup. The session serves only its captured project tree plus exact manifest-approved source files, enforces a 2 MiB JSON cap, validates bboxes and page bounds, and checks the exact loopback Host and same-origin headers. There is no CDN or browser automation dependency.
The Cleaned view also has a reversible correction brush: choose a pixel brush size, paint `Cover` strokes, or use `Restore` to reveal the original cleaned pixels. Strokes are stored in `project.json`, autosaved, undoable, and never modify the source or original cleaned PNG. Use `Save`, then `Apply brush & render page`; the server atomically creates a managed corrected-clean derivative, typesets from that derivative, records its provenance, and switches the page to Rendered. With no strokes, the action typesets from the verified cleaned artifact.

Run the MCP server with no subcommand. Stdout is reserved for MCP frames and diagnostics go to stderr:

```text
fukidashi-mcp --models-dir <absolute-models-root>
```

The current checkpoint provides local RT-DETR bubble and text-line detection, deterministic association and reading order, OSD-backed multilingual routing, PP-OCR recognition sessions, LaMa cleaning from text-line masks (or an explicit grayscale mask), explicit pending translation handoff, and the authenticated multi-page review editor alongside the presentation layout/export primitives. Missing assets are reported as actionable errors rather than fabricated detections or cleaned pages.

## Attribution and licensing

This project is Apache-2.0. Adapted preprocessing and model behavior are based on Comic Translate at upstream revision `8f13ae5c4bab567c12b9383f085ba32d98b43348`, whose Apache-2.0 license is retained in `LICENSE-Comic-Translate` when redistributing adapted source. Model weights and ONNX Runtime binaries remain separately provisioned and retain their own upstream licenses. The bundled Comic Neue and Patrick Hand TTFs are SIL Open Font License 1.1 assets; retain `assets/fonts/ComicNeue-OFL.txt`, `assets/fonts/PatrickHand-OFL.txt`, and `assets/fonts/README.md` with binary release notices.

### Manual bubble editing

The review editor has two correction lanes. Double-click a cyan OCR/translation bubble in Source, Cleaned, or Rendered view to open the focused editor. It shows the saved source OCR as read-only plus the current Vietnamese translation, bubble ID, page, font size, and padding. `Save translation` persists only that bubble with revision protection; `Save & rerender this page` saves and rerenders only the current page from the verified cleaned or corrected-clean artifact. Cancel discards the modal edits. Empty translations require explicit confirmation and never silently fall back to source text.

Orange rectangles are the AI lane: use them for wrong or missing segmentation, damaged artwork, or a translation that needs model help. A `wrong_translation` issue is linked to one overlapping stable bubble ID when the overlap is unambiguous and includes source OCR/current translation in feedback; ambiguous overlaps are surfaced instead of guessed. Use `Send marked issues to AI` only for this lane. Manual edits do not create AI feedback. A stale browser save or render receives HTTP 409 with a reload instruction, and autosave stops until the project is reloaded.

This interaction follows the original Comic Translate editor behavior studied in the upstream `app/ui/canvas/text_item.py` (`mouseDoubleClickEvent` enters text editing) and `app/controllers/rect_item.py` (`handle_rectangle_selection` loads source/translation fields), with commit behavior from `app/controllers/text.py` (`update_text_block_from_edit`).

Fonts are copied into the managed job before they are used for an editable render. For an existing recovered job, inspect the safe dry run with `fukidashi-mcp font-migrate --job <jobs-root>\<job-id>` and then use `--apply` once the listed Windows Fonts or existing managed fonts are expected. The command creates a timestamped `project.json` backup, rewrites only project-local `fonts/...` paths, preserves translations, geometry, issues, brush strokes, review state, and increments the project revision. It does not rerun OCR, typesetting, or export. Do not repeat legacy recovery after manual translation edits; use font migration for this repair.


