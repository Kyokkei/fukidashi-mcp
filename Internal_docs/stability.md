# Long-run stability contract

The MCP process must remain usable on a 16 GiB RAM host during multi-page work. Throughput is secondary.

## Memory lifecycle

- Only one heavy MCP operation runs at a time through the server semaphore.
- ONNX sessions load lazily.
- The default recycle interval is one completed analyze or clean call. Session recycling also happens after a failed inference attempt.
- A maskless clean may need OCR before LaMa. The OCR pipeline is dropped before LaMa is created unless `FUKIDASHI_RETAIN_STAGE_SESSIONS=1`.
- `fukidashi_release_models {}` provides an explicit page/chunk boundary.
- A CUDA provider or BFCArena allocation failure during LaMa execution drops that session and retries once on CPU with the already-built mask. OCR is not reloaded. The tool response reports the actual provider and fallback state.
- CUDA arena growth is same-as-requested and defaults to 2048 MiB. This is not a total VRAM cap because CUDA and cuDNN allocate outside the ORT arena.
- Dynamic page dimensions run with ORT memory patterns disabled. cuDNN uses heuristic search and bounded workspace behavior.
- Crop cleaning (`mode: "crop"`) runs bounded, stride-aligned LaMa crops serially. Its adaptive mask is anchored to the original detector seed and expands only within a small neighbourhood; disconnected artwork inside a broad OCR box is excluded. Checkpoint text-line regions below 0.5 confidence are also excluded as likely art/effect false positives.

The tradeoff is repeated model initialization. Set `FUKIDASHI_SESSION_RECYCLE_PAGES` above one only after observing a flat working set on a bounded fixture. Zero means no automatic recycling.

## Checkpoint and conversation lifecycle

The original full analyze response remains the compatibility default. Long runs explicitly request `response_detail: "compact"`. That response contains one translation item per stable region and avoids repeating the full bubble, text-line, unmatched-line, and vision-correction structures. `checkpoint_path` stores the complete `PageAnalysis` atomically for audit and editor construction.

An agent run processes page 2 first when page 1 is a cover. For each page it:

1. Calls analyze with `response_detail: "compact"` and a unique checkpoint path.
2. Translates only the returned items.
3. Cleans and typesets the page.
4. Atomically records source, analysis, clean, mask, rendered, and failure paths in its project manifest.
5. Calls `fukidashi_release_models {}` when automatic recycling is disabled.

Use a fresh agent conversation every 5–10 pages. A resumed chunk reads the project manifest and begins at the first page whose status is not complete. It does not replay old tool responses. A page counts as complete only when its actual checkpoint, clean image, mask, and rendered image exist. Covers use `excluded_cover` and do not count as translation or typesetting failures.

## Validation sequence

Run protocol and pure tests before native inference. For a hardware smoke test, copy one or two fixture pages and all generated outputs to E:, run pages serially, and record the process working set and GPU memory before and after each page. Stop if either value grows monotonically after session recycling. Do not begin with a complete book or a permanent background benchmark.
For crop cleaning, validate a few representative pages first and inspect the emitted grayscale mask before starting a longer run. The mask is the review artifact for confirming that disconnected line art and textures were not selected.

## Human review and export gate

`fukidashi_serve_editor` creates a revisioned review session and returns immediately. The browser stores draft feedback atomically in `review.json` and `review-audit.json`; feedback uses stable page/bubble IDs, pixel coordinates, bounded issue types, and project-local artifact paths. `fukidashi_wait_for_review` waits on a notification rather than polling and does not acquire the heavy inference semaphore. A stale revision, invalid origin, unknown bubble, out-of-bounds bbox, or duplicate submission is rejected.

The editor's `Submit fixes` action wakes the matching waiter with `request_fixes`. The caller repairs only those pages and serves a new revision. `Approve/export` requires every page to be explicitly marked reviewed; the export route rejects any reviewed project whose latest revision is not explicitly approved. Drafts and audit state survive a Claude restart so the caller can serve the editor again and resume the next revision. A draft is never a completed export.
