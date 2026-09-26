//! Durable, bounded approval pipeline.
//!
//! Rendering and packaging are separate phases. An approval freezes one
//! semantic snapshot, partitions its dirty pages deterministically, checkpoints
//! every completed page atomically, and assembles exactly one ordered archive
//! from validated cached artifacts without rerendering them.
//!
//! Snapshot identity (Blocker 1)
//! ------------------------------
//! The frozen identity is `(job_id, revision, snapshot_signature)` where the
//! signature hashes ONLY the canonical semantic state. The dirty plan is
//! operational data stored INSIDE the checkpoint, never part of the identity:
//! after a crash the recomputed dirty set legitimately shrinks (rendered pages
//! are no longer dirty), and the restart must reattach to the SAME checkpoint
//! and retain the ORIGINAL 60/60/22 plan.
//!
//! Input provenance (Blockers 2 and 3)
//! -----------------------------------
//! Every checkpoint entry binds the output to immutable inputs:
//!
//! - `semantic_render_signature`: canonical JSON of the frozen page inputs.
//! - `source_sha256` / `clean_sha256`: hashes of the managed source and clean
//!   artifacts the render was built from (empty only for legacy ad-hoc jobs
//!   without managed stages; managed entries always carry both).
//! - `input_hash`: ONE composite hash, deterministically built by
//!   [`composite_input_hash`] from `page_id + source_sha256 + clean_sha256 +
//!   semantic_render_signature`, joined with NUL separators and hashed with
//!   SHA-256. It is not a second copy of the semantic signature.
//! - `output_sha256`: SHA-256 of the committed rendered bytes.
//!
//! Crash-gap recovery promotes a live artifact to `complete` only when the
//! render sidecar itself is valid AND the sidecar-recorded semantic signature
//! and source/clean hashes all equal the frozen expectation. Internal sidecar
//! validity alone never promotes an old render.
//!
//! Single writer (Blockers 4 and 5)
//! --------------------------------
//! There is exactly ONE writer-exclusion mechanism: the existing workflow
//! render lock (`.fukidashi-render.lock`) with its heartbeat thread and
//! PID-liveness recovery. [`ApprovalLease`] holds that same file lease across
//! the whole approval pipeline; per-page renders on the worker thread attach
//! to it reentrantly instead of reacquiring it. A second approval either fails
//! fast with a busy error or waits on the real lock; a live long-running job
//! is never stolen merely because its lock file is old.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Upper bound for one render batch. A 142-page dirty plan partitions as
/// 60 / 60 / 22 and a restart keeps that original shape.
pub const MAX_APPROVAL_BATCH_PAGES: usize = 60;
const CHECKPOINT_FORMAT_VERSION: u32 = 2;

fn process_leases() -> &'static Mutex<HashMap<String, u64>> {
    static LEASES: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Deterministically partition dirty pages into batches of at most 60,
/// preserving the caller's page order.
pub fn partition_approval_batches(dirty: &[usize]) -> Vec<Vec<usize>> {
    if dirty.is_empty() {
        return Vec::new();
    }
    dirty
        .chunks(MAX_APPROVAL_BATCH_PAGES)
        .map(|chunk| chunk.to_vec())
        .collect()
}

