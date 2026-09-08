# Automatic multilingual OCR and translation handoff

User correction, 2026-09-05: pages should be read automatically across Japanese, Korean, Chinese and English, then translated to the user's selected target language. This installation defaults to Vietnamese (`vi`). Source language selection must not be a prerequisite. Exactly one GPT-5.6 Luna High engineer writes all implementation/test changes; Astra updates documents and audits. No extra agents or unrelated presentation work.

## Requested behavior

`fukidashi_analyze_page(image_path)` defaults to `source_language=auto`. Detect text regions, choose appropriate recognizers automatically, and return actual recognized text with source pixel coordinates, stable ordering, recognizer identity and confidence/uncertainty. Support mixed-script pages by routing per region where possible. Optional source override remains useful but is never mandatory. Preserve existing callers and no-ONNX explicit errors.

Add a configurable target language: explicit request > configured CLI/environment preference > `vi`. Use language tags, accepting targets beyond the four OCR source languages. Persist the target in project/analysis metadata so downstream translation and typesetting use the same preference. Do not conflate source recognition support with translation target support.

OCR performs local image-to-text inference. Translation is performed by the calling MCP model/user, as previously agreed, without adding cloud credentials or a mandatory hosted service. Return an explicit translation handoff containing target language, ordered source text and stable IDs/context, with translations marked pending. An untranslated string is never reported as a completed Vietnamese translation. Tool description and README must explain the automatic client workflow and its reliance on client orchestration. A standalone offline translation model is not silently implied.

## Installed evidence and upstream contracts

Read-only source: `D:\coding\comic-translate`. Available weights/dictionaries under `C:\Users\Yozora\AppData\Local\ComicTranslate\models`:

- `detection/detector-v4-s_int8.onnx` (bubble/text labels).
- `detection/script_id/osd_lstm.onnx` and `osd_labels.json` (script classifier).
- `ocr/ppocr-v6-onnx/PP-OCRv6_small_rec.onnx` and `.txt` (upstream explicitly maps Chinese and Japanese to this recognizer).
- `ocr/ppocr-v5-onnx/korean_PP-OCRv5_rec_mobile_infer.onnx` and `ppocrv5_korean_dict.txt`.
- `ocr/ppocr-v5-onnx/latin_PP-OCRv5_rec_mobile_infer.onnx` and `ppocrv5_latin_dict.txt` (candidate Latin/English path; verify actual graph/vocab and real text).
- `ocr/ppocr-v5-onnx/ch_PP-OCRv5_mobile_det.onnx` (text detection).
- Manga OCR mobile assets remain optional Japanese specialization; they are not the universal engine.

Native ORT 1.29.0 is installed at `D:\coding\comic-translate\.venv\Lib\site-packages\onnxruntime\capi\onnxruntime.dll`. Use explicit absolute runtime/model overrides for tests, never hardcode personal paths as shipped defaults. No large model downloads or Python inference subprocesses. Asset names/presence do not prove graph compatibility.

Read `modules/ocr/ppocr/engine.py`, `preprocessing.py`, `postprocessing.py`, and `modules/detection/script_detection.py` before porting. PP recognizer uses BGR normalization to [-1,1], height 48, ratio-preserving width and graph-compatible padding. CTC blank/repeat handling and dictionary class count must match inspected graph metadata. Do not copy the upstream dimensional heuristic blindly; validate actual output axes against dictionary class count.

OSD uses its own grayscale/black-white normalization and class aggregation, not PP-OCR preprocessing. Script identity is not language identity: Han-only text may be Chinese or Japanese; Latin script alone does not prove English. Report ambiguity, use shared recognizers/candidate evidence, and avoid comparing uncalibrated cross-model scores as guaranteed language probabilities. Do not require a text OCR result to magically exist before choosing an engine. Inspect classifier behavior or explicitly document a bounded candidate-recognition strategy if chosen instead.

## Focused acceptance

1. Actual ONNX runtime/session implementation and a working `analyze_page` path; no placeholder empty bubbles or fake success. Reuse existing semaphore to serialize heavy work. Keep runtime alive, sessions cached/lazy, buffers owned, memory limits enforced and logs on stderr.
2. Automatic source routing for the four requested language families, plus explicit overrides and honest uncertain/mixed results. Keep source reading order appropriate to orientation/script rather than applying Japanese RTL indiscriminately to Korean/English pages.
3. Vietnamese default and override to another target language survive request/result/project metadata. Translation handoff remains explicitly pending until client-supplied translated text exists.
4. Deterministic tests for BGR/tensor shapes, CTC blanks/repeats/dictionary alignment, routing including ambiguity, target precedence, invalid assets and backwards compatibility. Real local model smoke on representative multilingual fixtures; generated neutral text fixtures may be created by Luna using installed fonts. Record source strings and recognized results; a blank image does not prove OCR accuracy. Separate model execution from accuracy assertions.
5. Preserve all existing tests, run fmt/clippy/default/no-default checks once changes settle. One engineer owns commands; coordinator reviews evidence and does not duplicate builds concurrently. Report any unsupported real case rather than relabeling a schema-only implementation as multilingual OCR.

