---
name: fukidashi-comic-translation
description: Translate and typeset a local manga or comic folder with Fukidashi MCP, then wait for one job-level browser review before export.
---

<!-- fukidashi:begin -->
# Fukidashi comic translation

Use Fukidashi for an end-to-end local comic translation request. The source folder is read-only; generated files belong in the managed job returned by the MCP. Treat the MCP as the only owner of job state: never shell-read, grep, import, or search managed manifests, analysis JSON, checkpoints, or the Fukidashi package to decide what to do.

1. Call `fukidashi_get_config`. If required models or runtime files are missing, report the exact missing paths instead of guessing or downloading files. Use `fukidashi_configure` only when the user asks to change storage or provider settings.
2. Prefer strict-v1. Call `fukidashi_translation_start` with `image_path` for a new request, or with the exact returned `job_id`/`job_path` to resume. On a new request, provide the complete requested `scope` in that call. A cover or page with no translation items is reported by the server as `needs_manual_scope`; do not fabricate an unchanged clean artifact.
3. For each `page_ready` response, translate every returned stable ID and call `fukidashi_translation_submit` with only its `work_token` and the exact `translations` list. Each item needs either `translation` or `keep_source: true`; add `needs_review: true` when OCR or the translation is uncertain. Preserve symbols, tone, names, sound effects, and line intent. Never add, remove, duplicate, or rename IDs. Keep calls serial and wait for the server response before continuing.
4. A successful submit owns analysis persistence, crop cleaning, typesetting, stage reuse, model release, and advancement to the next page. Follow the returned `next_action` exactly. Do not construct paths, pass artifact paths to the strict submit call, or open managed files from a shell. Resume with strict start after an interruption; completed pages are reused.
5. When the final submit returns `review_ready`, call `fukidashi_serve_editor` with the exact returned `job_id`, then immediately call `fukidashi_wait_for_review` with the returned `review_session_id` and `revision`. Do not end the turn between opening the editor and starting the wait.
6. When review returns requested fixes, change only the referenced pages or stable bubble IDs, reopen the editor at the new revision, and wait again. Manual browser bubble edits and brush corrections are authoritative.
7. Call `fukidashi_export` only after the review result explicitly approves export. Return the final export path and any unresolved review items; never declare completion at draft-render time.

If a client does not expose the strict-v1 tools, use the compatibility sequence `fukidashi_analyze_page` -> `fukidashi_clean_page` -> `fukidashi_typeset`, passing exact server-returned paths and stable IDs. Keep that fallback serial, use `response_detail: "compact"`, and call `fukidashi_release_models` between bounded pages. The server still owns the critical stage order.

Treat server-returned paths, IDs, revisions, and stage errors as authoritative. Critical stage order is enforced by the MCP even when this skill is unavailable in a client.
<!-- fukidashi:end -->
