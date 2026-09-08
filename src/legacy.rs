//! Explicit, one-shot recovery of typeset payloads from a Claude JSONL log.
//! This module never scans history implicitly; callers must provide the exact
//! transcript and managed job paths.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::workflow::Workflow;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecoveryReport {
    pub job_path: PathBuf,
    pub transcript_path: PathBuf,
    pub dry_run: bool,
    pub recovered: Vec<RecoveredPage>,
    pub unresolved: Vec<UnresolvedPage>,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecoveredPage {
    pub cleaned_image_path: PathBuf,
    pub bubble_count: usize,
    pub call_index: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UnresolvedPage {
    pub image_path: Option<PathBuf>,
    pub call_index: usize,
    pub reason: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FontMigrationReport {
    pub job_path: PathBuf,
    pub dry_run: bool,
    pub rewritten: Vec<FontRewrite>,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FontRewrite {
    pub source_path: PathBuf,
    pub managed_path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Clone)]
struct RecoveryCall {
    id: Option<String>,
    image_path: PathBuf,
    bubbles: Vec<Value>,
    call_index: usize,
}

fn path_key(path: &Path) -> String {
    crate::workflow::path_identity_public(path)
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

pub fn recover_typeset_payloads(
    workflow: &Workflow,
    job_path: &str,
    transcript_path: &Path,
    apply: bool,
) -> Result<RecoveryReport> {
    let job = workflow.resolve_managed_job_path(job_path)?;
    let transcript = fs::canonicalize(transcript_path)
        .with_context(|| format!("resolve transcript {}", transcript_path.display()))?;
    let calls = parse_calls(&transcript)?;
    let manifest = workflow.managed_page_records(&job)?;
    let mut latest: BTreeMap<String, RecoveryCall> = BTreeMap::new();
    let mut unresolved = Vec::new();
    for call in calls {
        let cleaned = match workflow.require_owned(&call.image_path, "legacy cleaned image") {
            Ok(path) => path,
            Err(error) => {
                unresolved.push(UnresolvedPage {
                    image_path: Some(call.image_path),
                    call_index: call.call_index,
                    reason: format!("path is outside the managed job: {error}"),
                });
                continue;
            }
        };
        if workflow.validate_clean_input(&cleaned).is_err() {
            unresolved.push(UnresolvedPage {
                image_path: Some(cleaned),
                call_index: call.call_index,
                reason: "image_path is not a verified clean artifact".into(),
            });
            continue;
        }
        if !manifest.iter().any(|record| {
            record
                .get("cleaned_image")
                .and_then(Value::as_str)
                .is_some_and(|value| {
                    crate::workflow::paths_same_public(Path::new(value), &cleaned).unwrap_or(false)
                })
        }) {
            unresolved.push(UnresolvedPage {
                image_path: Some(cleaned),
                call_index: call.call_index,
                reason: "cleaned artifact is not registered for this job".into(),
            });
            continue;
        }
        if call.bubbles.is_empty() {
            unresolved.push(UnresolvedPage {
                image_path: Some(cleaned),
                call_index: call.call_index,
                reason: "typeset payload contains no bubbles; refusing to import an empty render"
                    .into(),
            });
            continue;
        }
        latest.insert(path_key(&cleaned), call);
    }

    let project_path = job.join("project.json");
    let mut state: Value = if project_path.is_file() {
        serde_json::from_slice(&fs::read(&project_path).context("read project.json")?)?
    } else {
        json!({"schema_version":1,"pages":[]})
    };
    let pages = state
        .get_mut("pages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("project.json must contain a pages array"))?;
    let mut recovered = Vec::new();
    for (key, call) in latest.values().enumerate() {
        let cleaned = workflow.require_owned(&call.image_path, "legacy cleaned image")?;
        let Some(page) = pages.iter_mut().find(|page| {
            page.get("cleaned_image_path")
                .and_then(Value::as_str)
                .and_then(|value| {
                    crate::workflow::paths_same_public(Path::new(value), &cleaned).ok()
                })
                .unwrap_or(false)
        }) else {
            unresolved.push(UnresolvedPage {
                image_path: Some(cleaned),
                call_index: call.call_index,
                reason: "no matching page in project.json".into(),
            });
            continue;
        };
        let mut normalized = normalize_bubbles(&call.bubbles)?;
        if apply {
            materialize_bubble_fonts(workflow, &job, &mut normalized)?;
        }
        page["bubbles"] = Value::Array(normalized);
        recovered.push(RecoveredPage {
            cleaned_image_path: cleaned,
            bubble_count: call.bubbles.len(),
            call_index: call.call_index,
        });
        let _ = key;
    }
    let backup_path = if apply && !recovered.is_empty() {
        if !project_path.is_file() {
            bail!("cannot apply recovery without an existing project.json");
        }
        let backup = next_backup_path(&job)?;
        let revision = state
            .get("state_revision")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        state["state_revision"] = Value::from(revision);
        fs::copy(&project_path, &backup).context("backup project.json")?;
        atomic_json(&project_path, &state)?;
        Some(backup)
    } else {
        None
    };
    Ok(RecoveryReport {
        job_path: job,
        transcript_path: transcript,
        dry_run: !apply,
        recovered,
        unresolved,
        backup_path,
    })
}

pub fn migrate_managed_fonts(
    workflow: &Workflow,
    job_path: &str,
    apply: bool,
) -> Result<FontMigrationReport> {
    let job = workflow.resolve_managed_job_path(job_path)?;
    let project_path = job.join("project.json");
    let mut state: Value = serde_json::from_slice(
        &fs::read(&project_path).with_context(|| format!("read {}", project_path.display()))?,
    )?;
    let mut rewrites = Vec::new();
    let mut mappings = BTreeMap::new();
    let pages = state
        .get_mut("pages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("project.json must contain a pages array"))?;
    for page in pages.iter_mut() {
        let Some(bubbles) = page.get_mut("bubbles").and_then(Value::as_array_mut) else {
            continue;
        };
        for bubble in bubbles.iter_mut() {
            let Some(font) = bubble.get("font_path").and_then(Value::as_str) else {
                continue;
            };
            let source = PathBuf::from(font);
            let source = if source.is_absolute() {
                source
            } else {
                job.join(source)
            };
            let key = workflow
                .require_owned(&source, "managed font")
                .or_else(|_| fs::canonicalize(&source).map_err(anyhow::Error::from))?;
            let identity = path_key(&key);
            let (destination, sha256) = match workflow.font_destination(&job, &key) {
                Ok(value) => value,
                Err(error) => {
                    remove_empty_fonts_dir(&job);
                    return Err(error);
                }
            };
            let relative = destination
                .strip_prefix(&job)
                .map_err(|_| anyhow!("managed font destination escaped job"))?
                .display()
                .to_string();
            if apply {
                let managed = workflow.materialize_font_path(&job, &key)?;
                let _ = managed;
            }
            mappings.insert(identity, (font.to_owned(), relative.clone()));
            if !rewrites
                .iter()
                .any(|item: &FontRewrite| item.source_path == key)
            {
                rewrites.push(FontRewrite {
                    source_path: key,
                    managed_path: destination,
                    sha256,
                });
            }
        }
    }
    if apply && !mappings.is_empty() {
        for page in pages.iter_mut() {
            if let Some(bubbles) = page.get_mut("bubbles").and_then(Value::as_array_mut) {
                for bubble in bubbles.iter_mut() {
                    if let Some(font) = bubble.get("font_path").and_then(Value::as_str)
                        && let Ok(canonical) = fs::canonicalize(if Path::new(font).is_absolute() {
                            PathBuf::from(font)
                        } else {
                            job.join(font)
                        })
                        && let Some((_, relative)) = mappings.get(&path_key(&canonical))
                    {
                        bubble["font_path"] = Value::String(relative.clone());
                    }
                }
            }
        }
        let revision = state
            .get("state_revision")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        state["state_revision"] = Value::from(revision);
        let backup = next_backup_path_with_prefix(&job, "project.json.pre-font-migration")?;
        fs::copy(&project_path, &backup).context("backup project.json")?;
        atomic_json(&project_path, &state)?;
        return Ok(FontMigrationReport {
            job_path: job,
            dry_run: false,
            rewritten: rewrites,
            backup_path: Some(backup),
        });
    }
    Ok(FontMigrationReport {
        job_path: job,
        dry_run: true,
        rewritten: rewrites,
        backup_path: None,
    })
}

fn remove_empty_fonts_dir(job: &Path) {
    let fonts = job.join("fonts");
    let is_empty = fs::read_dir(&fonts)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false);
    if is_empty {
        let _ = fs::remove_dir(&fonts);
    }
}

fn materialize_bubble_fonts(workflow: &Workflow, job: &Path, bubbles: &mut [Value]) -> Result<()> {
    for bubble in bubbles {
        let Some(font) = bubble.get("font_path").and_then(Value::as_str) else {
            continue;
        };
        let source = PathBuf::from(font);
        let source = if source.is_absolute() {
            source
        } else {
            job.join(source)
        };
        let managed = workflow.materialize_font_path(job, &source)?;
        let relative = managed
            .strip_prefix(job)
            .map_err(|_| anyhow!("managed font escaped job"))?
            .display()
            .to_string();
        bubble["font_path"] = Value::String(relative);
    }
    Ok(())
}

fn next_backup_path(job: &Path) -> Result<PathBuf> {
    next_backup_path_with_prefix(job, "project.json.pre-legacy-recovery")
}

fn next_backup_path_with_prefix(job: &Path, prefix: &str) -> Result<PathBuf> {
    let preferred = job.join(format!("{prefix}.bak"));
    if !preferred.exists() {
        return Ok(preferred);
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!("system clock is before Unix epoch: {error}"))?
        .as_secs();
    for attempt in 0..1000_u32 {
        let path = job.join(format!("{prefix}-{timestamp}-{attempt:03}.bak"));
        if !path.exists() {
            return Ok(path);
        }
    }
    bail!("unable to allocate a unique recovery backup path")
}

fn parse_calls(path: &Path) -> Result<Vec<RecoveryCall>> {
    let mut calls = Vec::new();
    let mut successful_ids = HashSet::new();
    let mut failed_ids = HashSet::new();
    for (line_index, line) in fs::read_to_string(path)?.lines().enumerate() {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        collect_calls(
            &value,
            &mut calls,
            &mut successful_ids,
            &mut failed_ids,
            line_index,
        )?;
    }
    calls.retain(|call| {
        call.id
            .as_deref()
            .map(|id| successful_ids.contains(id) && !failed_ids.contains(id))
            .unwrap_or(true)
    });
    Ok(calls)
}

fn collect_calls(
    value: &Value,
    calls: &mut Vec<RecoveryCall>,
    successful_ids: &mut HashSet<String>,
    failed_ids: &mut HashSet<String>,
    line_index: usize,
) -> Result<()> {
    if let Some(object) = value.as_object() {
        if object.get("type").and_then(Value::as_str) == Some("tool_use")
            && object
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.ends_with("fukidashi_typeset"))
            && let Some(input) = object.get("input").and_then(Value::as_object)
            && let (Some(image_path), Some(bubbles)) = (
                input.get("image_path").and_then(Value::as_str),
                input.get("bubbles").and_then(Value::as_array),
            )
        {
            calls.push(RecoveryCall {
                id: object.get("id").and_then(Value::as_str).map(str::to_owned),
                image_path: PathBuf::from(image_path),
                bubbles: bubbles.clone(),
                call_index: line_index,
            });
        }
        if object.get("type").and_then(Value::as_str) == Some("tool_result")
            && let Some(id) = object.get("tool_use_id").and_then(Value::as_str)
        {
            if object.get("is_error").and_then(Value::as_bool) == Some(true) {
                failed_ids.insert(id.to_owned());
            } else {
                successful_ids.insert(id.to_owned());
            }
        }
        for child in object.values() {
            collect_calls(child, calls, successful_ids, failed_ids, line_index)?;
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            collect_calls(child, calls, successful_ids, failed_ids, line_index)?;
        }
    }
    Ok(())
}

fn normalize_bubbles(bubbles: &[Value]) -> Result<Vec<Value>> {
    bubbles
        .iter()
        .map(|bubble| {
            let mut object = bubble
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("legacy bubble must be an object"))?;
            if object.get("translation").and_then(Value::as_str).is_none()
                && let Some(text) = object.get("text").cloned()
            {
                object.insert("translation".into(), text);
            }
            Ok(Value::Object(object))
        })
        .collect()
}

fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("project has no parent"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map(|_| ())
        .map_err(|error| anyhow!("promote project backup: {}", error.error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Rgb, RgbImage, Rgba, RgbaImage};
    use tempfile::tempdir;

    #[test]
    fn recovery_is_dry_run_safe_last_call_wins_and_preserves_edits() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        RgbaImage::from_pixel(8, 8, Rgba([10, 10, 10, 255]))
            .save(&source)
            .unwrap();
        let jobs = dir.path().join("jobs");
        let workflow = Workflow::new(jobs).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        let cleaned = RgbImage::from_pixel(8, 8, Rgb([255, 255, 255]));
        let mut mask = GrayImage::new(8, 8);
        mask.put_pixel(2, 2, image::Luma([255]));
        let (cleaned_path, _, _) = workflow
            .write_clean_artifact(&source, &cleaned, &mask, 3, "crop")
            .unwrap();
        let rendered = registration.job_dir.join("pages/0001/rendered.png");
        cleaned.save(&rendered).unwrap();
        let clean = workflow.validate_clean_input(&cleaned_path).unwrap();
        workflow
            .register_render(&rendered, &clean, Value::Null, json!({}))
            .unwrap();
        let project = registration.job_dir.join("project.json");
        let state = json!({"schema_version":1,"pages":[{
            "id":"page-1","image_path":source,"cleaned_image_path":cleaned_path,
            "rendered_image_path":rendered,"bubbles":[],
            "issues":[{"bbox":{"x1":1,"y1":1,"x2":2,"y2":2}}],
            "correction_strokes":[{"mode":"cover","size":4,"points":[{"x":3,"y":3}]}]
        }]});
        fs::write(&project, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        let transcript = dir.path().join("session.jsonl");
        let first = json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"typeset-first","name":"mcp__fukidashi__fukidashi_typeset","input":{
            "image_path":cleaned_path,"bubbles":[{"id":"b1","bbox":{"x1":1,"y1":1,"x2":4,"y2":4},"text":"old"}]
        }}]}});
        let first_result = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"typeset-first","is_error":false,"content":[{"type":"text","text":"rendered"}]}]}});
        let second = json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"typeset-second","name":"mcp__fukidashi__fukidashi_typeset","input":{
            "image_path":cleaned_path,"bubbles":[{"id":"b1","bbox":{"x1":1,"y1":1,"x2":4,"y2":4},"text":"latest"}]
        }}]}});
        let second_result = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"typeset-second","is_error":false,"content":[{"type":"text","text":"rendered"}]}]}});
        let outside = json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"typeset-outside","name":"mcp__fukidashi__fukidashi_typeset","input":{
            "image_path":source,"bubbles":[]
        }}]}});
        let outside_result = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"typeset-outside","is_error":false,"content":[{"type":"text","text":"rendered"}]}]}});
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n{}\n{}\n{}\n{}\n",
                first, first_result, second, second_result, outside, outside_result
            ),
        )
        .unwrap();
        let before = fs::read(&project).unwrap();
        let report = recover_typeset_payloads(
            &workflow,
            &registration.job_dir.to_string_lossy(),
            &transcript,
            false,
        )
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.recovered.len(), 1);
        assert_eq!(report.recovered[0].bubble_count, 1);
        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(fs::read(&project).unwrap(), before);

        let applied = recover_typeset_payloads(
            &workflow,
            &registration.job_dir.to_string_lossy(),
            &transcript,
            true,
        )
        .unwrap();
        assert!(applied.backup_path.unwrap().is_file());
        let saved: Value = serde_json::from_slice(&fs::read(&project).unwrap()).unwrap();
        assert_eq!(saved["state_revision"], 1);
        assert_eq!(saved["pages"][0]["bubbles"][0]["translation"], "latest");
        assert_eq!(saved["pages"][0]["issues"].as_array().unwrap().len(), 1);
        assert_eq!(
            saved["pages"][0]["correction_strokes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn font_migration_materializes_multiple_fonts_without_changing_edits() {
        let arial = Path::new(r"C:\Windows\Fonts\arial.ttf");
        let emoji = Path::new(r"C:\Windows\Fonts\seguiemj.ttf");
        if !arial.is_file() || !emoji.is_file() {
            return;
        }
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        RgbaImage::from_pixel(8, 8, Rgba([10, 10, 10, 255]))
            .save(&source)
            .unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        let clean = RgbImage::from_pixel(8, 8, Rgb([255, 255, 255]));
        let mut mask = GrayImage::new(8, 8);
        mask.put_pixel(2, 2, image::Luma([255]));
        let (cleaned, _, _) = workflow
            .write_clean_artifact(&source, &clean, &mask, 1, "test")
            .unwrap();
        let manual = "DÙ MỘT BÀ CHỊ\nNHƯ TÔI ĐẤY\nĐỤ BẠN GÁI\nCẬU ĐI CHĂNG NỮA";
        let state = json!({
            "schema_version": 1,
            "state_revision": 10,
            "pages": [{
                "id": "page-1",
                "image_path": source,
                "cleaned_image_path": cleaned,
                "bubbles": [
                    {"id":"b1","text":"LÀM","translation":manual,"bbox":{"x1":1,"y1":1,"x2":4,"y2":4},"font_path":arial},
                    {"id":"b2","text":"❤","translation":"❤","bbox":{"x1":4,"y1":1,"x2":7,"y2":4},"font_path":emoji}
                ],
                "issues": [{"type":"wrong_translation","bbox":{"x1":1,"y1":1,"x2":2,"y2":2}}],
                "correction_strokes": [{"mode":"cover","size":4,"points":[{"x":3,"y":3}]}]
            }]
        });
        let project = registration.job_dir.join("project.json");
        fs::write(&project, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        let review = registration.job_dir.join("review.json");
        fs::write(&review, br#"{"status":"awaiting_review","action":null}"#).unwrap();
        let dry = migrate_managed_fonts(&workflow, &registration.job_dir.to_string_lossy(), false)
            .unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.rewritten.len(), 2);
        assert!(
            dry.rewritten
                .iter()
                .all(|item| !item.managed_path.is_file())
        );
        let applied =
            migrate_managed_fonts(&workflow, &registration.job_dir.to_string_lossy(), true)
                .unwrap();
        assert!(!applied.dry_run);
        assert_eq!(applied.rewritten.len(), 2);
        assert!(
            applied
                .backup_path
                .as_ref()
                .is_some_and(|path| path.is_file())
        );
        let saved: Value = serde_json::from_slice(&fs::read(&project).unwrap()).unwrap();
        assert_eq!(saved["state_revision"], 11);
        assert_eq!(saved["pages"][0]["bubbles"][0]["translation"], manual);
        assert_eq!(saved["pages"][0]["bubbles"][0]["id"], "b1");
        assert_eq!(saved["pages"][0]["bubbles"][1]["id"], "b2");
        assert_eq!(saved["pages"][0]["issues"].as_array().unwrap().len(), 1);
        assert_eq!(
            saved["pages"][0]["correction_strokes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for bubble in saved["pages"][0]["bubbles"].as_array().unwrap() {
            let path = bubble["font_path"].as_str().unwrap();
            assert!(path.starts_with("fonts\\") || path.starts_with("fonts/"));
            assert!(registration.job_dir.join(path).is_file());
        }
        assert_eq!(
            fs::read(&review).unwrap(),
            br#"{"status":"awaiting_review","action":null}"#
        );
    }

    #[cfg(windows)]
    #[test]
    fn font_migration_rejects_unapproved_external_file() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        RgbaImage::from_pixel(8, 8, Rgba([10, 10, 10, 255]))
            .save(&source)
            .unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        let outside = dir.path().join("not-a-font.ttf");
        fs::write(&outside, b"fixture").unwrap();
        let project = registration.job_dir.join("project.json");
        let state =
            json!({"pages":[{"cleaned_image_path":source,"bubbles":[{"font_path":outside}]}]});
        fs::write(&project, serde_json::to_vec(&state).unwrap()).unwrap();
        let error = migrate_managed_fonts(&workflow, &registration.job_dir.to_string_lossy(), true)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside approved font provenance")
                || error.to_string().contains("supported")
        );
        assert!(!registration.job_dir.join("fonts").exists());
    }
}