Work only on this inference/configuration/handoff slice. Inpainting, editor completion, publishing and broad refactors remain outside this pass. Send one early evidence checkpoint (actual graph contracts and routing decision), then a final result with exact tests. No repeated quota checks, broad source dumps or background agent fan-out.

## Implemented and verified checkpoint — 2026-09-05

One `gpt-5.6-luna` agent with High reasoning implemented this slice; Sol/Astra only audited and updated documentation. `fukidashi_analyze_page` now runs local Rust ONNX DB text detection, OSD script routing and PP-OCR recognition with cached sessions behind the shared one-job semaphore. Automatic routing supports the requested Han/Japanese, Korean and Latin/English families. `source_language=auto` equals omission. Han-only and Latin-only evidence stays ambiguous where language cannot be established reliably.

Target precedence is request `target_language` > `FUKIDASHI_TARGET_LANGUAGE` > `vi`. Results contain the selected target plus an ordered `translation_handoff`; every item remains `translation: null`, `status: pending` until the MCP client supplies a translation. The server does not falsely report local translation.

The installed graphs were inspected and exercised with ONNX Runtime 1.29.0 from `D:\coding\comic-translate\.venv\Lib\site-packages\onnxruntime\capi\onnxruntime.dll` and models under `C:\Users\Yozora\AppData\Local\ComicTranslate\models`. Neutral fixture evidence:

| Expected source | Actual OCR | Route/source evidence |
| --- | --- | --- |
| `こんにちは` | `ニんこちは` | Han recognizer; Japanese (`ja`) |
| `你好世界` | `你好世界` | Han recognizer; ambiguous `ja|zh` |
| `안녕하세요` | `안녕하세오` | Korean recognizer; `ko` |
| `Hello world` | `Hello` and `worlc` in two regions | Latin recognizer; ambiguous `en|latin` |

This proves real execution and automatic family routing, while also showing normal mobile-model recognition errors and a DB detector split. It does not prove perfect OCR accuracy. A request-level `fr` target propagated through the response and all translations remained pending. The temporary hardcoded smoke-test source was removed; its generated PNG remains only inside ignored `target` build output.

Final Luna verification reported: `cargo fmt`; `cargo test` (six OCR/library unit tests plus all integration tests passed); `cargo test --no-default-features` passed; `cargo clippy --all-targets --all-features -- -D warnings` passed; and no-default Clippy with warnings denied passed. Deterministic coverage includes target fallback to `vi`, explicit `auto`, CTC blanks/repeats/spaces, ambiguity routing, and mixed Han/Latin total ordering.

The subsequent RT-DETR pass made `detector-v4-s_int8.onnx` the primary page detector and kept DB detection only as a zero-result fallback. The implementation accepts generic `[1,N]` label/score tensors and `[1,N,4]` boxes, clips finite boxes to page bounds, deduplicates detections, associates label 1/2 text lines to label 0 bubbles, preserves unmatched text, and assigns stable IDs after deterministic reading-order sorting. Each region exposes detector label and confidence. Translation handoff items also copy OCR confidence and retain the image-path/bbox correction contract; optional stable-ID corrections update `source_text` while preserving `ocr_text`.

The user-supplied five-page English comic fixture was exercised read-only through the real MCP path. The ignored report is `target/test-artifacts/real-pages.json`; source pages were neither copied nor committed. RT-DETR and grouped output counts were:

| Page | Raw bubble/text detections | Grouped bubbles | Unmatched text |
| --- | ---: | ---: | ---: |
| 1 | 14 / 14 | 14 | 0 |
| 2 | 8 / 8 | 8 | 0 |
| 3 | 4 / 4 | 4 | 0 |
| 4 | 10 / 10 | 10 | 0 |
| 5 | 3 / 3 | 3 | 0 |

The final five-page read-only pass selected `ppocr-v5-english` throughout on page 1 and reported `en`; pages 2–5 selected `ppocr-v5-latin` throughout and reported `en|latin`. No page reported `mixed`, no Han recognizer was selected, and no output contained the prior mojibake `å­¸`. Recognition quality is still limited by the mobile OCR model and crop quality. Representative imperfect outputs include `W e`, `EEGHV`, `RIAT'S`, `ANRWRERT`, `BROCOET`, digits, and blank strings. This run proves detector execution, association, routing, and handoff plumbing; it does not establish publication-quality transcription.