/// Canonical snapshot signature: sha256 hex of a canonical JSON document.
pub fn snapshot_signature(value: &serde_json::Value) -> String {
    let canonical = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical);
    format!("{:x}", hasher.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Compare serialized semantic render signatures by their JSON value.
///
/// A signature is stored as a JSON string inside a sidecar. JSON numbers can
/// have different textual spellings while decoding to the same value, so raw
/// string comparison can reject an artifact whose frozen page semantics are
/// unchanged. Keep non-JSON legacy signatures comparable by exact string.
pub fn semantic_render_signatures_equal(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (
        serde_json::from_str::<serde_json::Value>(left),
        serde_json::from_str::<serde_json::Value>(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Build the ONE composite input hash for a checkpoint entry.
///
/// `input_hash = sha256_hex(page_id + "\0" + source_sha256 + "\0" +
/// clean_sha256 + "\0" + semantic_render_signature + "\0")`.
///
/// Every component is load-bearing: the stable page association, the immutable
/// managed input artifacts, and the frozen semantic request. Managed entries
/// always carry real source/clean hashes; ad-hoc entries without managed
/// stages use empty strings, which still binds the entry to its page and
/// semantic signature.
pub fn composite_input_hash(
    page_id: &str,
    source_sha256: &str,
    clean_sha256: &str,
    semantic_render_signature: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(page_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(source_sha256.as_bytes());
    hasher.update(b"\0");
    hasher.update(clean_sha256.as_bytes());
    hasher.update(b"\0");
    hasher.update(semantic_render_signature.as_bytes());
    hasher.update(b"\0");
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSnapshot {
    pub job_id: String,
    pub revision: u64,
    /// Identity hash over `(job_id, revision, semantic state)` ONLY. The
    /// dirty plan is deliberately excluded so a restart that recomputes a
    /// smaller dirty set still matches the frozen operation.
    pub snapshot_signature: String,
    /// Hex of the canonical semantic state document (debugging/audit).
    #[serde(default)]
    pub state_signature: String,
    /// Operational plan, frozen at first run. NOT part of the identity.
    pub dirty_plan: Vec<usize>,
    /// Deterministic [`MAX_APPROVAL_BATCH_PAGES`]-bounded batches. NOT part of
    /// the identity.
    pub batches: Vec<Vec<usize>>,
}

impl ApprovalSnapshot {
    pub fn freeze(
        job_id: String,
        revision: u64,
        state_signature_value: &serde_json::Value,
        dirty_plan: Vec<usize>,
    ) -> Self {
        let batches = partition_approval_batches(&dirty_plan);
        let state_signature = snapshot_signature(state_signature_value);
        // NOTE: `dirty_plan` is intentionally NOT hashed here (Blocker 1).
        let snapshot_signature = snapshot_signature(&serde_json::json!({
            "job_id": job_id,
            "revision": revision,
            "state": state_signature_value,
        }));
        Self {
            job_id,
            revision,
            snapshot_signature,
            state_signature,
            dirty_plan,
            batches,
        }
    }

    pub fn batch_count(&self) -> usize {
        self.batches.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPageCheckpoint {
    pub page_index: usize,
    pub page_id: String,
    /// Canonical semantic render signature (JSON string of the frozen page
    /// inputs, e.g. `editor_page_render_signature`).
    pub semantic_render_signature: String,
    /// Hashes of the immutable managed input artifacts the render was built
    /// from. Empty only for legacy ad-hoc jobs without managed stages.
    pub source_sha256: String,
    pub clean_sha256: String,
    /// [`composite_input_hash`] over the fields above. See its docs for the
    /// exact construction.
    pub input_hash: String,
    pub output_path: String,
    pub output_sha256: String,
    pub status: String,
}

impl ApprovalPageCheckpoint {
    pub fn new(
        page_index: usize,
        page_id: String,
        semantic_render_signature: String,
        source_sha256: String,
        clean_sha256: String,
        output_path: String,
        output_sha256: String,
    ) -> Self {
        let input_hash = composite_input_hash(
            &page_id,
            &source_sha256,
            &clean_sha256,
            &semantic_render_signature,
        );
        Self {
            page_index,
            page_id,
            semantic_render_signature,
            source_sha256,
            clean_sha256,
            input_hash,
            output_path,
            output_sha256,
            status: "complete".to_owned(),
        }
    }
}

/// Frozen per-page expectation derived from the approval snapshot state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedPageProvenance {
    pub page_index: usize,
    pub page_id: String,
    pub semantic_render_signature: String,
    pub source_sha256: String,
    pub clean_sha256: String,
    pub output_path: PathBuf,
}

/// Live artifact evidence gathered by the caller (render sidecar validity,
/// sidecar-recorded semantic signature and input hashes, freshly hashed
/// output bytes). Pure data: this module performs no I/O here so the rules
/// are unit-testable without a job tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveArtifactEvidence {
    pub sidecar_valid: bool,
    /// Semantic signature recorded INSIDE the render sidecar at commit time
    /// (`qa.semantic_render_signature`). `None` for legacy sidecars, which
    /// must rerender.
    pub sidecar_semantic_signature: Option<String>,
    pub source_sha256: String,
    pub clean_sha256: String,
    /// Fresh SHA-256 of the live output file. `None` when the file is missing
    /// or unreadable.
    pub output_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalCheckpoint {
    pub format_version: u32,
    pub job_id: String,
    pub revision: u64,
    pub snapshot_signature: String,
    /// Original dirty plan, frozen at first run. Restarts retain this even
    /// when the recomputed dirty set is smaller.
    pub dirty_plan: Vec<usize>,
    pub batches: Vec<Vec<usize>>,
    pub pages: BTreeMap<String, ApprovalPageCheckpoint>,
}

impl ApprovalCheckpoint {
    pub fn fresh(snapshot: &ApprovalSnapshot) -> Self {
        Self {
            format_version: CHECKPOINT_FORMAT_VERSION,
            job_id: snapshot.job_id.clone(),
            revision: snapshot.revision,
            snapshot_signature: snapshot.snapshot_signature.clone(),
            dirty_plan: snapshot.dirty_plan.clone(),
            batches: snapshot.batches.clone(),
            pages: BTreeMap::new(),
        }
    }

    /// Semantic identity only: job, revision, signature, format. The dirty
    /// plan and batches never participate (Blocker 1).
    pub fn matches_snapshot(&self, snapshot: &ApprovalSnapshot) -> bool {
        self.format_version == CHECKPOINT_FORMAT_VERSION
            && self.job_id == snapshot.job_id
            && self.revision == snapshot.revision
            && self.snapshot_signature == snapshot.snapshot_signature
    }

    pub fn mark_complete(&mut self, page: ApprovalPageCheckpoint) {
        self.pages.insert(page.page_index.to_string(), page);
    }

    /// Merge newly dirty pages discovered on restart into the retained plan.
    ///
    /// The original plan remains the prefix and its existing batches remain
    /// intact. Pages that were clean when the checkpoint was created but have
    /// since lost their cache entry are appended in the freshly computed page
    /// order, filling the final batch before creating more batches. This keeps
    /// resume deterministic while ensuring a newly invalid page can never be
    /// silently omitted.
    fn merge_new_dirty_pages(&mut self, fresh_dirty: &[usize]) -> Vec<usize> {
        if self.dirty_plan.is_empty() && self.batches.is_empty() {
            self.dirty_plan = fresh_dirty.to_vec();
            self.batches = partition_approval_batches(fresh_dirty);
            return self.dirty_plan.clone();
        }

        let mut newly_added = Vec::new();
        for &index in fresh_dirty {
            if !self.dirty_plan.contains(&index) {
                self.dirty_plan.push(index);
                newly_added.push(index);
            }
        }
        if !self.batches.is_empty() {
            for index in newly_added {
                if self
                    .batches
                    .last()
                    .is_some_and(|last| last.len() < MAX_APPROVAL_BATCH_PAGES)
                {
                    self.batches.last_mut().expect("batch exists").push(index);
                } else {
                    self.batches.push(vec![index]);
                }
            }
        } else {
            self.batches = partition_approval_batches(&self.dirty_plan);
        }
        self.dirty_plan.clone()
    }

    /// The operative dirty plan: the ORIGINAL frozen plan when this checkpoint
    /// belongs to the snapshot, augmented with newly dirty pages discovered on
    /// restart. Old checkpoints without a stored plan fall back to flattened
    /// batches.
    pub fn effective_dirty_plan(&self, snapshot: &ApprovalSnapshot) -> Vec<usize> {
        if !self.matches_snapshot(snapshot) {
            return snapshot.dirty_plan.clone();
        }
        if self.dirty_plan.is_empty() {
            return self.batches.iter().flatten().copied().collect();
        }
        self.dirty_plan.clone()
    }

    fn entry_structurally_complete(&self, index: usize) -> bool {
        self.pages
            .get(&index.to_string())
            .is_some_and(|page| page.status == "complete" && page.page_index == index)
    }

    pub fn completed_for_snapshot(&self, snapshot: &ApprovalSnapshot) -> Vec<usize> {
        if !self.matches_snapshot(snapshot) {
            return Vec::new();
        }
        self.effective_dirty_plan(snapshot)
            .into_iter()
            .filter(|index| self.entry_structurally_complete(*index))
            .collect()
    }

    /// Pages still requiring render work: effective-plan pages without a
    /// structurally complete checkpoint entry. Content validation (hashes,
    /// sidecars) happens at the call site before reuse.
    pub fn remaining(&self, snapshot: &ApprovalSnapshot) -> Vec<usize> {
        if !self.matches_snapshot(snapshot) {
            return snapshot.dirty_plan.clone();
        }
        self.effective_dirty_plan(snapshot)
            .into_iter()
            .filter(|index| !self.entry_structurally_complete(*index))
            .collect()
    }
}

/// Production restart path. Loads the persisted checkpoint for this revision
/// and reattaches to the SAME operation when the semantic identity matches,
/// retaining the original dirty plan and batches. A non-matching (or absent)
/// checkpoint starts a fresh operation. Callers MUST route every restart
/// through here instead of comparing freshly recomputed dirty sets.
pub fn prepare_resume(
    job_dir: &Path,
    revision: u64,
    snapshot: &ApprovalSnapshot,
) -> (ApprovalCheckpoint, Vec<usize>) {
    match load_approval_checkpoint(job_dir, revision) {
        Some(mut cached) if cached.matches_snapshot(snapshot) => {
            let plan = cached.merge_new_dirty_pages(&snapshot.dirty_plan);
            (cached, plan)
        }
        _ => {
            let plan = snapshot.dirty_plan.clone();
            (ApprovalCheckpoint::fresh(snapshot), plan)
        }
    }
}

pub fn checkpoint_path(job_dir: &Path, revision: u64) -> PathBuf {
    job_dir.join(format!(".fukidashi-approval-{revision}.checkpoint.json"))
}

pub fn load_approval_checkpoint(job_dir: &Path, revision: u64) -> Option<ApprovalCheckpoint> {
    let bytes = fs::read(checkpoint_path(job_dir, revision)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Atomically persist a checkpoint: temp file in the job dir, fsync, rename.
/// Callers MUST propagate errors (`?`): a failed checkpoint write fails the
/// approval operation cleanly without pretending persistence succeeded. Page
/// artifacts already committed stay on disk for crash-gap recovery.
pub fn save_approval_checkpoint(
    job_dir: &Path,
    revision: u64,
    checkpoint: &ApprovalCheckpoint,
) -> Result<()> {
    let path = checkpoint_path(job_dir, revision);
    let bytes = serde_json::to_vec_pretty(checkpoint)?;
    let tmp = tempfile::NamedTempFile::new_in(job_dir).context("create approval checkpoint")?;
    fs::write(tmp.path(), &bytes).context("write approval checkpoint")?;
    // Sync through the temp file's own writable handle: reopening read-only
    // fails to sync on Windows (os error 5).
    tmp.as_file()
        .sync_all()
        .context("sync approval checkpoint")?;
    tmp.persist(&path)
        .map_err(|error| anyhow!("promote approval checkpoint: {}", error.error))?;
    Ok(())
}

/// Validate a structurally complete checkpoint entry against the frozen
/// expectation and freshly hashed live output. Never trusts
/// `status == complete` alone: page association, semantic signature, input
/// artifact hashes, composite input hash, output path, and live output bytes
/// must ALL agree (Blockers 2 and 3).
pub fn checkpoint_entry_valid(
    entry: &ApprovalPageCheckpoint,
    expected: &ExpectedPageProvenance,
    live_output_sha256: Option<&str>,
) -> bool {
    if entry.status != "complete" {
        return false;
    }
    if entry.page_index != expected.page_index {
        return false;
    }
    if entry.page_id != expected.page_id {
        return false;
    }
    if !semantic_render_signatures_equal(
        &entry.semantic_render_signature,
        &expected.semantic_render_signature,
    ) {
        return false;
    }
    if entry.source_sha256 != expected.source_sha256 || entry.clean_sha256 != expected.clean_sha256
    {
        return false;
    }
    if entry.input_hash
        != composite_input_hash(
            &expected.page_id,
            &expected.source_sha256,
            &expected.clean_sha256,
            &entry.semantic_render_signature,
        )
    {
        return false;
    }
    if Path::new(&entry.output_path) != expected.output_path {
        return false;
    }
    match (live_output_sha256, entry.output_sha256.as_str()) {
        (Some(live), recorded) => live == recorded,
        _ => false,
    }
}

/// Crash-gap recovery: the render artifact and sidecar committed but the
/// process died before the checkpoint did. Promotes the live artifact WITHOUT
/// rerendering only when the sidecar is valid AND the sidecar-recorded
/// semantic signature and input hashes all equal the frozen expectation AND
/// the live output bytes hash successfully. An old render for a superseded
/// translation (valid sidecar, wrong signature) returns `None` and the page
/// rerenders (Blocker 2).
pub fn recover_verified_artifact(
    expected: &ExpectedPageProvenance,
    live: &LiveArtifactEvidence,
) -> Option<ApprovalPageCheckpoint> {
    if !live.sidecar_valid {
        return None;
    }
    if !live
        .sidecar_semantic_signature
        .as_deref()
        .is_some_and(|signature| {
            semantic_render_signatures_equal(signature, &expected.semantic_render_signature)
        })
    {
        return None;
    }
    if live.source_sha256 != expected.source_sha256 || live.clean_sha256 != expected.clean_sha256 {
        return None;
    }
    let output_sha256 = live.output_sha256.clone()?;
    Some(ApprovalPageCheckpoint::new(
        expected.page_index,
        expected.page_id.clone(),
        expected.semantic_render_signature.clone(),
        expected.source_sha256.clone(),
        expected.clean_sha256.clone(),
        expected.output_path.display().to_string(),
        output_sha256,
    ))
}

/// One approval/export writer per job, backed by the EXISTING workflow render
/// lock file with its heartbeat thread and PID-liveness recovery. The lease
/// is operation-wide: per-page renders on the worker thread attach to it
/// reentrantly and must not reacquire it.
///
/// A second approval for the same job fails fast with a busy error when the
/// owner is demonstrably live; a demonstrably dead owner's stale file is
/// reclaimed by the lock infrastructure. A live long-running job is never
/// stolen merely because its lock file is old (Blockers 4 and 5).
pub struct ApprovalLease {
    key: String,
    _render: crate::workflow::RenderLock,
}

impl Drop for ApprovalLease {
    fn drop(&mut self) {
        if let Ok(mut leases) = process_leases().lock() {
            leases.remove(&self.key);
        }
    }
}

impl ApprovalLease {
    pub fn try_acquire(job_dir: &Path, revision: u64) -> Result<Self> {
        let key = format!("{}#{revision}", job_dir.display());
        {
            let mut leases = process_leases()
                .lock()
                .map_err(|_| anyhow!("approval lease registry poisoned"))?;
            if leases.contains_key(&key) {
                bail!("approval already running for revision {revision}");
            }
            leases.insert(key.clone(), std::process::id() as u64);
        }
        let acquired = (|| {
            let managed =
                job_dir.join("job.json").is_file() || job_dir.join(".fukidashi-job.json").is_file();
            // The approval lease is a fast busy path. Use the nonblocking
            // render lock so a browser/native review request never waits ten
            // seconds behind an active export.
            let lock_path = job_dir.join(".fukidashi-render.lock");
            if managed {
                let jobs_root = job_dir
                    .parent()
                    .ok_or_else(|| anyhow!("approval job has no jobs root"))?
                    .to_path_buf();
                let workflow = crate::workflow::Workflow::new(jobs_root)?;
                workflow.try_acquire_render_lock(job_dir)
            } else {
                // Ad-hoc editor jobs have no managed manifest, so they use
                // the same lock file, heartbeat, and liveness rule directly.
                crate::workflow::RenderLock::try_acquire_for_path(
                    lock_path,
                    &format!("approval job {}", job_dir.display()),
                )
            }
        })();
        match acquired {
            Ok(render) => Ok(Self {
                key,
                _render: render,
            }),
            Err(error) => {
                if let Ok(mut leases) = process_leases().lock() {
                    leases.remove(&key);
                }
                Err(error)
            }
        }
    }
}

/// Cancellation: stop before the next page, retain valid completed work, and
/// leave the checkpoint resumable. The current page runs to a safe boundary.
pub fn cancellation_requested(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::SeqCst)
}

pub fn batch_progress_message(
    batch_index: usize,
    batch_count: usize,
    page_in_batch: usize,
    batch_len: usize,
) -> String {
    format!(
        "Rendering batch {}/{}, page {}/{}",
        batch_index + 1,
        batch_count,
        page_in_batch + 1,
        batch_len
    )
}

// ---------------------------------------------------------------------------
// Single authoritative file-based archive path.
// ---------------------------------------------------------------------------

/// One archive entry for the shared packaging core.
pub enum ArchiveEntry<'a> {
    /// Stream bytes directly from this file; the whole archive is never held
    /// in RAM as decoded pages plus a `Vec<u8>` at once.
    File {
        name: String,
        method: zip::CompressionMethod,
        path: &'a Path,
    },
    Bytes {
        name: String,
        method: zip::CompressionMethod,
        bytes: &'a [u8],
    },
}

/// Write `entries` in order into a `ZipWriter` over any `Write + Seek`
/// target. Production passes a temp file; small callers and tests may pass a
/// `Cursor<Vec<u8>>`. One implementation, no duplicate assemblers.
pub fn write_archive_entries<W: Write + Seek>(
    writer: W,
    entries: &[ArchiveEntry<'_>],
) -> Result<W> {
    let mut zip = zip::ZipWriter::new(writer);
    for entry in entries {
        match entry {
            ArchiveEntry::File { name, method, path } => {
                let options = zip::write::SimpleFileOptions::default().compression_method(*method);
                zip.start_file(name, options)?;
                let mut source = fs::File::open(path)
                    .with_context(|| format!("read archive input {}", path.display()))?;
                std::io::copy(&mut source, &mut zip)?;
            }
            ArchiveEntry::Bytes {
                name,
                method,
                bytes,
            } => {
                let options = zip::write::SimpleFileOptions::default().compression_method(*method);
                zip.start_file(name, options)?;
                zip.write_all(bytes)?;
            }
        }
    }
    let writer = zip.finish()?;
    Ok(writer)
}

/// Write entries to `tmp_path` (caller-owned temp file inside the destination
/// directory) and fsync. The caller renames into place; nothing partial is
/// ever published and the render cache/checkpoint are untouched by packaging
/// failures.
pub fn write_archive_to_path(tmp_path: &Path, entries: &[ArchiveEntry<'_>]) -> Result<()> {
    let file = fs::File::create(tmp_path)
        .with_context(|| format!("create archive temp {}", tmp_path.display()))?;
    let file = write_archive_entries(file, entries)?;
    file.sync_all().context("sync final archive")?;
    Ok(())
}

/// Assemble ONE ordered archive from validated cached page artifacts.
/// Streams each file from disk; never concatenates ZIPs; never rerenders.
/// Publishes atomically only after every entry validates; a packaging failure
/// retains the render cache/checkpoint and publishes nothing partial.
pub fn assemble_ordered_zip(
    ordered_inputs: &[PathBuf],
    output_final: &Path,
    entry_prefix: &str,
) -> Result<()> {
    if ordered_inputs.is_empty() {
        bail!("no cached pages to package");
    }
    let mut seen = std::collections::HashSet::new();
    for input in ordered_inputs {
        if !seen.insert(input.clone()) {
            bail!("duplicate cached page {}", input.display());
        }
        if !input.is_file() {
            bail!("cached page is missing: {}", input.display());
        }
    }
    let output_dir = output_final
        .parent()
        .ok_or_else(|| anyhow!("export has no parent"))?;
    fs::create_dir_all(output_dir).context("create export directory")?;
    let tmp = tempfile::NamedTempFile::new_in(output_dir).context("create atomic archive")?;
    let entries: Vec<ArchiveEntry<'_>> = ordered_inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let ext = input
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("bin")
                .to_ascii_lowercase();
            ArchiveEntry::File {
                name: format!("{entry_prefix}{index:04}.{ext}"),
                method: zip::CompressionMethod::Deflated,
                path: input,
            }
        })
        .collect();
    write_archive_to_path(tmp.path(), &entries)?;
    fs::rename(tmp.path(), output_final).context("publish final archive")?;
    // Persisting via rename consumes the temp handle; keep the guard from
    // deleting the published file on some platforms by forgetting it.
    std::mem::forget(tmp);
    Ok(())
}

/// Approval audit binding: the approval records the exact frozen snapshot it
/// packaged, so "some review was approved" can never authorize arbitrary
/// current artifacts. The recorded dirty plan is the EFFECTIVE plan retained
/// from the checkpoint (original 60/60/22 shape on resumes), not the freshly
/// recomputed smaller set.
pub fn approval_audit_value(
    snapshot: &ApprovalSnapshot,
    effective_dirty_plan: &[usize],
    effective_batches: &[Vec<usize>],
    page_count: usize,
    reused_pages: usize,
) -> serde_json::Value {
    serde_json::json!({
        "job_id": snapshot.job_id,
        "snapshot_signature": snapshot.snapshot_signature,
        "state_signature": snapshot.state_signature,
        "revision": snapshot.revision,
        "dirty_pages": effective_dirty_plan,
        "batches": effective_batches,
        "packaged_pages": page_count,
        "reused_pages": reused_pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::sync::atomic::AtomicUsize;
    use tempfile::tempdir;

    fn provenance(index: usize, sig: &str) -> ExpectedPageProvenance {
        ExpectedPageProvenance {
            page_index: index,
            page_id: format!("page-{index}"),
            semantic_render_signature: sig.to_owned(),
            source_sha256: format!("source-{index}"),
            clean_sha256: format!("clean-{index}"),
            output_path: PathBuf::from(format!("/tmp/page-{index}.png")),
        }
    }

    fn live_for(expected: &ExpectedPageProvenance, output: &str) -> LiveArtifactEvidence {
        LiveArtifactEvidence {
            sidecar_valid: true,
            sidecar_semantic_signature: Some(expected.semantic_render_signature.clone()),
            source_sha256: expected.source_sha256.clone(),
            clean_sha256: expected.clean_sha256.clone(),
            output_sha256: Some(output.to_owned()),
        }
    }

    fn complete_entry(expected: &ExpectedPageProvenance, output: &str) -> ApprovalPageCheckpoint {
        ApprovalPageCheckpoint::new(
            expected.page_index,
            expected.page_id.clone(),
            expected.semantic_render_signature.clone(),
            expected.source_sha256.clone(),
            expected.clean_sha256.clone(),
            expected.output_path.display().to_string(),
            output.to_owned(),
        )
    }

    #[test]
    fn dirty_142_partitions_60_60_22() {
        let dirty: Vec<usize> = (0..142).collect();
        let batches = partition_approval_batches(&dirty);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 60);
        assert_eq!(batches[1].len(), 60);
        assert_eq!(batches[2].len(), 22);
        let round_trip: Vec<usize> = batches.into_iter().flatten().collect();
        assert_eq!(round_trip, dirty);
    }

    #[test]
    fn snapshot_identity_excludes_dirty_plan() {
        let state = serde_json::json!({"pages": [1, 2, 3]});
        let full = ApprovalSnapshot::freeze("job".into(), 7, &state, (0..142).collect());
        // Restart recomputes a smaller dirty set from unchanged semantics.
        let shrunk = ApprovalSnapshot::freeze("job".into(), 7, &state, (120..142).collect());
        assert_eq!(full.snapshot_signature, shrunk.snapshot_signature);
        assert_eq!(full.batches.len(), 3);
        assert_eq!(shrunk.batches.len(), 1);
        let changed_state = serde_json::json!({"pages": [1, 2, 4]});
        let changed = ApprovalSnapshot::freeze("job".into(), 7, &changed_state, (0..142).collect());
        assert_ne!(full.snapshot_signature, changed.snapshot_signature);
    }

    #[test]
    fn real_restart_reattaches_and_keeps_original_plan() {
        let dir = tempdir().unwrap();
        let revision = 7;
        let state = serde_json::json!({"pages": ["semantic"]});
        // First run: 142 dirty pages, original 60/60/22 plan persisted.
        let first = ApprovalSnapshot::freeze("job".into(), revision, &state, (0..142).collect());
        let mut checkpoint = ApprovalCheckpoint::fresh(&first);
        for index in 0..120_usize {
            let expected = provenance(index, "sig");
            checkpoint.mark_complete(complete_entry(&expected, "out"));
        }
        save_approval_checkpoint(dir.path(), revision, &checkpoint).unwrap();
        // Crash. Restart recomputes only 22 dirty pages from unchanged state.
        let restarted =
            ApprovalSnapshot::freeze("job".into(), revision, &state, (120..142).collect());
        assert_eq!(first.snapshot_signature, restarted.snapshot_signature);
        // The production preparation path must load the SAME checkpoint and
        // retain the original plan: exactly 22 renders remain.
        let (loaded, effective) = prepare_resume(dir.path(), revision, &restarted);
        assert_eq!(loaded.snapshot_signature, first.snapshot_signature);
        assert_eq!(loaded.batches, first.batches);
        assert_eq!(effective.len(), 142);
        assert_eq!(loaded.remaining(&restarted).len(), 22);
        assert_eq!(
            &loaded.remaining(&restarted)[..],
            &(120..142).collect::<Vec<_>>()[..]
        );
        // A changed semantic state must NOT reattach.
        let changed =
            ApprovalSnapshot::freeze("job".into(), revision, &serde_json::json!({}), vec![5]);
        let (fresh, plan) = prepare_resume(dir.path(), revision, &changed);
        assert_eq!(plan, vec![5]);
        assert!(fresh.pages.is_empty());
    }

    #[test]
    fn crash_after_120_leaves_22() {
        let dirty: Vec<usize> = (0..142).collect();
        let snapshot = ApprovalSnapshot::freeze("job".into(), 7, &serde_json::json!({}), dirty);
        assert_eq!(snapshot.batches.len(), 3);
        let mut checkpoint = ApprovalCheckpoint::fresh(&snapshot);
        for index in 0..120_usize {
            let expected = provenance(index, "sig");
            checkpoint.mark_complete(complete_entry(&expected, "out"));
        }
        // The original plan keeps its 60/60/22 shape after restart.
        assert_eq!(checkpoint.batches, snapshot.batches);
        assert_eq!(checkpoint.remaining(&snapshot).len(), 22);
        assert_eq!(
            &checkpoint.remaining(&snapshot)[..],
            &(120..142).collect::<Vec<_>>()[..]
        );
    }

    #[test]
    fn restart_merges_newly_invalid_page_into_retained_plan() {
        let dir = tempdir().unwrap();
        let revision = 12;
        let state = serde_json::json!({"pages": ["unchanged semantics"]});
        let first = ApprovalSnapshot::freeze("job".into(), revision, &state, vec![0]);
        let checkpoint = ApprovalCheckpoint::fresh(&first);
        save_approval_checkpoint(dir.path(), revision, &checkpoint).unwrap();

        // Page 10 was clean in the original operation but its cached output
        // disappeared while the process was down. The same semantic snapshot
        // must retain page 0 and append page 10 deterministically.
        let restarted = ApprovalSnapshot::freeze("job".into(), revision, &state, vec![0, 10]);
        let (resumed, plan) = prepare_resume(dir.path(), revision, &restarted);
        assert_eq!(plan, vec![0, 10]);
        assert_eq!(resumed.dirty_plan, vec![0, 10]);
        assert_eq!(resumed.batches, vec![vec![0, 10]]);
        save_approval_checkpoint(dir.path(), revision, &resumed).unwrap();
        let persisted = load_approval_checkpoint(dir.path(), revision).unwrap();
        assert_eq!(persisted.dirty_plan, vec![0, 10]);
    }

    #[test]
    fn zero_dirty_checkpoint_is_durable_before_export_binding() {
        let dir = tempdir().unwrap();
        let revision = 3;
        let state = serde_json::json!({"pages": ["already rendered"]});
        let snapshot = ApprovalSnapshot::freeze("job".into(), revision, &state, Vec::new());
        let (checkpoint, plan) = prepare_resume(dir.path(), revision, &snapshot);
        assert!(plan.is_empty());
        save_approval_checkpoint(dir.path(), revision, &checkpoint).unwrap();
        let persisted = load_approval_checkpoint(dir.path(), revision).unwrap();
        assert!(persisted.matches_snapshot(&snapshot));
        assert!(persisted.pages.is_empty());
    }

    #[test]
    fn one_changed_page_invalidates_only_it() {
        let snapshot =
            ApprovalSnapshot::freeze("job".into(), 3, &serde_json::json!({"rev": 3}), vec![5]);
        let mut checkpoint = ApprovalCheckpoint::fresh(&snapshot);
        let expected = ExpectedPageProvenance {
            page_index: 5,
            page_id: "page-5".into(),
            semantic_render_signature: "sig-5".into(),
            source_sha256: "s".into(),
            clean_sha256: "c".into(),
            output_path: PathBuf::from("/tmp/5.png"),
        };
        checkpoint.mark_complete(complete_entry(&expected, "out"));
        assert!(checkpoint.remaining(&snapshot).is_empty());
        // A new revision with a different snapshot invalidates structurally,
        // and the caller revalidates content per page.
        let changed =
            ApprovalSnapshot::freeze("job".into(), 4, &serde_json::json!({"rev": 4}), vec![5]);
        assert_eq!(checkpoint.remaining(&changed), vec![5]);
    }

    #[test]
    fn corrupt_or_missing_png_fails_validation_while_neighbors_pass() {
        let expected = provenance(0, "sig");
        let entry = complete_entry(&expected, "good-hash");
        assert!(checkpoint_entry_valid(&entry, &expected, Some("good-hash")));
        // Hash recorded at completion no longer matches the live file.
        assert!(!checkpoint_entry_valid(
            &entry,
            &expected,
            Some("changed-hash")
        ));
        // Missing file yields no live hash.
        assert!(!checkpoint_entry_valid(&entry, &expected, None));
        // Status alone never validates.
        let mut status_only = entry.clone();
        status_only.status = "rendering".to_owned();
        assert!(!checkpoint_entry_valid(
            &status_only,
            &expected,
            Some("good-hash")
        ));
        // Wrong page association never validates.
        let mut wrong_page = entry.clone();
        wrong_page.page_index = 9;
        assert!(!checkpoint_entry_valid(
            &wrong_page,
            &expected,
            Some("good-hash")
        ));
    }

    #[test]
    fn old_render_for_superseded_translation_is_rejected() {
        // Frozen expectation is translation NEW; the live sidecar is valid but
        // records translation OLD.
        let expected = provenance(3, "sig-NEW");
        let stale_live = LiveArtifactEvidence {
            sidecar_valid: true,
            sidecar_semantic_signature: Some("sig-OLD".to_owned()),
            source_sha256: expected.source_sha256.clone(),
            clean_sha256: expected.clean_sha256.clone(),
            output_sha256: Some("old-bytes-hash".to_owned()),
        };
        assert_eq!(recover_verified_artifact(&expected, &stale_live), None);
        // A checkpoint entry recorded for OLD also fails against NEW.
        let old_expected = provenance(3, "sig-OLD");
        let old_entry = complete_entry(&old_expected, "old-bytes-hash");
        assert!(!checkpoint_entry_valid(
            &old_entry,
            &expected,
            Some("old-bytes-hash")
        ));
    }

    #[test]
    fn crash_between_artifact_and_checkpoint_recovers_without_rerender() {
        let expected = provenance(3, "sig-3");
        let live = live_for(&expected, "rendered-hash");
        let recovered = recover_verified_artifact(&expected, &live).unwrap();
        assert_eq!(recovered.page_index, 3);
        assert_eq!(recovered.status, "complete");
        assert!(checkpoint_entry_valid(
            &recovered,
            &expected,
            Some("rendered-hash")
        ));
        // Legacy sidecars without a recorded signature must rerender.
        let legacy = LiveArtifactEvidence {
            sidecar_semantic_signature: None,
            ..live
        };
        assert_eq!(recover_verified_artifact(&expected, &legacy), None);
        // Invalid sidecars never promote.
        let invalid = LiveArtifactEvidence {
            sidecar_valid: false,
            ..live_for(&expected, "rendered-hash")
        };
        assert_eq!(recover_verified_artifact(&expected, &invalid), None);
    }

    #[test]
    fn composite_input_hash_binds_every_component() {
        let base = composite_input_hash("p", "s", "c", "sig");
        assert_eq!(base, composite_input_hash("p", "s", "c", "sig"));
        assert_ne!(base, composite_input_hash("q", "s", "c", "sig"));
        assert_ne!(base, composite_input_hash("p", "s2", "c", "sig"));
        assert_ne!(base, composite_input_hash("p", "s", "c2", "sig"));
        assert_ne!(base, composite_input_hash("p", "s", "c", "sig2"));
        // Not a bare copy of the semantic signature under another name.
        assert_ne!(base, "sig");
    }

    #[test]
    fn repeated_approval_same_revision_is_idempotent() {
        let snapshot =
            ApprovalSnapshot::freeze("job".into(), 9, &serde_json::json!({"s": 1}), vec![1, 2]);
        let mut checkpoint = ApprovalCheckpoint::fresh(&snapshot);
        let renders = AtomicUsize::new(0);
        // First run completes both pages.
        for index in [1_usize, 2] {
            renders.fetch_add(1, Ordering::SeqCst);
            let expected = provenance(index, "sig");
            checkpoint.mark_complete(complete_entry(&expected, "out"));
        }
        assert!(checkpoint.remaining(&snapshot).is_empty());
        // Second run for the same frozen snapshot renders nothing new.
        assert!(checkpoint.remaining(&snapshot).is_empty());
        assert_eq!(renders.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn concurrent_approvals_cannot_start_two_pipelines() {
        let dir = tempdir().unwrap();
        let job = dir.path().join("job");
        fs::create_dir_all(&job).unwrap();
        let first = ApprovalLease::try_acquire(&job, 11).unwrap();
        assert!(ApprovalLease::try_acquire(&job, 11).is_err());
        drop(first);
        assert!(ApprovalLease::try_acquire(&job, 11).is_ok());
    }

    #[test]
    fn live_long_running_lease_is_not_stolen_by_age() {
        let dir = tempdir().unwrap();
        let job = dir.path().join("job");
        fs::create_dir_all(&job).unwrap();
        let _lease = ApprovalLease::try_acquire(&job, 21).unwrap();
        // Age the lock file far beyond any fixed window: the live owner must
        // still own it and a second attempt must fail busy.
        let lock = job.join(".fukidashi-render.lock");
        assert!(lock.is_file());
        let file = OpenOptions::new().write(true).open(&lock).unwrap();
        file.set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        drop(file);
        assert!(ApprovalLease::try_acquire(&job, 21).is_err());
    }

    #[test]
    fn stale_dead_owner_is_reclaimed() {
        let dir = tempdir().unwrap();
        let job = dir.path().join("job");
        fs::create_dir_all(&job).unwrap();
        fs::write(job.join(".fukidashi-render.lock"), "pid=4294967295\n").unwrap();
        // No live owner: acquisition reclaims the stale file instead of
        // reporting busy.
        let lease = ApprovalLease::try_acquire(&job, 31).unwrap();
        drop(lease);
    }

    #[test]
    fn ordered_zip_contains_every_page_exactly_once() {
        let dir = tempdir().unwrap();
        let inputs: Vec<PathBuf> = (0..5)
            .map(|index| {
                let path = dir.path().join(format!("cached-{index}.png"));
                fs::write(&path, format!("page-{index}")).unwrap();
                path
            })
            .collect();
        let output = dir.path().join("final.zip");
        assemble_ordered_zip(&inputs, &output, "pages/page-").unwrap();
        let file = fs::File::open(&output).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        assert_eq!(zip.len(), 5);
        let mut names = Vec::new();
        for index in 0..zip.len() {
            names.push(zip.by_index(index).unwrap().name().to_owned());
        }
        assert_eq!(
            names,
            vec![
                "pages/page-0000.png",
                "pages/page-0001.png",
                "pages/page-0002.png",
                "pages/page-0003.png",
                "pages/page-0004.png",
            ]
        );
        // A duplicate input is rejected before anything is published.
        let mut duplicated = inputs.clone();
        duplicated.push(inputs[0].clone());
        assert!(
            assemble_ordered_zip(&duplicated, &dir.path().join("bad.zip"), "pages/page-").is_err()
        );
    }

    #[test]
    fn packaging_failure_publishes_no_partial_archive() {
        let dir = tempdir().unwrap();
        let good = dir.path().join("good.png");
        fs::write(&good, b"good").unwrap();
        let missing = dir.path().join("missing.png");
        let output = dir.path().join("final.zip");
        assert!(assemble_ordered_zip(&[good, missing], &output, "pages/page-").is_err());
        assert!(!output.is_file());
    }

    #[test]
    fn cancellation_after_completed_page_keeps_it_reusable() {
        let snapshot =
            ApprovalSnapshot::freeze("job".into(), 5, &serde_json::json!({}), vec![0, 1, 2]);
        let mut checkpoint = ApprovalCheckpoint::fresh(&snapshot);
        let cancel = AtomicBool::new(false);
        // Complete page 0, then cancel before page 1.
        let expected = provenance(0, "sig");
        checkpoint.mark_complete(complete_entry(&expected, "out"));
        cancel.store(true, Ordering::SeqCst);
        assert!(cancellation_requested(&cancel));
        let remaining = checkpoint.remaining(&snapshot);
        assert_eq!(remaining, vec![1, 2]);
        // The completed page stays structurally complete for this snapshot.
        assert!(checkpoint.completed_for_snapshot(&snapshot).contains(&0));
    }

    #[test]
    fn checkpoint_round_trips_atomically() {
        let dir = tempdir().unwrap();
        let snapshot =
            ApprovalSnapshot::freeze("job".into(), 2, &serde_json::json!({"a": 1}), vec![0]);
        let mut checkpoint = ApprovalCheckpoint::fresh(&snapshot);
        let expected = provenance(0, "s");
        checkpoint.mark_complete(complete_entry(&expected, "o"));
        save_approval_checkpoint(dir.path(), 2, &checkpoint).unwrap();
        let loaded = load_approval_checkpoint(dir.path(), 2).unwrap();
        assert_eq!(loaded, checkpoint);
    }

    #[test]
    fn checkpoint_write_failure_is_reported_not_ignored() {
        let dir = tempdir().unwrap();
        // A job "directory" that is actually a file cannot host a checkpoint.
        let not_a_dir = dir.path().join("not-a-dir");
        fs::write(&not_a_dir, b"{}").unwrap();
        let snapshot = ApprovalSnapshot::freeze("job".into(), 2, &serde_json::json!({}), vec![0]);
        let checkpoint = ApprovalCheckpoint::fresh(&snapshot);
        assert!(save_approval_checkpoint(&not_a_dir, 2, &checkpoint).is_err());
    }
}