The dedicated English graph was checked against its installed ONNX metadata: output class count is 438 and `ppocrv5_en_dict.txt` contains 436 entries, matching the two CTC slots. Latin, Korean and Han class counts likewise remain checked against their dictionaries at recognizer initialization. Manga OCR mobile decoding follows the upstream encoder/init/autoregressive-step contract, including bilinear floor-scaled white letterboxing, EOS handling, cache slot writes and `min(cache_len + 1, 127)` position IDs. A real neutral fixture run through explicit `source_language=ja` selected `manga-ocr-mobile`; its clean `こんにちは` region decoded exactly while larger detector crops remained subject to ordinary detection/model error.

The optional Baberu bundle was provisioned from the user-supplied files under `C:\Users\Yozora\AppData\Local\ComicTranslate\models\ocr\baberu-ocr`; the Downloads originals were left untouched. Graph inspection measured `vision_int4.onnx` as `[1,3,224,224] -> [1,256,512]`, prefill logits as `[1,257,14630]` with twelve `[1,2,257,64]` caches, and step logits as `[1,1,14630]` with dynamic `[1,2,total_len,64]` caches. The implementation follows the upstream `genshiai-daichi/baberu-ocr/onnx_infer.py` contract and only enables the route when all five files parse and are present. A fresh five-page run took 146.47 seconds and is recorded in `target/test-artifacts/real-pages-baberu-v5.json`; the distinct comparison is `target/test-artifacts/real-pages-baberu-v5-comparison.json`. Across 39 bubble positions, 35 texts changed and 4 matched `real-pages-english-v2.json`; pages 1-5 selected Baberu for every bubble, reported `en` on page 1 and `en|latin` on pages 2-5, with no Han/Manga recognizers, literal `mixed` source labels, or mojibake. This is a measured transcription change, not a blanket accuracy claim.

The neutral fixture was also run with explicit `source_language=ja` through the real MCP path at `target/test-artifacts/neutral-japanese-baberu-v4.json` (16.55 seconds). All eight unmatched text regions selected Baberu; the exact clean crop remained `text-2: こんにちは`, while larger mixed-script crops showed ordinary model limits. Translation handoff remained pending and preserved the source-image/bbox vision contract.

Final coordinator verification after the last source change: `cargo fmt --check` passed; `cargo test --locked --all-targets` passed 27 tests (16 library plus 11 integration); `cargo test --locked --no-default-features` passed 19 tests (8 library plus 11 integration). Both warnings-denied Clippy sweeps passed. No commit was created.

The next Luna pass also connected real LaMa cleaning. `PageAnalysis.text_lines` now retains the detector's label 1/2 regions so automatic cleaning masks text lines rather than whole label-0 bubble boxes. Bubble boxes are used only as an explicit compatibility fallback when no line evidence exists. The installed `lama-manga-dynamic.onnx` graph was validated as dynamic float tensors: image `[batch,3,h,w]`, mask `[batch,1,h,w]`, and output `[batch,3,h,w]`. Rust performs RGB/255 input, binary masking, symmetric bottom/right modulo-8 padding, shape/finite validation, unpadding, and masked-only compositing.

Two real page-1 cleaning runs wrote only ignored artifacts under `target/test-artifacts`. The explicit-mask run changed 2,103 of 2,184 masked pixels and zero outside. The automatic analyze-to-mask-to-LaMa run masked 89,254 pixels, changed 85,984, and changed zero pixels outside the mask. Visual inspection confirmed that most caption/dialogue text was cleared while panel art outside masks remained intact; small residual marks remained in one bubble, and broad rectangular text-line masks expose a future quality-refinement gate.

A final real stdio workflow ran page 1 through auto analysis (`en|latin`, target `vi`, 14 bubbles, pending handoff), recorded manual source corrections for weak OCR, supplied Vietnamese strings with combining marks, auto-cleaned from `text_lines`, typeset all 14 regions, and exported an ordered one-page ZIP whose manifest language is `vi`. The original source hash remained unchanged. The ignored workflow report and artifacts are under `target/test-artifacts/headless-workflow`.

Remaining work is quality refinement: stronger OCR transcription, tighter stroke-shaped masks, and better fit/placement for long translations. Browser editor completion is intentionally outside the requested headless product. Project-file target persistence was not added because the current direct struct API would be broken; the target is present in each analysis and translation handoff. If durable project persistence is required next, add a backward-compatible optional `target_language` field with serde default and update export round-trip tests.
