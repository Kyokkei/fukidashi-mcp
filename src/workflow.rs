//! Server-owned stage tracking for the MCP translation pipeline.
//!
//! The model is allowed to choose text and geometry, but it must not be able
//! to accidentally skip a required image stage.  Every clean and render
//! artifact gets a small sidecar whose source path and hashes form a chain.

use anyhow::{Context, Result, anyhow, bail};
use image::{GenericImageView, GrayImage, ImageFormat, RgbImage};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const CLEAN_SIDECAR_SUFFIX: &str = ".fukidashi-clean.json";
const RENDER_SIDECAR_SUFFIX: &str = ".fukidashi-render.json";
const LORE_FILE_NAME: &str = "lore.json";
const MAX_LORE_BYTES: usize = 1_048_576;
const LEGACY_MANIFEST_NAME: &str = ".fukidashi-job.json";
const MANIFEST_NAME: &str = "job.json";
const MAX_INGRESS_PAGES: usize = 500;
const MAX_INGRESS_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INGRESS_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_INGRESS_IMAGE_DIMENSION: u32 = 12_000;
const MAX_INGRESS_IMAGE_PIXELS: u64 = 100_000_000;

#[derive(Debug, Clone)]
struct PageArtifacts {
    analysis: PathBuf,
    mask: PathBuf,
    cleaned: PathBuf,
    corrected_clean: PathBuf,
    rendered: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanArtifact {
    pub stage: String,
    pub source_image: PathBuf,
    pub cleaned_image: PathBuf,
    pub mask_path: PathBuf,
    pub source_sha256: String,
    pub cleaned_sha256: String,
    pub masked_pixels: u64,
    pub changed_masked_pixels: u64,
    pub changed_ratio: f64,
    #[serde(default)]
    pub source_dark_pixels: u64,
    #[serde(default)]
    pub cleaned_dark_pixels: u64,
    #[serde(default)]
    pub dark_pixel_reduction_ratio: f64,
    /// A preserve-mode page may have no translatable pixels. Its clean stage
    /// is an explicit server-owned pass-through of the source image rather
    /// than an inference result with a fabricated mask.
    #[serde(default)]
    pub passthrough: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderArtifact {
    pub stage: String,
    pub source_image: PathBuf,
    pub cleaned_image: PathBuf,
    pub rendered_image: PathBuf,
    #[serde(default)]
    pub rendered_sha256: String,
    pub clean_sidecar: PathBuf,
    #[serde(default)]
    pub typeset: serde_json::Value,
    pub qa: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobManifest {
    stage: String,
    source_dir: PathBuf,
    #[serde(default)]
    pages: std::collections::BTreeMap<String, PageManifest>,
    #[serde(default)]
    expected_pages: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageManifest {
    source_image: PathBuf,
    state: String,
    /// Hash captured when the page first enters a managed job. Reusing a job
    /// after replacing a source file would otherwise expose stale artifacts.
    #[serde(default)]
    source_sha256: String,
    #[serde(default)]
    cleaned_image: Option<PathBuf>,
    #[serde(default)]
    rendered_image: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Workflow {
    root: PathBuf,
    jobs: Arc<Mutex<std::collections::HashMap<PathBuf, PathBuf>>>,
}

/// Scope captured on the first page analysis for a source folder. Page
/// numbers are one-based and follow the server's natural filename ordering.
/// An explicit path list is useful for non-contiguous or synthetic fixtures.
#[derive(Debug, Clone, Default)]
pub struct ScopeSpec {
    pub start_page: Option<usize>,
    pub end_page: Option<usize>,
    pub include_paths: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Registration {
    pub job_dir: PathBuf,
    pub expected_pages: Vec<PathBuf>,
    pub page_state: String,
}

/// Client-authored translation context. Unknown top-level fields are retained
/// so newer clients can add lore without making older servers discard it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoreDocument {
    #[serde(default = "default_lore_schema")]
    pub schema: u32,
    #[serde(default)]
    pub characters: Vec<LoreCharacter>,
    #[serde(default)]
    pub pronouns: Vec<LorePronoun>,
    #[serde(default)]
    pub glossary: Vec<LoreGlossaryEntry>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LoreCharacter {
    pub id: String,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(skip)]
    shorthand: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct LoreCharacterFields {
    id: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    notes: String,
}

impl<'de> Deserialize<'de> for LoreCharacter {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(name) => {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    return Err(serde::de::Error::custom(
                        "character shorthand must be a non-empty name",
                    ));
                }
                Ok(Self {
                    id: lore_character_id(trimmed),
                    names: vec![trimmed.to_owned()],
                    notes: String::new(),
                    shorthand: true,
                })
            }
            serde_json::Value::Object(_) => {
                let fields: LoreCharacterFields =
                    serde_json::from_value(value).map_err(|error| {
                        serde::de::Error::custom(format!(
                            "character object must contain string id and names: {error}"
                        ))
                    })?;
                Ok(Self {
                    id: fields.id,
                    names: fields.names,
                    notes: fields.notes,
                    shorthand: false,
                })
            }
            _ => Err(serde::de::Error::custom(
                "character must be a name string or {id,names,notes} object",
            )),
        }
    }
}

fn lore_character_id(name: &str) -> String {
    let mut id = String::new();
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            id.push(character);
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
    }
    let id = id.trim_end_matches('-');
    if id.is_empty() {
        "character".to_owned()
    } else {
        id.to_owned()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LorePronoun {
    pub speaker: String,
    pub addressee: String,
    pub pair: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoreGlossaryEntry {
    pub source: String,
    pub target: String,
}

fn default_lore_schema() -> u32 {
    1
}

impl LoreDocument {
    fn validate(mut self) -> Result<Self> {
        if self.schema == 0 {
            bail!("lore schema must be a positive integer");
        }
        if self.characters.len() > 4096
            || self.pronouns.len() > 4096
            || self.glossary.len() > 16_384
        {
            bail!("lore contains too many entries; reduce characters, pronouns, or glossary");
        }
        let mut seen_ids = std::collections::BTreeSet::new();
        let mut shorthand_counts = std::collections::BTreeMap::<String, usize>::new();
        for character in &mut self.characters {
            if character.id.trim().is_empty() {
                bail!("lore character id must not be empty");
            }
            if character.id.chars().count() > 256 {
                bail!("lore character id is too long (maximum 256 characters)");
            }
            if character.names.is_empty()
                || character.names.iter().any(|name| name.trim().is_empty())
            {
                bail!("lore character names must contain at least one non-empty name");
            }
            if character
                .names
                .iter()
                .any(|name| name.chars().count() > 512)
            {
                bail!("lore character name is too long (maximum 512 characters)");
            }
            if character.notes.chars().count() > 4096 {
                bail!("lore character notes are too long (maximum 4096 characters)");
            }
            let base_id = character.id.clone();
            if !seen_ids.insert(base_id.clone()) {
                if !character.shorthand {
                    bail!("lore character ids must be unique; duplicate id {base_id:?}");
                }
                let count = shorthand_counts.entry(base_id.clone()).or_insert(1);
                let mut candidate;
                loop {
                    *count += 1;
                    candidate = format!("{base_id}-{}", *count);
                    if seen_ids.insert(candidate.clone()) {
                        character.id = candidate;
                        break;
                    }
                }
            }
        }
        for pronoun in &self.pronouns {
            if pronoun.speaker.trim().is_empty()
                || pronoun.addressee.trim().is_empty()
                || pronoun.pair.trim().is_empty()
            {
                bail!("lore pronoun entries require speaker, addressee, and pair");
            }
            if pronoun.speaker.chars().count() > 256
                || pronoun.addressee.chars().count() > 256
                || pronoun.pair.chars().count() > 512
            {
                bail!("lore pronoun fields are too long");
            }
        }
        for entry in &self.glossary {
            if entry.source.trim().is_empty() || entry.target.trim().is_empty() {
                bail!("lore glossary entries require source and target");
            }
            if entry.source.chars().count() > 1024 || entry.target.chars().count() > 1024 {
                bail!("lore glossary entries are too long");
            }
        }
        Ok(self)
    }
}

/// One image already staged by an ingress provider. The workflow validates
/// that the path is inside the provider-owned staging directory, decodes one
/// file at a time, and publishes a fresh managed job atomically.
#[derive(Debug, Clone)]
pub struct IngressPageFile {
    pub original_name: String,
    pub path: PathBuf,
}

/// The first page in a managed job that has not reached a verified render.
/// Exposing this through the workflow keeps clients out of internal manifests
/// and makes resume behavior deterministic across MCP hosts clients.
#[derive(Debug, Clone, Serialize)]
pub struct PendingPage {
    pub job_dir: PathBuf,
    pub job_id: String,
    pub page_number: usize,
    pub total_pages: usize,
    pub source_image: PathBuf,
    pub state: String,
    pub analysis_path: PathBuf,
}

/// Operator-facing page fraction for CLI/MCP stderr: current / total * 100.
pub fn page_progress_percent(current_page: usize, total_pages: usize) -> f64 {
    if total_pages == 0 {
        0.0
    } else {
        (current_page as f64) * 100.0 / (total_pages as f64)
    }
}

/// `[FUKIDASHI] [Page 4/6] (66.7%) Analyzing layout & OCR...`
pub fn format_page_progress(current_page: usize, total_pages: usize, stage: &str) -> String {
    format!(
        "[FUKIDASHI] [Page {current_page}/{total_pages}] ({:.1}%) {stage}",
        page_progress_percent(current_page, total_pages)
    )
}

/// Structured progress object attached to strict-v1 start/submit payloads.
pub fn page_progress_json(
    current_page: usize,
    total_pages: usize,
    stage: &str,
) -> serde_json::Value {
    json!({
        "current_page": current_page,
        "total_pages": total_pages,
        "percent": (page_progress_percent(current_page, total_pages) * 10.0).round() / 10.0,
        "stage": stage,
        "message": format_page_progress(current_page, total_pages, stage),
    })
}

/// Stream `[Page X/Y]` status on stderr so a terminal operator is not staring
/// into a silent MCP/CLI process. JSON-RPC stays on stdout.
pub fn emit_page_progress(current_page: usize, total_pages: usize, stage: &str) {
    let line = format_page_progress(current_page, total_pages, stage);
    eprintln!("{line}");
    tracing::info!(
        target: "fukidashi.progress",
        current_page,
        total_pages,
        stage,
        "{line}"
    );
}

/// The server-owned paths and state for one page in a managed job.  Clients
/// normally receive only the page number and an opaque translation token; the
/// record is kept public for the strict loop implementation and diagnostics.
#[derive(Debug, Clone)]
pub struct ManagedPage {
    pub job_dir: PathBuf,
    pub source_image: PathBuf,
    pub state: String,
    pub analysis_path: PathBuf,
    pub mask_path: PathBuf,
    pub cleaned_image: PathBuf,
    pub corrected_clean: PathBuf,
    pub rendered_image: PathBuf,
}

/// Cross-process ownership marker for source-job allocation. The in-process
/// map prevents duplicate work within one server, while this marker closes
/// the window between creating a job directory and publishing its manifest
/// when two MCP processes see the same source folder.
struct SourceAllocationLock {
    _lease: LockLease,
}

struct ManifestLock {
    _lease: LockLease,
}

/// Held while an editor or MCP typeset operation writes a deterministic page
/// render. The lock is advisory but works across independent MCP processes.
pub struct RenderLock {
    _lease: LockLease,
}

struct LockLease {
    path: PathBuf,
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<thread::JoinHandle<()>>,
}

impl Drop for LockLease {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

impl Workflow {
    pub fn new(root: PathBuf) -> Result<Self> {
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()?.join(root)
        };
        reject_symlink_ancestors(&root)
            .with_context(|| format!("inspect jobs root {}", root.display()))?;
        fs::create_dir_all(&root)
            .with_context(|| format!("create jobs root {}", root.display()))?;
        reject_symlink_ancestors(&root)
            .with_context(|| format!("inspect jobs root {}", root.display()))?;
        let root = canonical_path(&root)
            .with_context(|| format!("resolve jobs root {}", root.display()))?;
        Ok(Self {
            root,
            jobs: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read the canonical job lore, returning an empty valid template when a
    /// job has not received client-authored context yet.
    pub fn read_lore(&self, job: &Path) -> Result<serde_json::Value> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let path = job.join(LORE_FILE_NAME);
        if !path.is_file() {
            return Ok(default_lore_value());
        }
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("read lore {}", path.display()))?,
        )
        .with_context(|| format!("parse lore {}", path.display()))?;
        validate_lore_value(value)
    }

    /// Validate and atomically persist client-authored lore in the selected
    /// managed job. The returned value is normalized only by serde defaults.
    pub fn write_lore(&self, job: &Path, value: &serde_json::Value) -> Result<serde_json::Value> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let value = validate_lore_value(value.clone())?;
        atomic_json(&job.join(LORE_FILE_NAME), &value)?;
        Ok(value)
    }

    /// Return the server-owned per-page paths for a registered source image.
    /// New jobs use these deterministic names; legacy jobs keep their existing
    /// flat artifacts when callers provide them explicitly.
    pub fn page_artifacts_for_source(
        &self,
        source: &Path,
    ) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf, PathBuf)> {
        let source = canonical_path(source)?;
        let job = self.allocate_job_for_source(&source)?;
        let manifest = load_manifest(&job)?;
        let paths = page_artifacts(&job, &manifest, &source)?;
        Ok((
            paths.analysis,
            paths.mask,
            paths.cleaned,
            paths.corrected_clean,
            paths.rendered,
        ))
    }

    /// Return the canonical analysis checkpoint for a source page and reject
    /// checkpoints from another managed job or from an arbitrary sibling
    /// path. This binds crop cleaning to the same source/page allocation.
    pub fn require_analysis_for_source(&self, source: &Path, requested: &Path) -> Result<PathBuf> {
        let requested = self.require_owned(requested, "analysis checkpoint")?;
        let canonical = self.page_artifacts_for_source(source)?.0;
        if !paths_same(&requested, &canonical)? {
            bail!(
                "analysis checkpoint must be the canonical managed page artifact {}",
                canonical.display()
            );
        }
        Ok(requested)
    }

    /// Persist the complete analysis in the managed page directory. A caller
    /// may still request an additional checkpoint path for conversation
    /// management; this canonical page checkpoint is always written.
    pub fn write_analysis_artifact(
        &self,
        source: &Path,
        analysis: &serde_json::Value,
    ) -> Result<PathBuf> {
        let source = canonical_path(source)?;
        let job = self.allocate_job_for_source(&source)?;
        let manifest = load_manifest(&job)?;
        let paths = page_artifacts(&job, &manifest, &source)?;
        atomic_json(&paths.analysis, analysis)?;
        Ok(paths.analysis)
    }

    /// Allocate an output directory owned by the server. Source directories
    /// are never used for generated artifacts.
    pub fn allocate_job(&self) -> Result<PathBuf> {
        let job = self.root.join(format!("job--{}", Uuid::new_v4().simple()));
        create_dir_all_owned(&job, &self.root)?;
        create_job_layout(&job, None)?;
        Ok(job)
    }

    pub fn allocate_job_for_source(&self, source: &Path) -> Result<PathBuf> {
        let source = canonical_path(source)?;
        let source_dir = source
            .parent()
            .ok_or_else(|| anyhow!("source image has no parent directory"))?
            .to_path_buf();
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| anyhow!("workflow job lock poisoned"))?;
        if let Some(job) = jobs.get(&source_dir).cloned()
            && is_safe_job_candidate(&job, &self.root)
            && load_manifest(&job).ok().is_some_and(|manifest| {
                paths_same(&manifest.source_dir, &source_dir).unwrap_or(false)
            })
        {
            return Ok(job);
        }
        jobs.remove(&source_dir);
        let _allocation_lock = acquire_source_allocation_lock(&self.root, &source_dir)?;
        if let Some(job) = jobs.get(&source_dir).cloned()
            && is_safe_job_candidate(&job, &self.root)
            && load_manifest(&job).ok().is_some_and(|manifest| {
                paths_same(&manifest.source_dir, &source_dir).unwrap_or(false)
            })
        {
            return Ok(job);
        }
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let candidate = entry.path();
                if !is_safe_job_candidate(&candidate, &self.root) {
                    continue;
                }
                let Ok(bytes) = fs::read(manifest_path(&candidate))
                    .or_else(|_| fs::read(legacy_manifest_path(&candidate)))
                else {
                    continue;
                };
                let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue;
                };
                let Some(raw_dir) = value.get("source_dir").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                if fs::canonicalize(raw_dir)
                    .ok()
                    .and_then(|path| path_identity(&path).ok())
                    == path_identity(&source_dir).ok()
                {
                    jobs.insert(source_dir.clone(), candidate.clone());
                    return Ok(candidate);
                }
            }
        }
        let job = self.allocate_source_job(&source_dir)?;
        let manifest = JobManifest {
            stage: "managed".into(),
            source_dir: source_dir.clone(),
            pages: Default::default(),
            expected_pages: Vec::new(),
        };
        save_manifest(&job, &manifest)?;
        jobs.insert(source_dir, job.clone());
        Ok(job)
    }

    /// Publish a brand-new managed job from image files obtained by an
    /// ingress provider. Files remain in the provider-owned staging tree
    /// until they are validated and copied one at a time into the new V2
    /// job. The workflow never retains a complete chapter in memory and the
    /// only path removed on failure is its own unpublished job staging tree.
    pub fn import_ingress_files(
        &self,
        slug: &str,
        staging_root: &Path,
        pages: Vec<IngressPageFile>,
        provenance: serde_json::Value,
    ) -> Result<Registration> {
        if pages.is_empty() {
            bail!("ingress returned no pages");
        }
        if pages.len() > MAX_INGRESS_PAGES {
            bail!("ingress page count exceeds {MAX_INGRESS_PAGES}");
        }

        reject_symlink_ancestors(staging_root)
            .with_context(|| format!("inspect ingress staging root {}", staging_root.display()))?;
        let staging_root = canonical_path(staging_root)
            .with_context(|| format!("resolve ingress staging root {}", staging_root.display()))?;
        let root_metadata = fs::symlink_metadata(&staging_root)
            .with_context(|| format!("inspect ingress staging root {}", staging_root.display()))?;
        if !root_metadata.is_dir()
            || root_metadata.file_type().is_symlink()
            || is_reparse_point(&root_metadata)
        {
            bail!(
                "ingress staging root is not a regular directory: {}",
                staging_root.display()
            );
        }

        let mut names = std::collections::HashSet::new();
        let mut total_bytes = 0_u64;
        for page in &pages {
            validate_ingress_page_name(&page.original_name)?;
            if !names.insert(page.original_name.to_ascii_lowercase()) {
                bail!("ingress page names must be unique: {}", page.original_name);
            }
            if !page.path.is_absolute() {
                bail!(
                    "ingress page path must be absolute: {}",
                    page.path.display()
                );
            }
            reject_symlink_ancestors(&page.path)?;
            let metadata = fs::symlink_metadata(&page.path)
                .with_context(|| format!("inspect staged ingress page {}", page.path.display()))?;
            if metadata.file_type().is_symlink()
                || is_reparse_point(&metadata)
                || !metadata.is_file()
            {
                bail!(
                    "ingress page is not a regular file without links: {}",
                    page.path.display()
                );
            }
            let canonical = canonical_path(&page.path)?;
            if !path_starts_with(&canonical, &staging_root) {
                bail!(
                    "ingress page escaped its staging directory: {}",
                    page.path.display()
                );
            }
            if metadata.len() == 0 || metadata.len() > MAX_INGRESS_FILE_BYTES {
                bail!(
                    "ingress page {} exceeds the per-page byte limit",
                    page.original_name
                );
            }
            total_bytes = total_bytes.saturating_add(metadata.len());
            if total_bytes > MAX_INGRESS_TOTAL_BYTES {
                bail!("ingress pages exceed the total image byte limit");
            }
        }

        let slug = sanitize_ingress_slug(slug);
        let stage = self
            .root
            .join(format!(".ingress-staging-{}", Uuid::new_v4().simple()));
        let job = self
            .root
            .join(format!("{}--{}", slug, Uuid::new_v4().simple()));
        let result = (|| -> Result<Registration> {
            create_dir_all_owned(&stage, &self.root)?;
            create_job_layout(&stage, None)?;
            let source_dir = job.join("source");
            let expected_pages = pages
                .iter()
                .enumerate()
                .map(|(index, _)| source_dir.join(format!("{:04}.png", index + 1)))
                .collect::<Vec<_>>();
            let mut page_records = std::collections::BTreeMap::new();
            let mut provenance_pages = Vec::with_capacity(pages.len());
            for (index, page) in pages.into_iter().enumerate() {
                let canonical = canonical_path(&page.path)?;
                validate_ingress_image_file(&canonical, &page.original_name)?;
                let image = image::ImageReader::open(&canonical)
                    .with_context(|| format!("open staged ingress page {}", canonical.display()))?
                    .with_guessed_format()
                    .with_context(|| {
                        format!("identify staged ingress page {}", canonical.display())
                    })?
                    .decode()
                    .with_context(|| format!("decode staged ingress page {}", canonical.display()))?
                    .to_rgb8();
                let staged = stage.join("source").join(format!("{:04}.png", index + 1));
                save_image_atomic_owned(&staged, image)
                    .with_context(|| format!("stage ingress page {}", page.original_name))?;
                let final_source = &expected_pages[index];
                page_records.insert(
                    page_key(final_source),
                    PageManifest {
                        source_image: final_source.clone(),
                        state: "pending".into(),
                        source_sha256: sha256_file(&staged)?,
                        cleaned_image: None,
                        rendered_image: None,
                    },
                );
                provenance_pages.push(json!({
                    "page_number": index + 1,
                    "original_name": page.original_name,
                    "source_image": final_source,
                }));
            }
            let manifest = JobManifest {
                stage: "ingress".into(),
                source_dir,
                pages: page_records,
                expected_pages: expected_pages.clone(),
            };
            create_page_layouts(&stage, &manifest)?;
            save_manifest(&stage, &manifest)?;
            atomic_json(
                &stage.join("ingress.json"),
                &json!({
                    "schema": "fukidashi-ingress/v1",
                    "source": provenance,
                    "pages": provenance_pages,
                }),
            )?;
            fs::rename(&stage, &job)
                .with_context(|| format!("publish imported job {}", job.display()))?;
            Ok(Registration {
                job_dir: job.clone(),
                expected_pages,
                page_state: "pending".into(),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    /// Test-only compatibility helper for older in-memory fixtures. Production
    /// ingress uses `import_ingress_files` so a whole chapter is never held as
    /// decoded images.
    #[cfg(test)]
    pub fn import_ingress_pages(
        &self,
        slug: &str,
        pages: Vec<(String, RgbImage)>,
        provenance: serde_json::Value,
    ) -> Result<Registration> {
        if pages.is_empty() {
            bail!("ingress returned no pages");
        }
        if pages.len() > MAX_INGRESS_PAGES {
            bail!("ingress page count exceeds {MAX_INGRESS_PAGES}");
        }
        let mut names = std::collections::HashSet::new();
        for (name, image) in &pages {
            if name.is_empty()
                || name == "."
                || name == ".."
                || name.chars().any(|ch| matches!(ch, '/' | '\\'))
            {
                bail!("ingress page name is not a single safe filename: {name:?}");
            }
            if !names.insert(name.to_ascii_lowercase()) {
                bail!("ingress page names must be unique: {name}");
            }
            let (width, height) = image.dimensions();
            if width == 0 || height == 0 {
                bail!("ingress page {name} has empty dimensions");
            }
        }

        let raw_slug = slug.trim();
        let slug = raw_slug
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let slug = if slug.is_empty() {
            "ingress".to_owned()
        } else {
            slug.chars().take(48).collect()
        };
        let stage = self
            .root
            .join(format!(".ingress-staging-{}", Uuid::new_v4().simple()));
        let job = self
            .root
            .join(format!("{}--{}", slug, Uuid::new_v4().simple()));
        let result = (|| -> Result<Registration> {
            create_dir_all_owned(&stage, &self.root)?;
            create_job_layout(&stage, None)?;
            let source_dir = job.join("source");
            let expected_pages = pages
                .iter()
                .enumerate()
                .map(|(index, _)| source_dir.join(format!("{:04}.png", index + 1)))
                .collect::<Vec<_>>();
            let mut page_records = std::collections::BTreeMap::new();
            let mut provenance_pages = Vec::with_capacity(pages.len());
            for (index, (original_name, image)) in pages.into_iter().enumerate() {
                let staged = stage.join("source").join(format!("{:04}.png", index + 1));
                save_image_atomic(&staged, &image)
                    .with_context(|| format!("stage ingress page {original_name}"))?;
                let final_source = &expected_pages[index];
                page_records.insert(
                    page_key(final_source),
                    PageManifest {
                        source_image: final_source.clone(),
                        state: "pending".into(),
                        source_sha256: sha256_file(&staged)?,
                        cleaned_image: None,
                        rendered_image: None,
                    },
                );
                provenance_pages.push(json!({
                    "page_number": index + 1,
                    "original_name": original_name,
                    "source_image": final_source,
                }));
            }
            let manifest = JobManifest {
                stage: "ingress".into(),
                source_dir,
                pages: page_records,
                expected_pages: expected_pages.clone(),
            };
            create_page_layouts(&stage, &manifest)?;
            save_manifest(&stage, &manifest)?;
            atomic_json(
                &stage.join("ingress.json"),
                &json!({
                    "schema": "fukidashi-ingress/v1",
                    "source": provenance,
                    "pages": provenance_pages,
                }),
            )?;
            fs::rename(&stage, &job)
                .with_context(|| format!("publish imported job {}", job.display()))?;
            Ok(Registration {
                job_dir: job.clone(),
                expected_pages,
                page_state: "pending".into(),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    pub fn acquire_render_lock(&self, job: &Path) -> Result<RenderLock> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let lease = acquire_lock_file(
            job.join(".fukidashi-render.lock"),
            &format!("render job {}", job.display()),
        )?;
        Ok(RenderLock { _lease: lease })
    }

    /// Allocate a readable, collision-safe layout for a new source folder.
    /// Existing UUID-shaped legacy jobs remain valid and are never moved.
    fn allocate_source_job(&self, source_dir: &Path) -> Result<PathBuf> {
        let raw = source_dir
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| std::borrow::Cow::Borrowed("comic"));
        let slug = raw
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let slug = if slug.is_empty() { "comic" } else { &slug };
        let source_id = stable_source_id(source_dir)?;
        for attempt in 0..1000_u32 {
            let id = if attempt == 0 {
                source_id.clone()
            } else {
                format!("{source_id}-{attempt}")
            };
            let job = self
                .root
                .join(format!("{}--{}", &slug[..slug.len().min(48)], id));
            match fs::create_dir(&job) {
                Ok(()) => {
                    create_job_layout(&job, Some(source_dir))?;
                    return Ok(job);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if is_safe_job_candidate(&job, &self.root)
                        && let Ok(existing) = load_manifest(&job)
                        && paths_same(&existing.source_dir, source_dir).unwrap_or(false)
                    {
                        return Ok(job);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("unable to allocate a unique managed job directory")
    }

    pub fn register_analysis(
        &self,
        source: &Path,
        scope: Option<&ScopeSpec>,
    ) -> Result<Registration> {
        let source = canonical_path(source)?;
        let job = self.allocate_job_for_source(&source)?;
        let _manifest_lock = acquire_manifest_lock(&job)?;
        let mut manifest = load_manifest(&job)?;
        if manifest.expected_pages.is_empty() {
            manifest.expected_pages = discover_expected_pages(&source, scope)?;
            manifest.source_dir = source
                .parent()
                .ok_or_else(|| anyhow!("source image has no parent directory"))?
                .to_path_buf();
            for expected in &manifest.expected_pages {
                manifest
                    .pages
                    .entry(page_key(expected))
                    .or_insert_with(|| PageManifest {
                        source_image: expected.clone(),
                        state: "pending".into(),
                        source_sha256: sha256_file(expected).unwrap_or_default(),
                        cleaned_image: None,
                        rendered_image: None,
                    });
            }
            create_page_layouts(&job, &manifest)?;
            save_manifest(&job, &manifest)?;
        } else if let Some(scope) = scope {
            let requested = discover_expected_pages(&source, Some(scope))?;
            if !same_path_list(&requested, &manifest.expected_pages)? {
                bail!(
                    "managed job already has a different expected-page scope; start a new source job or keep the original scope"
                );
            }
        }
        populate_legacy_source_hashes(&mut manifest)?;
        validate_source_hashes(&manifest)?;
        if !manifest
            .expected_pages
            .iter()
            .any(|expected| paths_same(expected, &source).unwrap_or(false))
        {
            bail!(
                "page is outside the managed expected-page scope: {}",
                source.display()
            );
        }
        let entry = manifest
            .pages
            .get_mut(&page_key(&source))
            .ok_or_else(|| anyhow!("expected page is missing from the managed manifest"))?;
        if entry.state == "pending" || entry.state.is_empty() {
            entry.state = "analyzed".into();
        }
        let page_state = entry.state.clone();
        create_page_layouts(&job, &manifest)?;
        save_manifest(&job, &manifest)?;
        Ok(Registration {
            job_dir: job,
            expected_pages: manifest.expected_pages,
            page_state,
        })
    }

    pub fn require_owned(&self, path: &Path, label: &str) -> Result<PathBuf> {
        reject_symlink_ancestors(path)
            .with_context(|| format!("inspect {label} {}", path.display()))?;
        let candidate =
            canonical_path(path).with_context(|| format!("resolve {label} {}", path.display()))?;
        let root = canonical_path(&self.root)
            .with_context(|| format!("resolve jobs root {}", self.root.display()))?;
        if !path_starts_with(&candidate, &root) {
            bail!(
                "{label} must be inside server-owned jobs root {}",
                root.display()
            );
        }
        Ok(candidate)
    }

    /// Resolve the managed job root for an artifact nested below `pages/0001`
    /// or another job subdirectory. Legacy flat artifacts resolve directly to
    /// the job directory, so the same helper covers both layouts.
    fn job_root_for_path(&self, path: &Path) -> Result<PathBuf> {
        reject_symlink_ancestors(path)
            .with_context(|| format!("inspect managed artifact {}", path.display()))?;
        let resolved = canonical_path(path)
            .with_context(|| format!("resolve managed artifact {}", path.display()))?;
        let mut candidate = if resolved.is_dir() {
            resolved
        } else {
            resolved
                .parent()
                .ok_or_else(|| anyhow!("managed artifact has no parent directory"))?
                .to_path_buf()
        };
        let root = canonical_path(&self.root)
            .with_context(|| format!("resolve jobs root {}", self.root.display()))?;
        loop {
            if candidate.parent() == Some(root.as_path())
                && (manifest_path(&candidate).is_file()
                    || legacy_manifest_path(&candidate).is_file())
            {
                return Ok(candidate);
            }
            if candidate == root {
                break;
            }
            candidate = candidate
                .parent()
                .ok_or_else(|| anyhow!("managed artifact is outside jobs root"))?
                .to_path_buf();
        }
        bail!(
            "managed artifact {} is not inside a direct managed job",
            path.display()
        )
    }

    pub fn managed_job_for_path(&self, path: &Path) -> Result<PathBuf> {
        self.job_root_for_path(path)
    }

    pub fn managed_page_records(&self, job: &Path) -> Result<Vec<serde_json::Value>> {
        let manifest = load_manifest(job)?;
        Ok(manifest
            .pages
            .values()
            .map(|page| {
                serde_json::json!({
                    "source_image": page.source_image,
                    "cleaned_image": page.cleaned_image,
                    "rendered_image": page.rendered_image,
                    "state": page.state,
                })
            })
            .collect())
    }

    /// Return the source images approved by the server-owned job manifest.
    ///
    /// The native editor runs in a separate process, so it cannot retain the
    /// in-memory allowlist captured while the review server was started.  It
    /// must reconstruct that list from the managed marker instead of trusting
    /// editable `project.json` paths.  Source hashes are checked before the
    /// list is exposed so a replaced source cannot be smuggled into a render.
    pub fn managed_source_paths_for_editor(&self, job: &Path) -> Result<Vec<PathBuf>> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let manifest = load_manifest(&job)?;
        validate_source_hashes(&manifest)?;
        let sources = if manifest.expected_pages.is_empty() {
            manifest
                .pages
                .values()
                .map(|page| page.source_image.clone())
                .collect::<Vec<_>>()
        } else {
            manifest.expected_pages.clone()
        };
        let mut approved = Vec::with_capacity(sources.len());
        for source in sources {
            let source = canonical_path(&source)
                .with_context(|| format!("resolve managed source {}", source.display()))?;
            let page = manifest.pages.get(&page_key(&source)).ok_or_else(|| {
                anyhow!("managed manifest is missing source {}", source.display())
            })?;
            if !paths_same(&page.source_image, &source)? {
                bail!("managed manifest source does not match its page record");
            }
            if !approved.iter().any(|path| path == &source) {
                approved.push(source);
            }
        }
        Ok(approved)
    }

    /// Resolve the deterministic artifacts for one page without allocating a
    /// new job from the source path.  Strict translation calls use this to
    /// resume a partially completed page and to avoid exposing path choices
    /// to the client model.
    pub fn managed_page(&self, job: &Path, source: &Path) -> Result<ManagedPage> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let source = canonical_path(source)
            .with_context(|| format!("resolve managed source page {}", source.display()))?;
        let manifest = load_manifest(&job)?;
        let page = manifest
            .pages
            .get(&page_key(&source))
            .ok_or_else(|| anyhow!("source image is not registered in the managed job"))?;
        if !paths_same(&page.source_image, &source)? {
            bail!("source image does not match the managed page record");
        }
        let artifacts = page_artifacts(&job, &manifest, &source)?;
        Ok(ManagedPage {
            job_dir: job,
            source_image: page.source_image.clone(),
            state: page.state.clone(),
            analysis_path: artifacts.analysis,
            mask_path: artifacts.mask,
            cleaned_image: page.cleaned_image.clone().unwrap_or(artifacts.cleaned),
            corrected_clean: artifacts.corrected_clean,
            rendered_image: page.rendered_image.clone().unwrap_or(artifacts.rendered),
        })
    }

    /// Return the content hash of an owned managed artifact.  The strict
    /// translation token binds this hash so a second client cannot submit
    /// translations against a changed analysis checkpoint.
    pub fn managed_file_sha256(&self, path: &Path, label: &str) -> Result<String> {
        let path = self.require_owned(path, label)?;
        if !path.is_file() {
            bail!("{label} does not exist: {}", path.display());
        }
        sha256_file(&path)
    }

    /// Total inventory size for `[Page X/Y]` progress, including already
    /// rendered pages. Empty expected_pages falls back to the manifest map.
    pub fn managed_job_page_count(&self, job: &Path) -> Result<usize> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let manifest = load_manifest(&job)?;
        if manifest.expected_pages.is_empty() {
            Ok(manifest.pages.len())
        } else {
            Ok(manifest.expected_pages.len())
        }
    }

    pub fn next_pending_page(&self, job: &Path) -> Result<Option<PendingPage>> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let mut manifest = load_manifest(&job)?;
        // Older jobs only recorded pages in the manifest and left
        // expected_pages empty.  Normalize that inventory before selecting a
        // page so strict resume can use the same canonical page artifacts as
        // a newly registered job.  Keep the legacy manifest filename when it
        // is the only marker present; save_manifest handles that choice.
        if manifest.expected_pages.is_empty() && !manifest.pages.is_empty() {
            let _manifest_lock = acquire_manifest_lock(&job)?;
            manifest = load_manifest(&job)?;
            if manifest.expected_pages.is_empty() && !manifest.pages.is_empty() {
                let mut expected = manifest
                    .pages
                    .values()
                    .map(|page| page.source_image.clone())
                    .collect::<Vec<_>>();
                expected.sort_by(|left, right| natural_cmp(left, right));
                manifest.expected_pages = expected;
                create_page_layouts(&job, &manifest)?;
                save_manifest(&job, &manifest)?;
            }
        }
        validate_source_hashes(&manifest)?;
        let ordered = if manifest.expected_pages.is_empty() {
            let mut pages = manifest
                .pages
                .values()
                .map(|page| page.source_image.clone())
                .collect::<Vec<_>>();
            pages.sort_by(|left, right| natural_cmp(left, right));
            pages
        } else {
            manifest.expected_pages.clone()
        };
        for (index, source) in ordered.iter().enumerate() {
            let Some(page) = manifest.pages.get(&page_key(source)) else {
                bail!(
                    "expected page {} is missing from the managed manifest",
                    index + 1
                );
            };
            if page.state == "rendered"
                && page
                    .rendered_image
                    .as_ref()
                    .is_some_and(|rendered| self.validate_render_input(rendered).is_ok())
            {
                continue;
            }
            let artifacts = page_artifacts(&job, &manifest, source)?;
            return Ok(Some(PendingPage {
                job_dir: job.clone(),
                job_id: job
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                page_number: index + 1,
                total_pages: ordered.len(),
                source_image: page.source_image.clone(),
                state: page.state.clone(),
                analysis_path: artifacts.analysis,
            }));
        }
        Ok(None)
    }

    /// Resolve an existing managed job directory. Job directories are direct
    /// children of the jobs root so a model cannot select an arbitrary folder.
    pub fn resolve_managed_job_path(&self, raw: &str) -> Result<PathBuf> {
        let input = PathBuf::from(raw);
        let candidate = if input.is_absolute() {
            input
        } else {
            self.root.join(input)
        };
        reject_symlink_ancestors(&candidate)
            .with_context(|| format!("inspect managed job directory {}", candidate.display()))?;
        let resolved = canonical_path(&candidate)
            .with_context(|| format!("resolve managed job directory {}", candidate.display()))?;
        let job = if resolved.is_file()
            && resolved
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == MANIFEST_NAME || name == LEGACY_MANIFEST_NAME)
        {
            resolved
                .parent()
                .ok_or_else(|| anyhow!("managed job manifest has no parent directory"))?
                .to_path_buf()
        } else {
            resolved
        };
        let root = canonical_path(&self.root).context("resolve jobs root")?;
        if job == root || job.parent() != Some(root.as_path()) {
            bail!(
                "managed job must be a direct child of server-owned jobs root {}",
                root.display()
            );
        }
        if !job.is_dir()
            || (!manifest_path(&job).is_file() && !legacy_manifest_path(&job).is_file())
        {
            bail!("managed job is missing its job.json manifest");
        }
        Ok(job)
    }

    pub fn resolve_managed_job_id(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty() || id.chars().any(|ch| matches!(ch, '\\' | '/' | ':' | '.')) {
            bail!("job_id must be the directory name returned by the pipeline");
        }
        self.resolve_managed_job_path(id)
    }

    /// Return a render artifact that is both in this managed job and valid in
    /// the clean -> render sidecar chain. An explicit path is accepted only
    /// when it names one of the manifest's rendered pages.
    pub fn verified_render_for_job(&self, job: &Path, requested: Option<&Path>) -> Result<PathBuf> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let manifest = load_manifest(&job)?;
        let mut pages = if manifest.expected_pages.is_empty() {
            manifest.pages.values().collect::<Vec<_>>()
        } else {
            manifest
                .expected_pages
                .iter()
                .filter_map(|source| manifest.pages.get(&page_key(source)))
                .collect::<Vec<_>>()
        };
        pages.sort_by(|left, right| natural_cmp(&left.source_image, &right.source_image));
        if let Some(requested) = requested {
            let requested = self.require_owned(requested, "editor image")?;
            if !paths_same(&self.job_root_for_path(&requested)?, &job)? {
                bail!("rendered artifact must belong to the selected managed job");
            }
            if !pages.iter().any(|page| {
                page.rendered_image
                    .as_ref()
                    .is_some_and(|render| paths_same(render, &requested).unwrap_or(false))
            }) {
                bail!("path is not a rendered artifact registered in the managed manifest");
            }
            self.validate_render_input(&requested)?;
            return Ok(requested);
        }
        for page in pages {
            if page.state != "rendered" {
                continue;
            }
            if let Some(rendered) = page.rendered_image.as_ref()
                && self.validate_render_input(rendered).is_ok()
            {
                return self.require_owned(rendered, "editor image");
            }
        }
        bail!("no rendered page passed the managed clean/render validation")
    }

    pub fn require_output_owned(&self, path: &Path, label: &str) -> Result<PathBuf> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("{label} has no parent directory"))?;
        let root = canonical_path(&self.root)
            .with_context(|| format!("resolve jobs root {}", self.root.display()))?;
        let absolute_parent = if parent.is_absolute() {
            parent.to_path_buf()
        } else {
            std::env::current_dir()?.join(parent)
        };
        if !lexically_within(&absolute_parent, &root) {
            bail!(
                "{label} must be inside server-owned jobs root {}",
                root.display()
            );
        }
        reject_symlink_ancestors(&absolute_parent)
            .with_context(|| format!("inspect {label} parent {}", parent.display()))?;
        if !absolute_parent.exists() {
            fs::create_dir_all(&absolute_parent)
                .with_context(|| format!("create {label} parent {}", parent.display()))?;
        }
        reject_symlink_ancestors(&absolute_parent)
            .with_context(|| format!("inspect {label} parent {}", parent.display()))?;
        // `fs::canonicalize` can return a Windows verbatim path (`\\?\E:\...`)
        // while the configured root is represented without that prefix. Use
        // the same canonical identity for both sides before enforcing the
        // managed-root boundary.
        let canonical_parent = canonical_path(&absolute_parent)
            .with_context(|| format!("resolve {label} parent {}", parent.display()))?;
        if !path_starts_with(&canonical_parent, &root) {
            bail!(
                "{label} must be inside server-owned jobs root {}",
                root.display()
            );
        }
        let parent = self.require_owned(&canonical_parent, label)?;
        let file = path
            .file_name()
            .ok_or_else(|| anyhow!("{label} has no file name"))?;
        Ok(parent.join(file))
    }

    /// Return the deterministic job-local destination for an approved font.
    /// Sources are limited to existing regular font files in the managed job's
    /// `fonts` directory or the Windows system font directory. Files elsewhere
    /// in a job are not treated as font provenance merely because they are
    /// readable.
    pub fn font_destination(&self, job: &Path, source: &Path) -> Result<(PathBuf, String)> {
        reject_symlink_ancestors(job)
            .with_context(|| format!("inspect font job {}", job.display()))?;
        let job =
            canonical_path(job).with_context(|| format!("resolve font job {}", job.display()))?;
        let source = canonical_path(source)
            .with_context(|| format!("resolve font source {}", source.display()))?;
        if !source.is_file() {
            bail!(
                "font path {} is not an existing regular file",
                source.display()
            );
        }
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        if !matches!(extension.as_str(), "ttf" | "otf" | "ttc") {
            bail!(
                "font path {} is not a supported TTF/OTF/TTC file",
                source.display()
            );
        }
        let fonts_dir = job.join("fonts");
        reject_symlink_ancestors(&fonts_dir)
            .with_context(|| format!("inspect managed font directory {}", fonts_dir.display()))?;
        let approved_job_font = fonts_dir.is_dir() && path_is_within(&source, &fonts_dir)?;
        let approved_discovered = font_search_dirs()
            .into_iter()
            .filter(|root| root.is_dir())
            .any(|root| path_is_within(&source, &root).unwrap_or(false));
        let approved_explicit = configured_font_paths()
            .into_iter()
            .filter_map(|path| fs::canonicalize(path).ok())
            .any(|path| path == source);
        let approved = approved_job_font || approved_discovered || approved_explicit;
        if !approved {
            bail!(
                "font path {} is outside approved font provenance; use a managed job font, configure FUKIDASHI_FONT_PATH/FUKIDASHI_FONT_DIRS, or install it in a platform font directory",
                source.display()
            );
        }
        let bytes = fs::read(&source).with_context(|| format!("read font {}", source.display()))?;
        if bytes.is_empty() || bytes.len() > 128 * 1024 * 1024 {
            bail!("font path {} has an invalid size", source.display());
        }
        let digest = Sha256::digest(&bytes);
        let hash = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let name = source
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("font.ttf")
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        Ok((fonts_dir.join(format!("{}-{}", &hash[..16], name)), hash))
    }

    pub fn materialize_font_path(&self, job: &Path, source: &Path) -> Result<PathBuf> {
        let (destination, _) = self.font_destination(job, source)?;
        let parent = destination
            .parent()
            .ok_or_else(|| anyhow!("font destination has no parent"))?;
        create_dir_all_owned(parent, job)?;
        let resolved_parent = canonical_path(parent)?;
        let resolved_job = canonical_path(job)?;
        if !path_starts_with(&resolved_parent, &resolved_job) {
            bail!("font destination escaped the managed job");
        }
        if destination.is_file() {
            return canonical_path(&destination);
        }
        let temporary = tempfile::NamedTempFile::new_in(parent).context("create managed font")?;
        fs::copy(source, temporary.path())
            .with_context(|| format!("copy approved font {}", source.display()))?;
        temporary
            .persist(&destination)
            .map_err(|error| anyhow!("promote managed font: {}", error.error))?;
        canonical_path(&destination)
    }

    /// Materialize one compile-time bundled font into a managed job.
    ///
    /// The destination is content-addressed and created with a same-directory
    /// temporary file so a release binary can provide fonts without relying on
    /// its current working directory or on a separate download. Existing
    /// destinations are verified byte-for-byte before they are reused.
    pub fn materialize_bundled_font(
        &self,
        job: &Path,
        font: &crate::fonts::BundledFont,
    ) -> Result<PathBuf> {
        if !font.has_expected_sha256() {
            bail!(
                "bundled font {} failed its embedded SHA-256 integrity check",
                font.file_name
            );
        }
        let job = self.require_owned(job, "bundled font job")?;
        let name = font
            .file_name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if name.is_empty() {
            bail!("bundled font has an empty file name");
        }
        let destination = job
            .join("fonts")
            .join(format!("{}-{}", &font.sha256[..16], name));
        let destination = self.require_output_owned(&destination, "bundled font destination")?;
        let parent = destination
            .parent()
            .ok_or_else(|| anyhow!("bundled font destination has no parent"))?;
        create_dir_all_owned(parent, &job)?;
        let resolved_job = canonical_path(&job)?;
        let resolved_parent = canonical_path(parent)?;
        if !path_starts_with(&resolved_parent, &resolved_job) {
            bail!("bundled font destination escaped the managed job");
        }

        if let Some(existing) = verified_bundled_destination(&destination, font)? {
            if !path_starts_with(&existing, &resolved_job) {
                bail!("bundled font destination escaped the managed job");
            }
            return Ok(existing);
        }

        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).context("create bundled managed font")?;
        temporary
            .write_all(font.bytes)
            .context("write bundled managed font")?;
        temporary
            .as_file()
            .sync_all()
            .context("flush bundled managed font")?;
        match temporary.persist(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = verified_bundled_destination(&destination, font)?
                    .ok_or_else(|| anyhow!("bundled font disappeared during materialization"))?;
                if !path_starts_with(&existing, &resolved_job) {
                    bail!("bundled font destination escaped the managed job");
                }
                return Ok(existing);
            }
            Err(error) => {
                return Err(anyhow!("promote bundled managed font: {}", error.error));
            }
        }
        let resolved = canonical_path(&destination)?;
        if !path_starts_with(&resolved, &resolved_job) {
            bail!("bundled font destination escaped the managed job");
        }
        Ok(resolved)
    }

    pub fn write_clean_artifact(
        &self,
        source: &Path,
        cleaned: &RgbImage,
        mask: &GrayImage,
        dilation: u8,
        mode: &str,
    ) -> Result<(PathBuf, PathBuf, serde_json::Value)> {
        let source = canonical_path(source)
            .with_context(|| format!("resolve source image {}", source.display()))?;
        let job = self.allocate_job_for_source(&source)?;
        let _render_lock = self.acquire_render_lock(&job)?;
        self.write_clean_artifact_locked(&source, cleaned, mask, dilation, mode)
    }

    /// Persist a clean artifact while the caller already owns the job render
    /// lock.  The native editor uses this when an operator adds a bubble over
    /// detector-missed prose: the source region is inpainted first, then the
    /// editor render can typeset Vietnamese on the resulting clean stage.
    pub(crate) fn write_clean_artifact_locked(
        &self,
        source: &Path,
        cleaned: &RgbImage,
        mask: &GrayImage,
        dilation: u8,
        mode: &str,
    ) -> Result<(PathBuf, PathBuf, serde_json::Value)> {
        let source = canonical_path(source)
            .with_context(|| format!("resolve source image {}", source.display()))?;
        let source_image = image::open(&source).context("decode source image for validation")?;
        if source_image.dimensions() != cleaned.dimensions()
            || mask.dimensions() != cleaned.dimensions()
        {
            bail!("clean stage dimensions do not match source image");
        }
        let mut masked_pixels = 0_u64;
        let mut changed_masked_pixels = 0_u64;
        let mut source_dark_pixels = 0_u64;
        let mut cleaned_dark_pixels = 0_u64;
        let source_rgba = source_image.to_rgba8();
        for (index, pixel) in mask.pixels().enumerate() {
            if pixel[0] == 0 {
                continue;
            }
            masked_pixels += 1;
            let source_rgb = &source_rgba.as_raw()[index * 4..index * 4 + 3];
            let clean_rgb = &cleaned.as_raw()[index * 3..index * 3 + 3];
            let source_luma = (u32::from(source_rgb[0]) * 299
                + u32::from(source_rgb[1]) * 587
                + u32::from(source_rgb[2]) * 114)
                / 1000;
            let clean_luma = (u32::from(clean_rgb[0]) * 299
                + u32::from(clean_rgb[1]) * 587
                + u32::from(clean_rgb[2]) * 114)
                / 1000;
            if source_luma < 96 {
                source_dark_pixels += 1;
            }
            if clean_luma < 96 {
                cleaned_dark_pixels += 1;
            }
            if source_rgb != clean_rgb {
                changed_masked_pixels += 1;
            }
        }
        if masked_pixels == 0 {
            bail!("clean stage produced an empty text mask; refusing to typeset");
        }
        if changed_masked_pixels == 0 {
            bail!("clean stage is byte-identical inside its mask; source text was not removed");
        }
        let changed_ratio = changed_masked_pixels as f64 / masked_pixels as f64;
        let dark_pixel_reduction_ratio = if source_dark_pixels == 0 {
            1.0
        } else {
            (source_dark_pixels.saturating_sub(cleaned_dark_pixels) as f64)
                / source_dark_pixels as f64
        };
        if changed_ratio < 0.01 {
            bail!(
                "clean stage changed only {:.2}% of its mask; source text was not reliably removed",
                changed_ratio * 100.0
            );
        }
        let job = self.allocate_job_for_source(&source)?;
        let _manifest_lock = acquire_manifest_lock(&job)?;
        let mut manifest = load_manifest(&job)?;
        if manifest.expected_pages.is_empty() {
            manifest.expected_pages = discover_expected_pages(&source, None)?;
            manifest.source_dir = source
                .parent()
                .ok_or_else(|| anyhow!("source image has no parent directory"))?
                .to_path_buf();
            for expected in &manifest.expected_pages {
                manifest
                    .pages
                    .entry(page_key(expected))
                    .or_insert_with(|| PageManifest {
                        source_image: expected.clone(),
                        state: "pending".into(),
                        source_sha256: sha256_file(expected).unwrap_or_default(),
                        cleaned_image: None,
                        rendered_image: None,
                    });
            }
            create_page_layouts(&job, &manifest)?;
        }
        populate_legacy_source_hashes(&mut manifest)?;
        validate_source_hashes(&manifest)?;
        let paths = page_artifacts(&job, &manifest, &source)?;
        let cleaned_path = paths.cleaned;
        let mask_path = paths.mask;
        save_image_atomic(&cleaned_path, cleaned).context("write owned cleaned image")?;
        save_gray_image_atomic(&mask_path, mask).context("write owned clean mask")?;
        let sidecar_path = clean_sidecar(&cleaned_path);
        let artifact = CleanArtifact {
            stage: "cleaned".into(),
            source_image: source.clone(),
            cleaned_image: cleaned_path.clone(),
            mask_path: mask_path.clone(),
            source_sha256: sha256_file(&source)?,
            cleaned_sha256: sha256_file(&cleaned_path)?,
            masked_pixels,
            changed_masked_pixels,
            changed_ratio,
            source_dark_pixels,
            cleaned_dark_pixels,
            dark_pixel_reduction_ratio,
            passthrough: false,
        };
        atomic_json(&sidecar_path, &serde_json::to_value(&artifact)?)?;
        let entry = manifest
            .pages
            .get_mut(&page_key(&source))
            .ok_or_else(|| anyhow!("cleaned page is outside the managed expected-page scope"))?;
        entry.state = "cleaned".into();
        entry.cleaned_image = Some(cleaned_path.clone());
        save_manifest(&job, &manifest)?;
        let value = json!({
            "image_path": cleaned_path,
            "cleaned_image_path": cleaned_path,
            "mask_path": mask_path,
            "workflow": {
                "stage": "cleaned",
                "sidecar_path": sidecar_path,
                "source_image": source,
                "mode": mode,
                "dilation": dilation,
                "masked_pixels": masked_pixels,
                "changed_masked_pixels": changed_masked_pixels,
                "changed_ratio": changed_ratio,
                "source_dark_pixels": source_dark_pixels,
                "cleaned_dark_pixels": cleaned_dark_pixels,
                "dark_pixel_reduction_ratio": dark_pixel_reduction_ratio,
            }
        });
        Ok((artifact.cleaned_image, artifact.mask_path, value))
    }

    /// Create a verified clean stage for a preserve-mode page with no
    /// translatable dialogue. The unchanged image is intentional: the server
    /// records this as an explicit pass-through so it can advance without
    /// pretending an inpainting mask removed text.
    pub fn write_passthrough_clean_artifact(
        &self,
        source: &Path,
    ) -> Result<(PathBuf, PathBuf, serde_json::Value)> {
        let source = canonical_path(source)
            .with_context(|| format!("resolve source image {}", source.display()))?;
        let source_image = image::open(&source).context("decode source image for pass-through")?;
        let dimensions = source_image.dimensions();
        let source_rgb = source_image.to_rgb8();
        let job = self.allocate_job_for_source(&source)?;
        let _render_lock = self.acquire_render_lock(&job)?;
        let _manifest_lock = acquire_manifest_lock(&job)?;
        let mut manifest = load_manifest(&job)?;
        let paths = page_artifacts(&job, &manifest, &source)?;
        save_image_atomic(&paths.cleaned, &source_rgb).context("write pass-through clean image")?;
        save_gray_image_atomic(&paths.mask, &GrayImage::new(dimensions.0, dimensions.1))
            .context("write pass-through clean mask")?;
        let source_sha256 = sha256_file(&source)?;
        // Pass-through images are decoded and re-encoded as PNG.  Their bytes
        // therefore intentionally differ from the source (which is often
        // WebP), so the clean-stage hash must describe the artifact we wrote.
        let cleaned_sha256 = sha256_file(&paths.cleaned)?;
        let artifact = CleanArtifact {
            stage: "cleaned".into(),
            source_image: source.clone(),
            cleaned_image: paths.cleaned.clone(),
            mask_path: paths.mask.clone(),
            source_sha256: source_sha256.clone(),
            cleaned_sha256,
            masked_pixels: 0,
            changed_masked_pixels: 0,
            changed_ratio: 1.0,
            source_dark_pixels: 0,
            cleaned_dark_pixels: 0,
            dark_pixel_reduction_ratio: 1.0,
            passthrough: true,
        };
        let sidecar_path = clean_sidecar(&paths.cleaned);
        atomic_json(&sidecar_path, &serde_json::to_value(&artifact)?)?;
        let entry = manifest
            .pages
            .get_mut(&page_key(&source))
            .ok_or_else(|| anyhow!("pass-through page is outside the managed job scope"))?;
        entry.state = "cleaned".into();
        entry.cleaned_image = Some(paths.cleaned.clone());
        entry.rendered_image = None;
        save_manifest(&job, &manifest)?;
        Ok((
            paths.cleaned,
            paths.mask,
            json!({
                "image_path": artifact.cleaned_image,
                "cleaned_image_path": artifact.cleaned_image,
                "mask_path": artifact.mask_path,
                "workflow": {
                    "stage": "cleaned",
                    "mode": "preserve",
                    "passthrough": true,
                    "sidecar_path": sidecar_path,
                }
            }),
        ))
    }

    pub fn validate_clean_input(&self, cleaned_path: &Path) -> Result<CleanArtifact> {
        let cleaned_path = self.require_owned(cleaned_path, "typeset input")?;
        if !cleaned_path.is_file() {
            bail!("typeset input does not exist");
        }
        let sidecar_path = clean_sidecar(&cleaned_path);
        let artifact: CleanArtifact = serde_json::from_slice(
            &fs::read(&sidecar_path)
                .with_context(|| format!("read clean-stage sidecar {}", sidecar_path.display()))?,
        )
        .context("parse clean-stage sidecar")?;
        if artifact.stage != "cleaned" || !paths_same(&artifact.cleaned_image, &cleaned_path)? {
            bail!("typeset input is not a valid server-owned cleaned stage");
        }
        let source = canonical_path(&artifact.source_image).context("resolve clean source")?;
        let job = self.job_root_for_path(&cleaned_path)?;
        let manifest = load_manifest(&job)?;
        let page = manifest
            .pages
            .get(&page_key(&source))
            .ok_or_else(|| anyhow!("clean stage source is not registered in its managed job"))?;
        if !paths_same(&page.source_image, &source)? {
            bail!("clean stage source does not match the managed page record");
        }
        if let Some(registered) = page.cleaned_image.as_ref() {
            let registered_parent = registered
                .parent()
                .ok_or_else(|| anyhow!("managed clean artifact has no page directory"))?;
            let cleaned_parent = cleaned_path
                .parent()
                .ok_or_else(|| anyhow!("clean stage has no page directory"))?;
            if !paths_same(registered_parent, cleaned_parent)? {
                bail!("clean stage does not belong to the managed source page");
            }
        }
        if sha256_file(&source)? != artifact.source_sha256
            || sha256_file(&cleaned_path)? != artifact.cleaned_sha256
        {
            bail!("clean stage was changed after validation; rerun cleaning");
        }
        if !artifact.passthrough
            && (artifact.changed_masked_pixels == 0
                || artifact.changed_ratio < 0.01
                || artifact.masked_pixels == 0)
        {
            bail!("clean stage did not reliably remove source text");
        }
        Ok(artifact)
    }

    /// Persist a brush-corrected derivative while retaining the original
    /// clean artifact and its source provenance. The derivative is itself a
    /// valid clean-stage input for a later typeset call.
    pub fn write_derived_clean_artifact(
        &self,
        base: &CleanArtifact,
        output: &Path,
        corrected: &RgbImage,
    ) -> Result<CleanArtifact> {
        let output = self.require_output_owned(output, "corrected clean output")?;
        let parent = output
            .parent()
            .ok_or_else(|| anyhow!("corrected clean output has no parent"))?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        image::DynamicImage::ImageRgb8(corrected.clone())
            .write_to(temp.as_file_mut(), image::ImageFormat::Png)?;
        temp.as_file().sync_all()?;
        temp.persist(&output)
            .map_err(|error| anyhow!("promote corrected clean image: {}", error.error))?;
        let mut derived = base.clone();
        derived.cleaned_image = output.clone();
        derived.cleaned_sha256 = sha256_file(&output)?;
        atomic_json(&clean_sidecar(&output), &serde_json::to_value(&derived)?)?;
        Ok(derived)
    }

    pub fn register_render(
        &self,
        output: &Path,
        clean: &CleanArtifact,
        typeset: serde_json::Value,
        qa: serde_json::Value,
    ) -> Result<PathBuf> {
        let output = self.require_owned(output, "typeset output")?;
        if !output.is_file() {
            bail!("typeset output does not exist");
        }
        let output_job = self.job_root_for_path(&output)?;
        let _render_lock = self.acquire_render_lock(&output_job)?;
        self.register_render_locked(&output, clean, typeset, qa)
    }

    /// Register a render while the caller already owns the job render lock.
    /// MCP and editor renderers use this form to avoid re-entering the lock;
    /// direct workflow callers use `register_render`, which acquires it.
    pub(crate) fn register_render_locked(
        &self,
        output: &Path,
        clean: &CleanArtifact,
        typeset: serde_json::Value,
        qa: serde_json::Value,
    ) -> Result<PathBuf> {
        let output = self.require_owned(output, "typeset output")?;
        if !output.is_file() {
            bail!("typeset output does not exist");
        }
        let output_job = self.job_root_for_path(&output)?;
        let _manifest_lock = acquire_manifest_lock(&output_job)?;
        let clean_path = self.require_owned(&clean.cleaned_image, "clean artifact")?;
        let clean_job = self.job_root_for_path(&clean_path)?;
        if !paths_same(&output_job, &clean_job)? {
            bail!("clean artifact and typeset output must belong to the same managed job");
        }
        let validated_clean = self.validate_clean_input(&clean_path)?;
        if !paths_same(&validated_clean.source_image, &clean.source_image)? {
            bail!("clean artifact source does not match its validated sidecar");
        }
        let mut manifest = load_manifest(&output_job)?;
        let source = canonical_path(&validated_clean.source_image)?;
        let entry_source = manifest
            .pages
            .get(&page_key(&source))
            .map(|entry| entry.source_image.clone())
            .ok_or_else(|| anyhow!("rendered page is not registered in the managed job"))?;
        if !paths_same(&entry_source, &source)? {
            bail!("rendered source does not match the managed page record");
        }
        if manifest_path(&output_job).is_file() && !manifest.expected_pages.is_empty() {
            let expected = page_artifacts(&output_job, &manifest, &source)?.rendered;
            if !paths_same(&output, &expected)? {
                bail!(
                    "managed render must use the deterministic page artifact {}",
                    expected.display()
                );
            }
        }
        validate_typeset_completeness(&output_job, &source, &typeset)?;
        let sidecar = render_sidecar(&output);
        let artifact = RenderArtifact {
            stage: "rendered".into(),
            source_image: validated_clean.source_image.clone(),
            cleaned_image: validated_clean.cleaned_image.clone(),
            rendered_image: output.clone(),
            rendered_sha256: sha256_file(&output)?,
            clean_sidecar: clean_sidecar(&validated_clean.cleaned_image),
            typeset,
            qa,
        };
        atomic_json(&sidecar, &serde_json::to_value(&artifact)?)?;
        let entry = manifest
            .pages
            .get_mut(&page_key(&source))
            .ok_or_else(|| anyhow!("rendered page is not registered in the managed job"))?;
        entry.state = "rendered".into();
        entry.rendered_image = Some(output.clone());
        save_manifest(&output_job, &manifest)?;
        Ok(sidecar)
    }

    pub fn validate_render_input(&self, rendered: &Path) -> Result<RenderArtifact> {
        let rendered = self.require_owned(rendered, "editor image")?;
        let sidecar = render_sidecar(&rendered);
        let artifact: RenderArtifact = serde_json::from_slice(&fs::read(&sidecar)?)?;
        if artifact.stage != "rendered" || !paths_same(&artifact.rendered_image, &rendered)? {
            bail!("editor image is not a server-owned rendered stage");
        }
        if !rendered.is_file() || !artifact.cleaned_image.is_file() {
            bail!("rendered stage is incomplete");
        }
        let clean = self.validate_clean_input(&artifact.cleaned_image)?;
        if !paths_same(&clean.source_image, &artifact.source_image)? {
            bail!("rendered stage source does not match its clean stage");
        }
        if !artifact.rendered_sha256.is_empty()
            && sha256_file(&rendered)? != artifact.rendered_sha256
        {
            bail!("rendered stage was changed after typesetting; rerun typesetting");
        }
        Ok(artifact)
    }

    /// Construct the complete editor state from the managed manifest and
    /// render sidecars. Model-supplied pages are intentionally ignored.
    pub fn editor_state(
        &self,
        rendered: &Path,
        supplied: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let rendered = self.require_owned(rendered, "editor image")?;
        let job = self.job_root_for_path(&rendered)?;
        let manifest = load_manifest(&job)?;
        let mut sources = if manifest.expected_pages.is_empty() {
            manifest
                .pages
                .values()
                .map(|page| page.source_image.clone())
                .collect::<Vec<_>>()
        } else {
            manifest.expected_pages.clone()
        };
        sources.sort_by(|left, right| natural_cmp(left, right));
        if sources.is_empty() {
            bail!("managed job has no expected pages");
        }
        let mut pages = Vec::with_capacity(sources.len());
        let mut global_font_path = None;
        for source in sources {
            let entry = manifest
                .pages
                .get(&page_key(&source))
                .ok_or_else(|| anyhow!("expected page is missing from the managed manifest"))?;
            if entry.state != "rendered" {
                bail!("expected page is not rendered: {}", source.display());
            }
            let cleaned = entry
                .cleaned_image
                .as_ref()
                .ok_or_else(|| anyhow!("rendered page has no clean artifact"))?;
            let rendered_page = entry
                .rendered_image
                .as_ref()
                .ok_or_else(|| anyhow!("rendered page has no render artifact"))?;
            let artifact = self.validate_render_input(rendered_page)?;
            let bubbles = editor_bubbles(&artifact.typeset, &page_key(&source));
            if global_font_path.is_none() {
                global_font_path = artifact
                    .qa
                    .get("global_font_path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
            }
            let removed_bubbles = artifact
                .qa
                .get("removed_bubbles")
                .filter(|value| value.is_array())
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
            let correction_strokes = artifact
                .qa
                .get("correction_strokes")
                .filter(|value| value.is_array())
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
            pages.push(json!({
                "id": stable_page_id(&source),
                "image_path": source,
                "source_image": source,
                "cleaned_image_path": cleaned,
                "corrected_cleaned_image_path": artifact.cleaned_image,
                "rendered_image_path": rendered_page,
                "state": "rendered",
                "bubbles": bubbles,
                "removed_bubbles": removed_bubbles,
                "correction_strokes": correction_strokes,
                "render_dirty": false,
                "rendered_state_revision": 0,
            }));
        }
        let mut state = json!({"schema_version": 1, "pages": pages});
        if let Some(font_path) = global_font_path {
            state["font_path"] = serde_json::Value::String(font_path.clone());
            state["_editor_requested_global_font_path"] = serde_json::Value::String(font_path);
        }
        if let Some(object) = supplied.and_then(serde_json::Value::as_object) {
            for key in ["title", "source_language", "target_language", "metadata"] {
                if let Some(value) = object.get(key) {
                    state[key] = value.clone();
                }
            }
            if let Some(font_path) = object.get("font_path") {
                state["font_path"] = font_path.clone();
                if font_path
                    .as_str()
                    .is_some_and(|path| !path.trim().is_empty())
                {
                    // The public value is the current operator input. Refresh
                    // the private marker with it so a stale marker cannot
                    // suppress a global font change.
                    state["_editor_requested_global_font_path"] = font_path.clone();
                } else {
                    state
                        .as_object_mut()
                        .expect("editor state object")
                        .remove("_editor_requested_global_font_path");
                }
            } else if let Some(marker) = object.get("_editor_requested_global_font_path") {
                if state
                    .get("font_path")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|path| !path.trim().is_empty())
                {
                    state["_editor_requested_global_font_path"] = marker.clone();
                }
            }
        }
        Ok(state)
    }

    pub fn validate_review_state(&self, rendered: &Path, state: &serde_json::Value) -> Result<()> {
        let rendered = self.require_owned(rendered, "editor image")?;
        let job = self.job_root_for_path(&rendered)?;
        let manifest = load_manifest(&job)?;
        if manifest.pages.is_empty() {
            bail!("managed job has no analyzed pages");
        }
        let pending = manifest
            .pages
            .values()
            .filter(|page| page.state != "rendered")
            .map(|page| page.source_image.display().to_string())
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            bail!(
                "cannot start review; managed pages are not rendered: {}",
                pending.join(", ")
            );
        }
        let pages = state
            .get("pages")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow!("editor state must contain a pages array for a managed job"))?;
        if pages.len() != manifest.pages.len() {
            bail!(
                "editor state has {} pages but managed job has {}",
                pages.len(),
                manifest.pages.len()
            );
        }
        let expected = manifest
            .pages
            .values()
            .filter_map(|page| page.rendered_image.as_ref())
            .map(|path| path_identity(path))
            .collect::<Result<std::collections::BTreeSet<_>>>()?;
        let mut supplied = std::collections::BTreeSet::new();
        for page in pages {
            let rendered_path = page
                .get("rendered_image_path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("every managed editor page needs rendered_image_path"))?;
            let rendered_path = path_identity(Path::new(rendered_path))
                .with_context(|| format!("resolve rendered page {rendered_path}"))?;
            if !supplied.insert(rendered_path.clone()) {
                bail!("editor state lists the same rendered page more than once");
            }
            if !expected.contains(&rendered_path) {
                bail!("editor page rendered_image_path is not a rendered managed artifact");
            }
        }
        if supplied != expected {
            bail!("editor state does not include every rendered managed page exactly once");
        }
        Ok(())
    }

    /// Validate every rendered sidecar before a managed review is approved.
    ///
    /// The editor may reopen legacy jobs whose older artifacts predate the
    /// completeness checks. Keeping this gate at approval time lets an
    /// operator repair those pages in the editor while still preventing an
    /// unchanged English or silently erased region from reaching export.
    pub fn validate_editor_completeness(
        &self,
        job: &Path,
        state: &serde_json::Value,
    ) -> Result<()> {
        let job = canonical_path(job)?;
        let manifest = load_manifest(&job)?;
        let mut expected_pages = if manifest.expected_pages.is_empty() {
            manifest
                .pages
                .values()
                .map(|page| page.source_image.clone())
                .collect::<Vec<_>>()
        } else {
            manifest.expected_pages.clone()
        };
        if manifest.expected_pages.is_empty() {
            expected_pages.sort_by(|left, right| natural_cmp(left, right));
        }
        let page_count = state
            .get("pages")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        if page_count != expected_pages.len() {
            bail!(
                "review state has {page_count} pages but the managed job requires {}",
                expected_pages.len()
            );
        }
        // Validate every page before returning.  The old loop returned on the
        // first failure, which made a large review feel like whack-a-mole:
        // fixing page 2 simply revealed page 99 on the next approval attempt.
        // Keep each page's one-based number and source name attached to its
        // own error so the operator can repair all affected pages in one pass.
        let mut failures = Vec::new();
        for (page_index, source) in expected_pages.into_iter().enumerate() {
            let result = (|| -> Result<()> {
                let page = manifest
                    .pages
                    .get(&page_key(&source))
                    .ok_or_else(|| anyhow!("expected page is missing from the managed manifest"))?;
                let rendered = page.rendered_image.as_ref().ok_or_else(|| {
                    anyhow!("expected page is not rendered: {}", source.display())
                })?;
                let artifact = self.validate_render_input(rendered)?;
                validate_typeset_completeness(&job, &source, &artifact.typeset)
            })()
            .with_context(|| {
                format!(
                    "page {} completeness ({})",
                    page_index + 1,
                    source.display()
                )
            });
            if let Err(error) = result {
                failures.push(format!("page {} ({error:#})", page_index + 1));
            }
        }
        if !failures.is_empty() {
            bail!(
                "managed review completeness failed on {} page(s): {}",
                failures.len(),
                failures.join("; ")
            );
        }
        Ok(())
    }

    /// Return the pages that must be rendered before a managed approval.
    ///
    /// A saved editor flag is only a hint: older project snapshots marked
    /// every bubble dirty because they persisted renderer bookkeeping that
    /// the reconstructed sidecar state recomputed differently. Compare the
    /// canonical render inputs to the managed sidecar, so those stale flags
    /// and report-only differences can reuse a verified render while a real
    /// edit, missing artifact, or invalid artifact still takes the render
    /// path.
    pub fn editor_render_plan(&self, job: &Path, state: &serde_json::Value) -> Result<Vec<usize>> {
        let job = self.resolve_managed_job_path(&job.to_string_lossy())?;
        let manifest = load_manifest(&job)?;
        let mut expected_pages = if manifest.expected_pages.is_empty() {
            manifest
                .pages
                .values()
                .map(|page| page.source_image.clone())
                .collect::<Vec<_>>()
        } else {
            manifest.expected_pages.clone()
        };
        if manifest.expected_pages.is_empty() {
            expected_pages.sort_by(|left, right| natural_cmp(left, right));
        }
        let pages = state
            .get("pages")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow!("review state must contain pages"))?;
        if pages.len() != expected_pages.len() {
            bail!(
                "review state has {} pages but the managed job requires {}",
                pages.len(),
                expected_pages.len()
            );
        }

        let mut rerender = Vec::new();
        for (page_index, source) in expected_pages.into_iter().enumerate() {
            let state_page = &pages[page_index];
            let page = manifest
                .pages
                .get(&page_key(&source))
                .ok_or_else(|| anyhow!("expected page is missing from the managed manifest"))?;
            let Some(rendered) = page.rendered_image.as_ref() else {
                rerender.push(page_index);
                continue;
            };

            let state_rendered_matches = state_page
                .get("rendered_image_path")
                .and_then(serde_json::Value::as_str)
                .map(Path::new)
                .map(|path| {
                    let path = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        job.join(path)
                    };
                    paths_same(&path, rendered).unwrap_or(false)
                })
                .unwrap_or(false);
            let artifact = if page.state == "rendered" && state_rendered_matches {
                self.validate_render_input(rendered).ok()
            } else {
                None
            };
            let Some(artifact) = artifact else {
                rerender.push(page_index);
                continue;
            };

            let cached_page = serde_json::json!({
                "font_path": artifact
                    .qa
                    .get("global_font_path")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                "bubbles": editor_bubbles(&artifact.typeset, &page_key(&source)),
                "removed_bubbles": artifact
                    .qa
                    .get("removed_bubbles")
                    .filter(|value| value.is_array())
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
                "correction_strokes": artifact
                    .qa
                    .get("correction_strokes")
                    .filter(|value| value.is_array())
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
            });
            let mut state_page_for_signature = state_page.clone();
            if let Some(object) = state_page_for_signature.as_object_mut() {
                if let Some(font_path) = state.get("font_path") {
                    object.insert("font_path".to_owned(), font_path.clone());
                }
                if let Some(font_path) = state.get("_editor_requested_global_font_path") {
                    object.insert(
                        "_editor_requested_global_font_path".to_owned(),
                        font_path.clone(),
                    );
                }
            }
            let content_changed = crate::editor::page_render_signature(&state_page_for_signature)
                != crate::editor::page_render_signature(&cached_page);
            let complete = validate_typeset_completeness(&job, &source, &artifact.typeset).is_ok();
            if content_changed || !complete {
                rerender.push(page_index);
            }
        }
        Ok(rerender)
    }

    pub fn validate_export_job(&self, project_dir: &Path) -> Result<()> {
        let project_dir = self.require_owned(project_dir, "export project")?;
        let manifest = load_manifest(&project_dir)?;
        if manifest.pages.is_empty() {
            bail!("managed job has no registered pages");
        }
        let pending = manifest
            .pages
            .values()
            .filter(|page| page.state != "rendered")
            .map(|page| page.source_image.display().to_string())
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            bail!(
                "managed job contains pages that were not rendered: {}",
                pending.join(", ")
            );
        }
        Ok(())
    }
}

fn default_lore_value() -> serde_json::Value {
    serde_json::to_value(LoreDocument {
        schema: default_lore_schema(),
        characters: Vec::new(),
        pronouns: Vec::new(),
        glossary: Vec::new(),
        extra: std::collections::BTreeMap::new(),
    })
    .expect("default lore serializes")
}

fn validate_lore_value(value: serde_json::Value) -> Result<serde_json::Value> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("lore must be a JSON object"))?;
    let encoded = serde_json::to_vec(&value).context("measure lore payload")?;
    if encoded.len() > MAX_LORE_BYTES {
        bail!(
            "lore payload is too large ({} bytes; maximum {} bytes)",
            encoded.len(),
            MAX_LORE_BYTES
        );
    }
    if object.get("schema").is_some_and(serde_json::Value::is_null) {
        bail!("lore schema must be a positive integer");
    }
    const LORE_TEMPLATE: &str = r#"{"schema":1,"characters":["Fuyu"],"pronouns":[],"glossary":[{"source":"proprietress","target":"bà chủ"}]}"#;
    let document: LoreDocument = serde_json::from_value(value).map_err(|error| {
        anyhow!(
            "invalid lore; expected {LORE_TEMPLATE}; character strings are accepted and canonicalized: {error}"
        )
    })?;
    let document = document.validate().map_err(|error| {
        anyhow!(
            "invalid lore; expected {LORE_TEMPLATE}; character strings are accepted and canonicalized: {error}"
        )
    })?;
    serde_json::to_value(document).context("serialize validated lore")
}

pub fn clean_sidecar(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), CLEAN_SIDECAR_SUFFIX))
}

fn verified_bundled_destination(
    path: &Path,
    font: &crate::fonts::BundledFont,
) -> Result<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "bundled font destination is a symlink or junction: {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "bundled font destination is not a regular file: {}",
            path.display()
        );
    }
    let bytes = fs::read(path)
        .with_context(|| format!("read bundled font destination {}", path.display()))?;
    if bytes != font.bytes {
        bail!(
            "bundled font destination has unexpected contents: {}",
            path.display()
        );
    }
    Ok(Some(canonical_path(path)?))
}

fn render_sidecar(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), RENDER_SIDECAR_SUFFIX))
}

fn page_key(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn stable_page_id(path: &Path) -> String {
    let mut hasher = Sha256::new();
    let identity = path_identity(path).unwrap_or_else(|_| path.to_string_lossy().into_owned());
    hasher.update(identity.as_bytes());
    format!("page-{:x}", hasher.finalize())
}

fn is_vietnamese_target(target_language: Option<&str>) -> bool {
    target_language
        .unwrap_or_default()
        .split(['-', '_'])
        .next()
        .is_some_and(|language| language.eq_ignore_ascii_case("vi"))
}

fn is_english_prose(source_language: &str, text: &str) -> bool {
    let language = source_language.to_ascii_lowercase();
    let language_is_english = language == "en"
        || language.starts_with("en-")
        || language.starts_with("en|")
        || language == "auto"
        || language == "und"
        || language.is_empty();
    if !language_is_english {
        return false;
    }
    let text = text.trim();
    if text.len() < 12 {
        return false;
    }
    let latin_letters = text
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .count();
    let letters = text
        .chars()
        .filter(|character| character.is_alphabetic())
        .count();
    if latin_letters < 10 || letters == 0 || latin_letters * 100 < letters * 65 {
        return false;
    }
    let words = text
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_ascii_alphabetic()))
        .filter(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|character| character.is_ascii_alphabetic())
        })
        .collect::<Vec<_>>();
    if words.len() < 3 {
        return false;
    }
    let stopword_hits = words
        .iter()
        .filter(|word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "a" | "an"
                    | "and"
                    | "are"
                    | "but"
                    | "for"
                    | "from"
                    | "he"
                    | "i"
                    | "in"
                    | "is"
                    | "it"
                    | "of"
                    | "on"
                    | "she"
                    | "that"
                    | "the"
                    | "this"
                    | "to"
                    | "was"
                    | "we"
                    | "were"
                    | "with"
                    | "you"
            )
        })
        .count();
    let unique_words = words
        .iter()
        .map(|word| word.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    text.chars()
        .any(|character| ".?!,:;'\"".contains(character))
        || stopword_hits >= 2
        || (words.len() >= 4 && unique_words >= 2)
}

fn normalized_source_text(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn source_text_covers_line(source: &str, line: &str, source_language: &str) -> bool {
    if source.trim().is_empty() || line.trim().is_empty() {
        return false;
    }
    let source_normalized = normalized_source_text(source);
    let line_normalized = normalized_source_text(line);
    if !line_normalized.is_empty() && source_normalized == line_normalized {
        return true;
    }
    if line_normalized.len() < 8 || source_normalized.contains(&line_normalized) {
        return !line_normalized.is_empty() && source_normalized.contains(&line_normalized);
    }
    if !is_english_prose(source_language, line) {
        return false;
    }
    let source_words = source
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let line_words = line
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if line_words.len() < 2 {
        return false;
    }
    let matching = line_words
        .iter()
        .filter(|word| source_words.iter().any(|candidate| candidate == *word))
        .count();
    matching * 4 >= line_words.len() * 3
}

fn explicit_source_preserve(item: &serde_json::Value) -> bool {
    item.get("keep_source")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || item
            .get("preserve_source")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn default_source_preserve(item: &serde_json::Value, replace_sfx: bool) -> bool {
    !replace_sfx
        && (item
            .get("preserve_by_default")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || item
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind == "unmatched_text")
            || item
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id.starts_with("text-")))
}

fn explicit_or_default_source_preserve(item: &serde_json::Value, replace_sfx: bool) -> bool {
    explicit_source_preserve(item) || default_source_preserve(item, replace_sfx)
}

/// Return a rectangle that represents the source area owned by an editor
/// request.  Native editor bubbles can retain detector geometry in
/// `text_bbox`, while newer edits make `bubble_bbox` follow the operator's
/// `bbox`; a full-page afterword may use either form.  Prefer any valid
/// containing rectangle instead of assuming one field is authoritative.
fn request_source_anchor_covers_line(
    request: &serde_json::Value,
    line_bbox: Option<crate::domain::Rect>,
) -> bool {
    let Some(line_bbox) = line_bbox.and_then(|bbox| bbox.validate().ok()) else {
        return false;
    };
    [
        "source_anchor",
        "source_bbox",
        "bubble_bbox",
        "bbox",
        "text_bbox",
    ]
    .into_iter()
    .filter_map(|key| request.get(key))
    .filter_map(|value| serde_json::from_value::<crate::domain::Rect>(value.clone()).ok())
    .filter_map(|bbox| bbox.validate().ok())
    .any(|anchor| {
        // Containment is deliberate.  A mere overlap can leave part of a
        // prose line outside the source anchor and must not satisfy the
        // completeness gate.
        anchor.x1 <= line_bbox.x1
            && anchor.y1 <= line_bbox.y1
            && anchor.x2 >= line_bbox.x2
            && anchor.y2 >= line_bbox.y2
    })
}

fn validate_typeset_completeness(
    job: &Path,
    source: &Path,
    typeset: &serde_json::Value,
) -> Result<()> {
    let manifest = load_manifest(job)?;
    let analysis = page_artifacts(job, &manifest, source)?.analysis;
    if !analysis.is_file() {
        return Ok(());
    }
    let bytes = fs::read(&analysis).context("read analysis for render completeness")?;
    let analysis: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse analysis for render completeness")?;
    let items = analysis
        .get("translation_handoff")
        .and_then(|handoff| handoff.get("items"))
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let target = analysis
        .get("target_language")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            analysis
                .get("translation_handoff")
                .and_then(|handoff| handoff.get("target_language"))
                .and_then(serde_json::Value::as_str)
        });
    let replace_sfx = analysis
        .get("strict_v1")
        .and_then(|strict| strict.get("sfx_mode"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|mode| mode == "replace");
    let requests = typeset
        .get("request_bubbles")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // A detector region must be represented by the persisted handoff before
    // it can be cleaned. This also catches the historical broad-bubble case
    // where one OCR item accidentally erased several independent text lines.
    let handoff_ids = items
        .iter()
        .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    for key in ["bubbles", "unmatched_text"] {
        let Some(regions) = analysis.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for (index, region) in regions.iter().enumerate() {
            let Some(id) = region.get("id").and_then(serde_json::Value::as_str) else {
                bail!("detected {key} region {index} has no stable id");
            };
            if !handoff_ids.contains(id) {
                bail!("detected {key} region {id:?} has no translation handoff item");
            }
        }
    }
    for item in items {
        let Some(id) = item.get("id").and_then(serde_json::Value::as_str) else {
            bail!("translation item without an id cannot be rendered");
        };
        let source_language = item
            .get("source_language")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("auto");
        let source_text = item
            .get("source_text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let prose = is_vietnamese_target(target) && is_english_prose(source_language, source_text);
        let preserved = explicit_source_preserve(item)
            || (default_source_preserve(item, replace_sfx) && !prose);
        if preserved {
            continue;
        }
        let request = requests
            .iter()
            .find(|request| request.get("id").and_then(serde_json::Value::as_str) == Some(id));
        let Some(request) = request else {
            bail!("translation item {id:?} has no rendered bubble");
        };
        let text = request
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if text.trim().is_empty() {
            bail!("translation item {id:?} has an empty rendered translation");
        }
        if prose && normalized_source_text(source_text) == normalized_source_text(text) {
            bail!("translation item {id:?} retained unchanged English prose");
        }
    }
    for (index, request) in requests.iter().enumerate() {
        let text = request
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let preserved = explicit_source_preserve(request)
            || request
                .get("preserve_by_default")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            || request
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind == "unmatched_text")
            || request
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id.starts_with("text-"));
        if text.trim().is_empty() && !preserved {
            bail!("rendered bubble {index} has an empty translation");
        }
    }

    // OCR prose can exist outside a detector bubble. Require a translated
    // request for every such line, while permitting an explicit preserve
    // decision. A manual editor bubble may supply the missing source text for
    // a legacy checkpoint, but an empty request cannot satisfy this gate.
    if is_vietnamese_target(target)
        && let Some(lines) = analysis
            .get("text_lines")
            .and_then(serde_json::Value::as_array)
    {
        for (index, line) in lines.iter().enumerate() {
            let source_language = line
                .get("source_language")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("auto");
            let line_text = line
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !is_english_prose(source_language, line_text) {
                continue;
            }
            let line_bbox = line
                .get("bbox")
                .cloned()
                .and_then(|value| serde_json::from_value::<crate::domain::Rect>(value).ok());
            let covered_by_rendered_request = requests.iter().any(|request| {
                let source = request
                    .get("source_text")
                    .or_else(|| request.get("original_text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let request_language = request
                    .get("source_language")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(source_language);
                let source_text_matches =
                    source_text_covers_line(source, line_text, request_language);
                let source_anchor_matches = request_source_anchor_covers_line(request, line_bbox);
                if !source_text_matches && !source_anchor_matches {
                    return false;
                }
                if explicit_or_default_source_preserve(request, replace_sfx) {
                    return true;
                }
                let text = request
                    .get("text")
                    .or_else(|| request.get("translation"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    return false;
                }
                if !source.trim().is_empty()
                    && normalized_source_text(source) == normalized_source_text(text)
                {
                    return false;
                }
                // A manually added editor bubble may have no OCR source text.
                // Keep an unchanged ASCII prose entry from passing solely on
                // broad geometry, while allowing ordinary Vietnamese text.
                source_text_matches
                    || !(source.trim().is_empty()
                        && text.is_ascii()
                        && is_english_prose(request_language, text))
            });
            // An all-keep strict submission intentionally has no rendered
            // bubbles.  Its handoff items still carry the source decision and
            // detector anchor, so use those anchors for the explicit
            // preserve path while keeping uncovered lines rejected.
            let covered_by_preserved_handoff = items.iter().any(|item| {
                let item_source = item
                    .get("source_text")
                    .or_else(|| item.get("ocr_text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let item_language = item
                    .get("source_language")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(source_language);
                let item_prose = is_english_prose(item_language, item_source);
                let preserved = explicit_source_preserve(item)
                    || (default_source_preserve(item, replace_sfx) && !item_prose);
                preserved
                    && (source_text_covers_line(item_source, line_text, item_language)
                        || request_source_anchor_covers_line(item, line_bbox))
            });
            let covered = covered_by_rendered_request || covered_by_preserved_handoff;
            if !covered {
                bail!(
                    "detected English prose line {index} has no translated or explicitly preserved item"
                );
            }
        }
    }
    Ok(())
}

fn editor_bubbles(typeset: &serde_json::Value, page_key: &str) -> Vec<serde_json::Value> {
    let mut occurrences = std::collections::HashMap::<String, usize>::new();
    let mut used_ids = std::collections::HashSet::<String>::new();
    let requests = typeset
        .get("request_bubbles")
        .and_then(serde_json::Value::as_array);
    let reports = typeset
        .get("report")
        .and_then(|value| value.get("bubbles"))
        .and_then(serde_json::Value::as_array);
    let Some(requests) = requests else {
        return Vec::new();
    };
    requests
        .iter()
        .enumerate()
        .filter_map(|(index, request)| {
            let mut bubble = request.as_object()?.clone();
            let identity = bubble_identity(request);
            let occurrence = occurrences.entry(identity).or_insert(0);
            let occurrence_index = *occurrence;
            *occurrence = occurrence.saturating_add(1);
            let requested_id = bubble
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| stable_bubble_id(request, page_key, occurrence_index));
            let mut bubble_id = requested_id.clone();
            if !used_ids.insert(bubble_id.clone()) {
                let mut suffix = occurrence_index.saturating_add(1);
                loop {
                    let candidate = format!("{requested_id}-{suffix}");
                    if used_ids.insert(candidate.clone()) {
                        bubble_id = candidate;
                        break;
                    }
                    suffix = suffix.saturating_add(1);
                }
            }
            bubble.insert("id".into(), serde_json::Value::String(bubble_id));
            let is_editor_render_payload = bubble
                .get("_editor_render_payload")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let requested_font_path = bubble
                .get("_editor_requested_font_path")
                .or_else(|| bubble.get("requested_font_path"))
                .and_then(serde_json::Value::as_str)
                .filter(|path| !path.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    (!is_editor_render_payload).then(|| {
                        bubble
                            .get("font_path")
                            .and_then(serde_json::Value::as_str)
                            .filter(|path| !path.trim().is_empty())
                            .map(str::to_owned)
                    })?
                });
            let requested_fallback_font_paths = bubble
                .get("_editor_requested_fallback_font_paths")
                .filter(|value| value.is_array())
                .cloned()
                .or_else(|| {
                    bubble
                        .get("fallback_font_paths")
                        .filter(|value| value.is_array())
                        .cloned()
                });
            let requested_min_font_size = bubble
                .get("_editor_requested_min_font_size")
                .cloned()
                .or_else(|| bubble.get("min_font_size").cloned());
            let requested_max_font_size = bubble
                .get("_editor_requested_max_font_size")
                .cloned()
                .or_else(|| bubble.get("max_font_size").cloned());
            let requested_padding =
                bubble
                    .get("_editor_requested_padding")
                    .cloned()
                    .or_else(|| {
                        bubble
                            .get("padding")
                            .filter(|value| !value.is_null())
                            .cloned()
                    });
            if let Some(text) = request.get("text") {
                bubble.insert("translation".into(), text.clone());
            }
            if let Some(report) = reports.and_then(|items| items.get(index))
                && let Some(report) = report.as_object()
            {
                for (key, value) in report {
                    // Older sidecars contain explicit JSON nulls for optional
                    // geometry. Treat those as absent so the concrete layout
                    // produced by the renderer is available to editor
                    // rerenders. A current report's full fallback list also
                    // supersedes a request's used-only legacy list.
                    let replace_fallback_list = key == "fallback_font_paths" && value.is_array();
                    if replace_fallback_list
                        || bubble.get(key).is_none_or(serde_json::Value::is_null)
                    {
                        bubble.insert(key.clone(), value.clone());
                    }
                }
                // The requested font can legitimately be a non-Unicode face;
                // the typesetter may have selected a managed fallback. Keep
                // that resolved face separately so a brush-only editor
                // rerender uses the same metrics and cannot fail a previously
                // valid layout merely because fallback selection is omitted.
                if let Some(font_path) = report
                    .get("resolved_font_path")
                    .or_else(|| report.get("font_path"))
                    .and_then(serde_json::Value::as_str)
                {
                    bubble.insert(
                        "rendered_font_path".into(),
                        serde_json::Value::String(font_path.to_owned()),
                    );
                }
            }
            if let Some(path) = requested_font_path.or_else(|| {
                bubble
                    .get("requested_font_path")
                    .and_then(serde_json::Value::as_str)
                    .filter(|path| !path.trim().is_empty())
                    .map(str::to_owned)
            }) {
                // Keep the requested face beside the resolved request/report
                // fields so reopening an editor render retains user intent.
                bubble.insert(
                    "_editor_requested_font_path".into(),
                    serde_json::Value::String(path),
                );
            }
            if let Some(paths) = requested_fallback_font_paths {
                bubble.insert("_editor_requested_fallback_font_paths".into(), paths);
            }
            if let Some(min_font_size) = requested_min_font_size {
                bubble.insert("_editor_requested_min_font_size".into(), min_font_size);
            }
            if let Some(max_font_size) = requested_max_font_size {
                bubble.insert("_editor_requested_max_font_size".into(), max_font_size);
            }
            if let Some(padding) = requested_padding {
                bubble.insert("_editor_requested_padding".into(), padding);
            }
            Some(serde_json::Value::Object(bubble))
        })
        .collect()
}

fn stable_bubble_id(request: &serde_json::Value, page_key: &str, occurrence: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bubble_identity(request).as_bytes());
    hasher.update(occurrence.to_le_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("{page_key}-bubble-{}", &digest[..16])
}

fn bubble_identity(request: &serde_json::Value) -> String {
    let mut identity = String::new();
    if let Some(object) = request.as_object() {
        for key in ["bbox", "bubble_bbox", "text_bbox"] {
            identity.push_str(key);
            identity.push(':');
            if let Some(value) = object.get(key) {
                identity.push_str(&value.to_string());
            }
            identity.push(';');
        }
    } else {
        identity.push_str(&request.to_string());
    }
    identity
}

/// Return the OS canonical spelling for filesystem I/O. Identity comparisons
/// use `path_identity`, which normalizes only equivalent Windows drive/UNC
/// spellings while leaving verbatim volume paths usable.
fn canonical_path(path: &Path) -> Result<PathBuf> {
    // Keep the OS canonical spelling for all I/O. In particular, Windows may
    // return a verbatim path for long names or a volume GUID; stripping that
    // prefix makes a valid path unusable by subsequent filesystem calls.
    Ok(fs::canonicalize(path)?)
}

fn path_identity(path: &Path) -> Result<String> {
    let canonical = canonical_path(path)?;
    let value = identity_path(&canonical).to_string_lossy().into_owned();
    #[cfg(windows)]
    return Ok(value.to_ascii_lowercase());
    #[cfg(not(windows))]
    Ok(value)
}

fn paths_same(left: &Path, right: &Path) -> Result<bool> {
    Ok(path_identity(left)? == path_identity(right)?)
}

fn path_is_within(path: &Path, root: &Path) -> Result<bool> {
    let path = canonical_path(path)?;
    let root = canonical_path(root)?;
    Ok(path_starts_with(&path, &root))
}

fn lexically_within(path: &Path, root: &Path) -> bool {
    let path = lexical_normalize(path);
    let root = lexical_normalize(root);
    path_starts_with(&path, &root)
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    let path = identity_path(path);
    let root = identity_path(root);
    let mut path_components = path.components();
    root.components().all(|root_component| {
        path_components
            .next()
            .is_some_and(|path_component| component_equal(path_component, root_component))
    })
}

fn component_equal(left: Component<'_>, right: Component<'_>) -> bool {
    if cfg!(windows) {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    } else {
        left == right
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn identity_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r#"\\?\UNC\"#) {
            return PathBuf::from(format!(r#"\\{}"#, rest));
        }
        // Only normalize the standard drive and UNC verbatim forms. Volume
        // GUID and GLOBALROOT paths have no equivalent short spelling and
        // must remain intact for identity comparisons and I/O.
        if let Some(rest) = value.strip_prefix(r#"\\?\"#)
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return PathBuf::from(rest.to_owned());
        }
    }
    path.to_path_buf()
}

fn reject_symlink_ancestors(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = lexical_normalize(&absolute);
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
                bail!(
                    "path component is a symlink, junction, or reparse point: {}",
                    current.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

fn create_dir_all_owned(path: &Path, boundary: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let boundary = if boundary.is_absolute() {
        boundary.to_path_buf()
    } else {
        std::env::current_dir()?.join(boundary)
    };
    if !lexically_within(&absolute, &boundary) {
        bail!(
            "path {} is outside managed directory {}",
            path.display(),
            boundary.display()
        );
    }
    reject_symlink_ancestors(&absolute)?;
    fs::create_dir_all(&absolute)?;
    reject_symlink_ancestors(&absolute)?;
    let resolved = canonical_path(&absolute)?;
    let resolved_boundary = canonical_path(&boundary)?;
    if !path_starts_with(&resolved, &resolved_boundary) {
        bail!(
            "path {} escaped managed directory {}",
            path.display(),
            boundary.display()
        );
    }
    Ok(())
}

fn is_safe_job_candidate(candidate: &Path, root: &Path) -> bool {
    if reject_symlink_ancestors(candidate).is_err() {
        return false;
    }
    if !candidate.is_dir() {
        return false;
    }
    let Ok(resolved) = canonical_path(candidate) else {
        return false;
    };
    let Ok(root) = canonical_path(root) else {
        return false;
    };
    resolved.parent() == Some(root.as_path()) && path_starts_with(&resolved, &root)
}

/// Platform font directories plus explicitly configured directories. The
/// list is discovery-only; callers still validate the file type and copy
/// fonts into a managed job before using them for editable renders.
pub fn font_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(root) = std::env::var_os(crate::config::ENV_STORAGE_ROOT) {
        dirs.push(PathBuf::from(root).join("fonts"));
    }
    dirs.extend(crate::config::configured_font_dirs_from_disk());
    let separator = if cfg!(windows) { ';' } else { ':' };
    if let Some(value) = std::env::var_os("FUKIDASHI_FONT_DIRS") {
        dirs.extend(
            value
                .to_string_lossy()
                .split(separator)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        );
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot")) {
            dirs.push(PathBuf::from(root).join("Fonts"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
            dirs.push(PathBuf::from(data).join("fonts"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/fonts"));
        }
        dirs.extend([
            PathBuf::from("/usr/local/share/fonts"),
            PathBuf::from("/usr/share/fonts"),
        ]);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
        dirs.extend([
            PathBuf::from("/Library/Fonts"),
            PathBuf::from("/System/Library/Fonts"),
        ]);
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Explicit font files configured for fallback/discovery. This is separate
/// from directory discovery so a user can point at one licensed font file.
pub fn configured_font_paths() -> Vec<PathBuf> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    std::env::var_os("FUKIDASHI_FONT_PATH")
        .into_iter()
        .flat_map(|value| {
            value
                .to_string_lossy()
                .split(separator)
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Return whether a font filename names a generic desktop UI face that is
/// unsuitable as a comic bubble's requested primary.  These faces remain
/// eligible in the last-resort fallback pool; this predicate is only for the
/// primary selection boundary.
pub fn is_generic_desktop_font(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_generic_name = |name: &str| {
        matches!(
            name,
            "arial"
                | "ariali"
                | "arialb"
                | "arialbd"
                | "arialbi"
                | "arialbold"
                | "calibri"
                | "calibrib"
                | "calibrii"
                | "calibriz"
                | "calibril"
                | "calibrili"
                | "segoeui"
                | "segoeuib"
                | "segoeuii"
                | "segoeuiz"
                | "seguisym"
                | "tahoma"
                | "tahomabd"
                | "verdana"
                | "verdanab"
                | "verdanai"
                | "verdanaz"
                | "times"
                | "timesbd"
                | "timesbi"
                | "timesi"
                | "timesnewroman"
                | "timesnewromanpsmt"
                | "dejavusans"
                | "dejavusansbold"
                | "dejavusansoblique"
                | "dejavusanscondensed"
                | "arial-bold"
                | "arial-regular"
                | "calibri-bold"
                | "calibri-regular"
                | "segoe-ui"
                | "tahoma-bold"
                | "verdana-bold"
                | "times-new-roman"
                | "dejavu-sans"
        )
    };
    if is_generic_name(&stem) {
        return true;
    }
    // Managed font copies are named `<content-hash>-<original-basename>`.
    // Require a hexadecimal prefix before treating a suffix as the original
    // family, so names such as `MyArialComic.ttf` or `My-Arial.ttf` remain
    // legitimate custom fonts.
    stem.split_once('-').is_some_and(|(prefix, suffix)| {
        prefix.len() >= 8
            && prefix
                .chars()
                .all(|character| character.is_ascii_hexdigit())
            && is_generic_name(suffix)
    })
}

/// Keep generic desktop faces available for coverage, but after bundled and
/// user-provided comic fallbacks so they cannot become the Vietnamese bubble
/// face merely because an old sidecar listed one first.
pub fn order_fallback_font_paths(paths: Vec<String>) -> Vec<String> {
    let mut preferred = Vec::with_capacity(paths.len());
    let mut generic = Vec::new();
    for path in paths {
        if is_generic_desktop_font(Path::new(&path)) {
            generic.push(path);
        } else {
            preferred.push(path);
        }
    }
    preferred.extend(generic);
    preferred
}

pub fn paths_same_public(left: &Path, right: &Path) -> Result<bool> {
    paths_same(left, right)
}

pub fn path_identity_public(path: &Path) -> Result<String> {
    path_identity(path)
}

pub fn path_is_within_public(path: &Path, root: &Path) -> Result<bool> {
    if path.exists() && root.exists() {
        path_is_within(path, root)
    } else {
        Ok(lexically_within(path, root))
    }
}

fn same_path_list(left: &[PathBuf], right: &[PathBuf]) -> Result<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    left.iter()
        .zip(right)
        .try_fold(true, |same, (left, right)| {
            Ok(same && paths_same(left, right)?)
        })
}

fn supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff" | "avif"
            )
        })
        .unwrap_or(false)
}

fn natural_cmp(left: &Path, right: &Path) -> Ordering {
    let left = left
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let right = right
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let (mut li, mut ri) = (0, 0);
    let lb = left.as_bytes();
    let rb = right.as_bytes();
    while li < lb.len() && ri < rb.len() {
        let ld = lb[li].is_ascii_digit();
        let rd = rb[ri].is_ascii_digit();
        if ld && rd {
            let ls = li;
            let rs = ri;
            while li < lb.len() && lb[li].is_ascii_digit() {
                li += 1;
            }
            while ri < rb.len() && rb[ri].is_ascii_digit() {
                ri += 1;
            }
            let ldigits = &left[ls..li];
            let rdigits = &right[rs..ri];
            let ltrim = ldigits.trim_start_matches('0');
            let rtrim = rdigits.trim_start_matches('0');
            let ltrim = if ltrim.is_empty() { "0" } else { ltrim };
            let rtrim = if rtrim.is_empty() { "0" } else { rtrim };
            match ltrim.len().cmp(&rtrim.len()).then_with(|| ltrim.cmp(rtrim)) {
                Ordering::Equal => {}
                order => return order,
            }
        } else {
            match lb[li].cmp(&rb[ri]) {
                Ordering::Equal => {
                    li += 1;
                    ri += 1;
                }
                order => return order,
            }
        }
    }
    left.len().cmp(&right.len())
}

fn discover_expected_pages(source: &Path, scope: Option<&ScopeSpec>) -> Result<Vec<PathBuf>> {
    let source_dir = source
        .parent()
        .ok_or_else(|| anyhow!("source image has no parent directory"))?;
    let mut discovered = fs::read_dir(source_dir)
        .with_context(|| format!("scan source directory {}", source_dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file() && supported_image(path))
        .map(|path| canonical_path(&path).unwrap_or(path))
        .collect::<Vec<_>>();
    discovered.sort_by(|left, right| natural_cmp(left, right));
    if discovered.is_empty() {
        bail!("source directory contains no supported comic images");
    }
    let Some(scope) = scope else {
        return Ok(discovered);
    };
    if let Some(paths) = &scope.include_paths {
        if scope.start_page.is_some() || scope.end_page.is_some() {
            bail!("scope.include_paths cannot be combined with start_page or end_page");
        }
        if paths.is_empty() {
            bail!("scope.include_paths must contain at least one image");
        }
        let mut selected = Vec::new();
        for path in paths {
            let path = canonical_path(path)
                .with_context(|| format!("resolve scope image {}", path.display()))?;
            if path.parent() != Some(source_dir) || !supported_image(&path) {
                bail!("scope image must be a supported file in the source directory");
            }
            if !discovered
                .iter()
                .any(|discovered| paths_same(discovered, &path).unwrap_or(false))
            {
                bail!(
                    "scope image is not present in the source directory: {}",
                    path.display()
                );
            }
            if !selected.contains(&path) {
                selected.push(path);
            }
        }
        selected.sort_by(|left, right| natural_cmp(left, right));
        return Ok(selected);
    }
    let start = scope.start_page.unwrap_or(1);
    let end = scope.end_page.unwrap_or(discovered.len());
    if start == 0 || end < start || end > discovered.len() {
        bail!(
            "scope page range must be one-based and inside 1..={}",
            discovered.len()
        );
    }
    Ok(discovered[start - 1..end].to_vec())
}

fn manifest_path(job: &Path) -> PathBuf {
    job.join(MANIFEST_NAME)
}

fn legacy_manifest_path(job: &Path) -> PathBuf {
    job.join(LEGACY_MANIFEST_NAME)
}

fn load_manifest(job: &Path) -> Result<JobManifest> {
    let path = if manifest_path(job).is_file() {
        manifest_path(job)
    } else {
        legacy_manifest_path(job)
    };
    serde_json::from_slice(&fs::read(path)?).context("parse managed job manifest")
}

fn save_manifest(job: &Path, manifest: &JobManifest) -> Result<()> {
    let value = serde_json::to_value(manifest)?;
    let modern_exists = manifest_path(job).is_file();
    let legacy_exists = legacy_manifest_path(job).is_file();
    // New jobs use one canonical manifest. Existing flat/UUID jobs retain and
    // update only their legacy marker, avoiding a pair of files that can drift
    // after a process interruption.
    if modern_exists || !legacy_exists {
        atomic_json(&manifest_path(job), &value)?;
    } else {
        atomic_json(&legacy_manifest_path(job), &value)?;
    }
    Ok(())
}

fn create_job_layout(job: &Path, source_dir: Option<&Path>) -> Result<()> {
    for directory in ["source", "pages", "fonts", "backups", "review", "output"] {
        create_dir_all_owned(&job.join(directory), job)?;
    }
    if let Some(source_dir) = source_dir {
        atomic_json(
            &job.join("source/source.json"),
            &json!({"source_dir": source_dir}),
        )?;
    }
    Ok(())
}

fn create_page_layouts(job: &Path, manifest: &JobManifest) -> Result<()> {
    for index in 0..manifest.expected_pages.len() {
        create_dir_all_owned(&job.join("pages").join(format!("{:04}", index + 1)), job)?;
    }
    Ok(())
}

fn page_artifacts(job: &Path, manifest: &JobManifest, source: &Path) -> Result<PageArtifacts> {
    let index = manifest
        .expected_pages
        .iter()
        .position(|candidate| paths_same(candidate, source).unwrap_or(false))
        .ok_or_else(|| anyhow!("source image is not registered in the managed job"))?;
    let directory = job.join("pages").join(format!("{:04}", index + 1));
    create_dir_all_owned(&directory, job)?;
    Ok(PageArtifacts {
        analysis: directory.join("analysis.json"),
        mask: directory.join("mask.png"),
        cleaned: directory.join("cleaned.png"),
        corrected_clean: directory.join("corrected-clean.png"),
        rendered: directory.join("rendered.png"),
    })
}

fn stable_source_id(source_dir: &Path) -> Result<String> {
    let digest = source_digest(source_dir)?;
    Ok(digest[..16].to_owned())
}

fn populate_legacy_source_hashes(manifest: &mut JobManifest) -> Result<()> {
    for page in manifest.pages.values_mut() {
        if !page.source_sha256.is_empty() || !page.source_image.is_file() {
            continue;
        }
        let current = sha256_file(&page.source_image)?;
        if let Some(cleaned) = page.cleaned_image.as_deref() {
            let sidecar = clean_sidecar(cleaned);
            if let Ok(bytes) = fs::read(&sidecar)
                && let Ok(artifact) = serde_json::from_slice::<CleanArtifact>(&bytes)
                && !artifact.source_sha256.is_empty()
                && artifact.source_sha256 != current
            {
                bail!(
                    "legacy source image changed since its clean artifact was created: {}",
                    page.source_image.display()
                );
            }
        }
        page.source_sha256 = current;
    }
    Ok(())
}

fn validate_source_hashes(manifest: &JobManifest) -> Result<()> {
    for page in manifest.pages.values() {
        if page.source_sha256.is_empty() || !page.source_image.is_file() {
            continue;
        }
        let current = sha256_file(&page.source_image)?;
        if current != page.source_sha256 {
            bail!(
                "source image changed after managed job allocation: {}; start a new source job before processing it again",
                page.source_image.display()
            );
        }
    }
    Ok(())
}

fn source_digest(source_dir: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(path_identity(source_dir)?.as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

fn acquire_source_allocation_lock(root: &Path, source_dir: &Path) -> Result<SourceAllocationLock> {
    let digest = source_digest(source_dir)?;
    let lease = acquire_lock_file(
        root.join(format!(".fukidashi-source-{digest}.lock")),
        &format!("source job {}", source_dir.display()),
    )?;
    Ok(SourceAllocationLock { _lease: lease })
}

fn acquire_manifest_lock(job: &Path) -> Result<ManifestLock> {
    let lease = acquire_lock_file(
        job.join(".fukidashi-manifest.lock"),
        &format!("managed manifest {}", job.display()),
    )?;
    Ok(ManifestLock { _lease: lease })
}

fn acquire_lock_file(path: PathBuf, label: &str) -> Result<LockLease> {
    const WAIT: Duration = Duration::from_secs(10);
    const STALE: Duration = Duration::from_secs(10 * 60);
    let deadline = Instant::now() + WAIT;
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                let _ = file.sync_all();
                let (stop, receiver) = mpsc::channel();
                let heartbeat_path = path.clone();
                let heartbeat = thread::spawn(move || {
                    while receiver.recv_timeout(Duration::from_secs(30)).is_err() {
                        let Ok(mut file) = OpenOptions::new().write(true).open(&heartbeat_path)
                        else {
                            break;
                        };
                        let _ = file.set_len(0);
                        let _ = writeln!(file, "pid={}", std::process::id());
                        let _ = file.sync_all();
                    }
                });
                return Ok(LockLease {
                    path,
                    stop: Some(stop),
                    heartbeat: Some(heartbeat),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A killed renderer can leave its lease file behind. Once a
                // valid owner PID is demonstrably dead, reclaim immediately;
                // waiting ten minutes makes a repair unnecessarily depend on
                // manual filesystem cleanup. Files with no readable owner
                // metadata retain the age guard to avoid racing a process
                // between create_new and writing its PID.
                let stale = lock_owner_is_dead(&path)
                    || (fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > STALE)
                        && !lock_owner_is_alive(&path));
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!("timed out waiting for another process to finish {}", label);
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn lock_owner_is_alive(path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Some(pid) = contents
        .lines()
        .find_map(|line| line.strip_prefix("pid=")?.trim().parse::<u32>().ok())
    else {
        return false;
    };
    if pid == std::process::id() {
        return true;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut code = 0;
        let result = unsafe { GetExitCodeProcess(handle, &mut code) } != 0;
        unsafe { CloseHandle(handle) };
        result && code == STILL_ACTIVE as u32
    }
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        false
    }
    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

fn lock_owner_is_dead(path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Some(pid) = contents
        .lines()
        .find_map(|line| line.strip_prefix("pid=")?.trim().parse::<u32>().ok())
    else {
        return false;
    };
    !lock_owner_is_alive(path) && pid != std::process::id()
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn atomic_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("sidecar has no parent"))?;
    reject_symlink_ancestors(parent)?;
    fs::create_dir_all(parent)?;
    reject_symlink_ancestors(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map(|_| ())
        .map_err(|error| anyhow!("promote sidecar: {}", error.error))
}

fn save_image_atomic(path: &Path, image: &RgbImage) -> Result<()> {
    save_image_atomic_owned(path, image.clone())
}

fn save_image_atomic_owned(path: &Path, image: RgbImage) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("image has no parent directory"))?;
    reject_symlink_ancestors(parent)?;
    fs::create_dir_all(parent)?;
    reject_symlink_ancestors(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    image::DynamicImage::ImageRgb8(image).write_to(temp.as_file_mut(), image::ImageFormat::Png)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map(|_| ())
        .map_err(|error| anyhow!("promote image: {}", error.error))
}

fn save_gray_image_atomic(path: &Path, image: &GrayImage) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("mask has no parent directory"))?;
    reject_symlink_ancestors(parent)?;
    fs::create_dir_all(parent)?;
    reject_symlink_ancestors(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    image::DynamicImage::ImageLuma8(image.clone())
        .write_to(temp.as_file_mut(), image::ImageFormat::Png)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map(|_| ())
        .map_err(|error| anyhow!("promote mask: {}", error.error))
}

fn sanitize_ingress_slug(raw: &str) -> String {
    let slug = raw
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if slug.is_empty() {
        "ingress".to_owned()
    } else {
        slug.chars().take(48).collect()
    }
}

fn validate_ingress_page_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 240
        || name.chars().any(|ch| {
            ch.is_control() || matches!(ch, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
    {
        bail!("ingress page name is not a safe single filename: {name:?}");
    }
    Ok(())
}

fn validate_ingress_image_file(path: &Path, name: &str) -> Result<()> {
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("open ingress image {name}"))?
        .with_guessed_format()
        .with_context(|| format!("identify ingress image {name}"))?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow!("ingress image {name} has no recognized format"))?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        bail!("ingress image format must be PNG, JPEG, or WebP: {name}");
    }
    let (width, height) = reader
        .into_dimensions()
        .with_context(|| format!("read dimensions for ingress image {name}"))?;
    if width == 0
        || height == 0
        || width > MAX_INGRESS_IMAGE_DIMENSION
        || height > MAX_INGRESS_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_INGRESS_IMAGE_PIXELS
    {
        bail!("ingress image dimensions exceed configured limits: {name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use tempfile::tempdir;

    #[test]
    fn dead_render_lock_owner_is_reclaimed_without_age_wait() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join(".fukidashi-render.lock");
        std::fs::write(&lock, "pid=4294967295\n").unwrap();
        let lease = acquire_lock_file(lock.clone(), "test render job").unwrap();
        assert!(lock.exists());
        drop(lease);
        assert!(!lock.exists());
    }

    #[test]
    fn current_render_lock_owner_is_not_reclaimed() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join(".fukidashi-render.lock");
        std::fs::write(&lock, format!("pid={}\n", std::process::id())).unwrap();
        assert!(!lock_owner_is_dead(&lock));
    }

    #[test]
    fn page_progress_reports_current_over_total() {
        assert_eq!(
            format_page_progress(4, 6, "Analyzing layout & OCR..."),
            "[FUKIDASHI] [Page 4/6] (66.7%) Analyzing layout & OCR..."
        );
        assert_eq!(
            format_page_progress(1, 6, "Inpainting clean mask..."),
            "[FUKIDASHI] [Page 1/6] (16.7%) Inpainting clean mask..."
        );
        let payload = page_progress_json(4, 6, "Typesetting dialogue...");
        assert_eq!(payload["current_page"], 4);
        assert_eq!(payload["total_pages"], 6);
        assert_eq!(payload["percent"], 66.7);
        assert_eq!(payload["stage"], "Typesetting dialogue...");
        assert_eq!(page_progress_percent(0, 0), 0.0);
    }

    #[test]
    fn lore_is_validated_atomically_and_keeps_forward_fields() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("page.png");
        RgbaImage::from_pixel(16, 16, Rgba([255, 255, 255, 255]))
            .save(&source)
            .unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        assert_eq!(
            workflow.read_lore(&registration.job_dir).unwrap()["schema"],
            1
        );
        let authored = json!({
            "schema": 1,
            "characters": [{"id":"lisa","names":["Lisa"],"notes":"manager"}],
            "pronouns": [{"speaker":"lisa","addressee":"senpai","pair":"cậu/tớ"}],
            "glossary": [{"source":"manager","target":"quản lý"}],
            "future_extension": {"voice": "dry"}
        });
        let saved = workflow
            .write_lore(&registration.job_dir, &authored)
            .unwrap();
        assert_eq!(saved["future_extension"]["voice"], "dry");
        assert_eq!(workflow.read_lore(&registration.job_dir).unwrap(), saved);
        assert!(
            workflow
                .write_lore(&registration.job_dir, &json!({"schema": 0}))
                .is_err()
        );
        assert!(registration.job_dir.join("lore.json").is_file());
    }

    #[test]
    fn lore_accepts_natural_character_shorthand_and_canonicalizes_ids() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("page.png");
        RgbaImage::from_pixel(16, 16, Rgba([255, 255, 255, 255]))
            .save(&source)
            .unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        let saved = workflow
            .write_lore(
                &registration.job_dir,
                &json!({
                    "schema": 1,
                    "characters": ["Fuyu", "Fuyu", "Kuga"],
                    "pronouns": [],
                    "glossary": [{"source":"proprietress","target":"bà chủ"}],
                    "future": {"voice": "dry"}
                }),
            )
            .unwrap();
        assert_eq!(
            saved["characters"][0],
            json!({
                "id": "fuyu",
                "names": ["Fuyu"],
                "notes": ""
            })
        );
        assert_eq!(saved["characters"][1]["id"], "fuyu-2");
        assert_eq!(saved["characters"][2]["id"], "kuga");
        assert_eq!(saved["future"]["voice"], "dry");
        assert_eq!(workflow.read_lore(&registration.job_dir).unwrap(), saved);
    }

    #[test]
    fn lore_rejects_malformed_or_unbounded_values_with_actionable_errors() {
        let oversized = json!({"schema":1,"characters":["x".repeat(1_100_000)]});
        let error = validate_lore_value(oversized).unwrap_err().to_string();
        assert!(error.contains("too large"));
        assert!(error.contains("maximum"));

        for value in [
            json!(null),
            json!({"schema":1,"characters":[null]}),
            json!({"schema":1,"characters":[{"id":"x","names":[]}]}),
            json!({"schema":1,"characters":[{"id":"","names":["x"]}]}),
            json!({"schema":1,"pronouns":[{"speaker":"","addressee":"x","pair":"y"}]}),
            json!({"schema":1,"glossary":[{"source":"x","target":""}]}),
        ] {
            let error = validate_lore_value(value).unwrap_err().to_string();
            assert!(error.contains("lore") || error.contains("character"));
            if !error.contains("JSON object") {
                assert!(
                    error.contains("expected")
                        || error.contains("must")
                        || error.contains("require")
                );
            }
        }
    }

    #[test]
    fn generic_desktop_faces_are_primary_only_denylisted() {
        assert!(is_generic_desktop_font(Path::new(
            "C:/Windows/Fonts/Arial.ttf"
        )));
        assert!(is_generic_desktop_font(Path::new(
            "C:/Windows/Fonts/segoeui.ttf"
        )));
        assert!(is_generic_desktop_font(Path::new(
            "C:/Windows/Fonts/DejaVuSans.ttf"
        )));
        assert!(is_generic_desktop_font(Path::new(
            "C:/jobs/fonts/0123456789abcdef-Arial.ttf"
        )));
        assert!(is_generic_desktop_font(Path::new(
            "C:/jobs/fonts/0123456789abcdef-Arial-Bold.ttf"
        )));
        assert!(!is_generic_desktop_font(Path::new(
            "C:/fonts/ComicNeue-Regular.ttf"
        )));
        assert!(!is_generic_desktop_font(Path::new(
            "C:/fonts/MyArialComic.ttf"
        )));
        assert!(!is_generic_desktop_font(Path::new(
            "C:/fonts/PatrickHand-Regular.ttf"
        )));
    }

    #[test]
    fn generic_fallbacks_are_relegated_after_comic_faces() {
        let ordered = order_fallback_font_paths(vec![
            "C:/jobs/fonts/0123456789abcdef-Arial.ttf".into(),
            "C:/fonts/PatrickHand-Regular.ttf".into(),
            "C:/fonts/NotoSansSymbols2-Regular.ttf".into(),
        ]);
        assert_eq!(
            ordered,
            [
                "C:/fonts/PatrickHand-Regular.ttf",
                "C:/fonts/NotoSansSymbols2-Regular.ttf",
                "C:/jobs/fonts/0123456789abcdef-Arial.ttf"
            ]
        );
    }

    #[test]
    fn passthrough_clean_hash_describes_reencoded_png() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("cover.webp");
        let source_image = RgbaImage::from_pixel(12, 9, Rgba([240, 240, 240, 255]));
        image::DynamicImage::ImageRgba8(source_image)
            .save_with_format(&source, image::ImageFormat::WebP)
            .unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        workflow.register_analysis(&source, None).unwrap();
        let (cleaned, _, _) = workflow.write_passthrough_clean_artifact(&source).unwrap();
        let artifact = workflow.validate_clean_input(&cleaned).unwrap();
        assert!(artifact.passthrough);
        assert_eq!(artifact.source_sha256, sha256_file(&source).unwrap());
        assert_eq!(artifact.cleaned_sha256, sha256_file(&cleaned).unwrap());
        assert_ne!(artifact.source_sha256, artifact.cleaned_sha256);
    }

    #[test]
    fn clean_artifact_rejects_unchanged_mask() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let image = RgbaImage::from_pixel(8, 8, Rgba([255, 255, 255, 255]));
        image::DynamicImage::ImageRgba8(image.clone())
            .save(&source)
            .unwrap();
        let rgb = image::DynamicImage::ImageRgba8(image).to_rgb8();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let mut mask = GrayImage::new(8, 8);
        mask.put_pixel(2, 2, image::Luma([255]));
        let error = workflow
            .write_clean_artifact(&source, &rgb, &mask, 3, "crop")
            .unwrap_err();
        assert!(error.to_string().contains("byte-identical"));
    }

    #[test]
    fn derived_clean_artifact_is_owned_and_keeps_original_unchanged() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let mut source_image = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 255]));
        source_image.put_pixel(3, 3, Rgba([20, 20, 20, 255]));
        source_image.save(&source).unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let cleaned = RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 255]));
        let mut mask = GrayImage::new(8, 8);
        mask.put_pixel(3, 3, image::Luma([255]));
        let (cleaned_path, _, _) = workflow
            .write_clean_artifact(&source, &cleaned, &mask, 3, "crop")
            .unwrap();
        let base = workflow.validate_clean_input(&cleaned_path).unwrap();
        let mut corrected = cleaned.clone();
        corrected.put_pixel(4, 4, image::Rgb([1, 2, 3]));
        let derived_path = base
            .cleaned_image
            .parent()
            .unwrap()
            .join("fukidashi-corrected-clean-page-0.png");
        let derived = workflow
            .write_derived_clean_artifact(&base, &derived_path, &corrected)
            .unwrap();
        assert_eq!(
            *image::open(&cleaned_path)
                .unwrap()
                .to_rgb8()
                .get_pixel(4, 4),
            image::Rgb([255, 255, 255])
        );
        assert_eq!(
            *image::open(&derived.cleaned_image)
                .unwrap()
                .to_rgb8()
                .get_pixel(4, 4),
            image::Rgb([1, 2, 3])
        );
        assert!(
            workflow
                .validate_clean_input(&derived.cleaned_image)
                .is_ok()
        );
        assert!(
            workflow
                .write_derived_clean_artifact(&base, &dir.path().join("outside.png"), &corrected)
                .is_err()
        );
    }

    #[test]
    fn clean_artifact_is_required_before_typeset() {
        let dir = tempdir().unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let error = workflow
            .validate_clean_input(&dir.path().join("source.png"))
            .unwrap_err();
        assert!(error.to_string().contains("resolve typeset input"));
    }

    #[test]
    fn expected_inventory_is_natural_sorted_and_scope_is_persistent() {
        let dir = tempdir().unwrap();
        for name in ["page-1.png", "page-2.png", "page-10.png"] {
            RgbaImage::from_pixel(2, 2, Rgba([255, 255, 255, 255]))
                .save(dir.path().join(name))
                .unwrap();
        }
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let scope = ScopeSpec {
            start_page: Some(2),
            end_page: Some(3),
            include_paths: None,
        };
        let page_two = dir.path().join("page-2.png");
        let first = workflow.register_analysis(&page_two, Some(&scope)).unwrap();
        assert_eq!(
            first.expected_pages,
            vec![
                canonical_path(&dir.path().join("page-2.png")).unwrap(),
                canonical_path(&dir.path().join("page-10.png")).unwrap(),
            ]
        );
        let page_ten = dir.path().join("page-10.png");
        workflow.register_analysis(&page_ten, Some(&scope)).unwrap();
        let page_one_error = workflow
            .register_analysis(&dir.path().join("page-1.png"), None)
            .unwrap_err();
        assert!(page_one_error.to_string().contains("outside the managed"));
        let incomplete = workflow.validate_export_job(&first.job_dir).unwrap_err();
        assert!(incomplete.to_string().contains("not rendered"));
    }

    #[test]
    fn readable_jobs_isolate_similar_sources_and_nested_page_lifecycle() {
        let dir = tempdir().unwrap();
        let first_source_dir = dir.path().join("first").join("comic");
        let second_source_dir = dir.path().join("second").join("comic");
        fs::create_dir_all(&first_source_dir).unwrap();
        fs::create_dir_all(&second_source_dir).unwrap();
        for source_dir in [&first_source_dir, &second_source_dir] {
            let mut source = RgbaImage::from_pixel(8, 8, Rgba([255, 255, 255, 255]));
            source.put_pixel(3, 3, Rgba([0, 0, 0, 255]));
            source.save(source_dir.join("page-1.png")).unwrap();
        }
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let first_source = first_source_dir.join("page-1.png");
        let second_source = second_source_dir.join("page-1.png");
        let first = workflow.register_analysis(&first_source, None).unwrap();
        let second = workflow.register_analysis(&second_source, None).unwrap();
        let reopened = Workflow::new(dir.path().join("jobs")).unwrap();
        assert_eq!(
            reopened
                .register_analysis(&first_source, None)
                .unwrap()
                .job_dir,
            first.job_dir
        );
        assert_ne!(first.job_dir, second.job_dir);
        let first_analysis = workflow.page_artifacts_for_source(&first_source).unwrap().0;
        assert!(
            workflow
                .require_analysis_for_source(&second_source, &first_analysis)
                .is_err()
        );
        for registration in [&first, &second] {
            let name = registration.job_dir.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("comic--"));
            assert_eq!(name.len(), "comic--".len() + 16);
            assert!(registration.job_dir.join("job.json").is_file());
            assert!(registration.job_dir.join("source/source.json").is_file());
            assert!(registration.job_dir.join("pages/0001").is_dir());
            assert!(registration.job_dir.join("fonts").is_dir());
            assert!(registration.job_dir.join("backups").is_dir());
            assert!(registration.job_dir.join("review").is_dir());
            assert!(registration.job_dir.join("output").is_dir());
        }
        let cleaned = RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 255]));
        let mut mask = GrayImage::new(8, 8);
        mask.put_pixel(3, 3, image::Luma([255]));
        let (cleaned_path, _, _) = workflow
            .write_clean_artifact(&first_source, &cleaned, &mask, 3, "full")
            .unwrap();
        assert_eq!(
            cleaned_path.file_name().and_then(|name| name.to_str()),
            Some("cleaned.png")
        );
        assert_eq!(
            cleaned_path
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str()),
            Some("0001")
        );
        assert!(cleaned_path.parent().unwrap().join("mask.png").is_file());
        assert!(
            !cleaned_path
                .parent()
                .unwrap()
                .join("corrected-clean.png")
                .exists()
        );
        let rendered = cleaned_path.parent().unwrap().join("rendered.png");
        cleaned.save(&rendered).unwrap();
        let clean = workflow.validate_clean_input(&cleaned_path).unwrap();
        workflow
            .register_render(&rendered, &clean, json!({}), json!({}))
            .unwrap();
        let (second_cleaned_path, _, _) = workflow
            .write_clean_artifact(&second_source, &cleaned, &mask, 3, "full")
            .unwrap();
        let second_rendered = second_cleaned_path.parent().unwrap().join("rendered.png");
        cleaned.save(&second_rendered).unwrap();
        let second_clean = workflow.validate_clean_input(&second_cleaned_path).unwrap();
        let error = workflow
            .register_render(&second_rendered, &clean, json!({}), json!({}))
            .unwrap_err();
        assert!(error.to_string().contains("same managed job"));
        assert!(!crate::workflow::render_sidecar(&second_rendered).exists());
        workflow
            .register_render(&second_rendered, &second_clean, json!({}), json!({}))
            .unwrap();
        assert!(workflow.validate_render_input(&rendered).is_ok());
        let state = workflow.editor_state(&rendered, None).unwrap();
        workflow.validate_review_state(&rendered, &state).unwrap();
        workflow.validate_export_job(&first.job_dir).unwrap();
        assert!(first.job_dir.join("pages/0001/rendered.png").is_file());
        assert!(second.job_dir.join("pages/0001/rendered.png").is_file());
        fs::write(
            first.job_dir.join("project.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "pages": [{
                    "id": "page-1",
                    "image_path": first_source,
                    "rendered_image_path": rendered,
                    "bubbles": []
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            first.job_dir.join("review.json"),
            br#"{"review_session_id":"test-session","revision":1,"status":"approved","action":"approve_export","approved_pages":[0]}"#,
        )
        .unwrap();
        let exports = dir.path().join("exports");
        let export =
            crate::export::export_project_to(&first.job_dir, "zip", Some(&exports)).unwrap();
        assert!(
            Path::new(export["output_path"].as_str().unwrap())
                .starts_with(fs::canonicalize(&exports).unwrap())
        );
    }

    #[test]
    fn source_job_allocation_validates_preexisting_collisions_and_extended_ids() {
        let dir = tempdir().unwrap();
        let first_source_dir = dir.path().join("one").join("comic");
        let second_source_dir = dir.path().join("two").join("comic");
        fs::create_dir_all(&first_source_dir).unwrap();
        fs::create_dir_all(&second_source_dir).unwrap();
        let source = first_source_dir.join("page.png");
        RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]))
            .save(&source)
            .unwrap();
        let jobs = dir.path().join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        let source_id = stable_source_id(&canonical_path(&first_source_dir).unwrap()).unwrap();
        assert_eq!(source_id.len(), 16);
        let colliding = jobs.join(format!("comic--{source_id}"));
        fs::create_dir_all(&colliding).unwrap();
        fs::write(
            manifest_path(&colliding),
            serde_json::to_vec(&JobManifest {
                stage: "managed".into(),
                source_dir: canonical_path(&second_source_dir).unwrap(),
                pages: Default::default(),
                expected_pages: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let workflow = Workflow::new(jobs).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        assert_ne!(registration.job_dir, colliding);
        assert!(
            registration
                .job_dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(&format!("--{source_id}-1"))
        );
        let preserved: JobManifest =
            serde_json::from_slice(&fs::read(manifest_path(&colliding)).unwrap()).unwrap();
        assert_eq!(
            preserved.source_dir,
            canonical_path(&second_source_dir).unwrap()
        );
    }

    #[test]
    fn source_job_allocation_is_cross_process_safe() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("nested").join("comic");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("page.png");
        RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]))
            .save(&source)
            .unwrap();
        let jobs = dir.path().join("jobs");
        let left = Workflow::new(jobs.clone()).unwrap();
        let right = Workflow::new(jobs.clone()).unwrap();
        let left_source = source.clone();
        let right_source = source.clone();
        let first = std::thread::spawn(move || left.register_analysis(&left_source, None));
        let second = std::thread::spawn(move || right.register_analysis(&right_source, None));
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();
        assert_eq!(first.job_dir, second.job_dir);
        let jobs = fs::read_dir(jobs)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
            .count();
        assert_eq!(jobs, 1);
    }

    #[test]
    fn source_replacement_fails_closed_in_an_existing_job() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("comic");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("page.png");
        RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]))
            .save(&source)
            .unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        workflow.register_analysis(&source, None).unwrap();
        RgbaImage::from_pixel(4, 4, Rgba([255, 255, 255, 255]))
            .save(&source)
            .unwrap();
        let error = workflow.register_analysis(&source, None).unwrap_err();
        assert!(error.to_string().contains("source image changed"));
    }

    #[test]
    fn legacy_manifest_inventory_preserves_existing_page_records() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("comic");
        fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("page.png");
        RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]))
            .save(&source)
            .unwrap();
        let jobs = dir.path().join("jobs");
        let job = jobs.join("legacy-job");
        fs::create_dir_all(&job).unwrap();
        let cleaned = job.join("cleaned.png");
        let rendered = job.join("rendered.png");
        RgbImage::from_pixel(4, 4, image::Rgb([255, 255, 255]))
            .save(&cleaned)
            .unwrap();
        RgbImage::from_pixel(4, 4, image::Rgb([255, 255, 255]))
            .save(&rendered)
            .unwrap();
        let source = canonical_path(&source).unwrap();
        let manifest = JobManifest {
            stage: "rendered".into(),
            source_dir: canonical_path(&source_dir).unwrap(),
            pages: [(
                page_key(&source),
                PageManifest {
                    source_image: source.clone(),
                    state: "rendered".into(),
                    source_sha256: String::new(),
                    cleaned_image: Some(cleaned.clone()),
                    rendered_image: Some(rendered.clone()),
                },
            )]
            .into_iter()
            .collect(),
            expected_pages: Vec::new(),
        };
        fs::write(
            legacy_manifest_path(&job),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let workflow = Workflow::new(jobs).unwrap();
        workflow.register_analysis(&source, None).unwrap();
        let saved: JobManifest =
            serde_json::from_slice(&fs::read(legacy_manifest_path(&job)).unwrap()).unwrap();
        let page = saved.pages.get(&page_key(&source)).unwrap();
        assert_eq!(page.state, "rendered");
        assert_eq!(page.cleaned_image.as_deref(), Some(cleaned.as_path()));
        assert_eq!(page.rendered_image.as_deref(), Some(rendered.as_path()));
        assert!(!manifest_path(&job).exists());
    }

    #[test]
    fn next_pending_page_normalizes_legacy_inventory_before_resume() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("comic");
        fs::create_dir_all(&source_dir).unwrap();
        let first = source_dir.join("page-01.png");
        let second = source_dir.join("page-02.png");
        for source in [&first, &second] {
            RgbaImage::from_pixel(4, 4, Rgba([255, 255, 255, 255]))
                .save(source)
                .unwrap();
        }
        let first = canonical_path(&first).unwrap();
        let second = canonical_path(&second).unwrap();
        let source_dir = canonical_path(&source_dir).unwrap();
        let jobs = dir.path().join("jobs");
        let job = jobs.join("legacy-job");
        fs::create_dir_all(&job).unwrap();
        let pages = [
            (
                page_key(&first),
                PageManifest {
                    source_image: first.clone(),
                    state: "analyzed".into(),
                    source_sha256: String::new(),
                    cleaned_image: None,
                    rendered_image: None,
                },
            ),
            (
                page_key(&second),
                PageManifest {
                    source_image: second,
                    state: "analyzed".into(),
                    source_sha256: String::new(),
                    cleaned_image: None,
                    rendered_image: None,
                },
            ),
        ]
        .into_iter()
        .collect();
        fs::write(
            legacy_manifest_path(&job),
            serde_json::to_vec_pretty(&JobManifest {
                stage: "managed".into(),
                source_dir,
                pages,
                expected_pages: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let workflow = Workflow::new(jobs).unwrap();
        let pending = workflow.next_pending_page(&job).unwrap().unwrap();
        assert_eq!(pending.page_number, 1);
        assert_eq!(pending.total_pages, 2);
        assert!(pending.analysis_path.ends_with("pages/0001/analysis.json"));
        let saved: JobManifest =
            serde_json::from_slice(&fs::read(legacy_manifest_path(&job)).unwrap()).unwrap();
        assert_eq!(saved.expected_pages.len(), 2);
        assert!(job.join("pages/0001").is_dir());
        assert!(job.join("pages/0002").is_dir());
    }

    #[test]
    fn generated_bubble_ids_are_stable_and_unique() {
        let first = json!({
            "request_bubbles": [
                {"bbox":{"x1":1,"y1":1,"x2":4,"y2":4},"text":"same"},
                {"bbox":{"x1":5,"y1":5,"x2":8,"y2":8},"text":"other"},
                {"bbox":{"x1":1,"y1":1,"x2":4,"y2":4},"text":"same"}
            ]
        });
        let reordered = json!({
            "request_bubbles": [
                {"bbox":{"x1":5,"y1":5,"x2":8,"y2":8},"text":"other"},
                {"bbox":{"x1":1,"y1":1,"x2":4,"y2":4},"text":"same"},
                {"bbox":{"x1":1,"y1":1,"x2":4,"y2":4},"text":"same"}
            ]
        });
        let first = editor_bubbles(&first, "page");
        let reordered = editor_bubbles(&reordered, "page");
        let ids = first
            .iter()
            .map(|bubble| bubble["id"].as_str().unwrap().to_owned())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 3);
        let other_id = first
            .iter()
            .find(|bubble| bubble["text"] == "other")
            .unwrap()["id"]
            .clone();
        assert_eq!(
            reordered
                .iter()
                .find(|bubble| bubble["text"] == "other")
                .unwrap()["id"],
            other_id
        );
        let first_same = first
            .iter()
            .filter(|bubble| bubble["text"] == "same")
            .map(|bubble| bubble["id"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();
        let reordered_same = reordered
            .iter()
            .filter(|bubble| bubble["text"] == "same")
            .map(|bubble| bubble["id"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(first_same, reordered_same);
        let translated = editor_bubbles(
            &json!({
                "request_bubbles": [{
                    "bbox":{"x1":1,"y1":1,"x2":4,"y2":4},
                    "text":"changed translation"
                }]
            }),
            "page",
        );
        assert!(first_same.contains(translated[0]["id"].as_str().unwrap()));
    }

    #[test]
    fn editor_bubbles_keep_needs_review_advisory_without_creating_a_flag() {
        let advisory = editor_bubbles(
            &json!({
                "request_bubbles": [{"id":"b1","bbox":{"x1":1,"y1":1,"x2":8,"y2":8},"text":"dịch","needs_review":true}]
            }),
            "page-1",
        );
        assert_eq!(advisory[0]["needs_review"], true);
        assert!(advisory[0].get("flagged").is_none());

        let cleared = editor_bubbles(
            &json!({
                "request_bubbles": [{"id":"b1","bbox":{"x1":1,"y1":1,"x2":8,"y2":8},"text":"dịch","needs_review":true,"flagged":false}]
            }),
            "page-1",
        );
        assert_eq!(cleared[0]["flagged"], false);
    }

    #[test]
    fn editor_bubbles_round_trip_the_reported_full_fallback_candidate_list() {
        let bubbles = editor_bubbles(
            &json!({
                "request_bubbles": [{
                    "id": "b1",
                    "bbox": {"x1":1,"y1":1,"x2":8,"y2":8},
                    "text": "Hello",
                    "text_color": "white"
                }],
                "report": {"bubbles": [{
                    "fallback_font_paths": ["primary-used.ttf", "symbol-unused.ttf"],
                    "fallback_fonts_used": ["primary-used.ttf"],
                    "resolved_text_color": "white",
                    "sampled_luminance": 23
                }]}
            }),
            "page-1",
        );
        assert_eq!(
            bubbles[0]["fallback_font_paths"],
            json!(["primary-used.ttf", "symbol-unused.ttf"])
        );
        assert_eq!(bubbles[0]["text_color"], "white");
        assert_eq!(bubbles[0]["resolved_text_color"], "white");
        assert_eq!(bubbles[0]["sampled_luminance"], 23);
    }

    #[test]
    fn editor_bubbles_preserve_native_layout_override_markers() {
        let bubbles = editor_bubbles(
            &json!({
                "request_bubbles": [{
                    "id": "b1",
                    "bbox": {"x1":1,"y1":1,"x2":8,"y2":8},
                    "text": "Hello",
                    "font_size": 18.0,
                    "padding": 6.0,
                    "_editor_render_payload": true,
                    "_editor_font_size_override": 18.0,
                    "_editor_padding_override": 6.0
                }],
                "report": {"bubbles": [{
                    "input_bbox": {"x1":1,"y1":1,"x2":8,"y2":8},
                    "font_size": 11.5,
                    "padding": 2.0
                }]}
            }),
            "page-1",
        );
        assert_eq!(bubbles[0]["_editor_font_size_override"], 18.0);
        assert_eq!(bubbles[0]["_editor_padding_override"], 6.0);
    }

    #[test]
    fn editor_bubbles_keep_unmarked_request_font_settings_with_report_metadata() {
        let bubbles = editor_bubbles(
            &json!({
                "request_bubbles": [{
                    "id": "b1",
                    "bbox": {"x1":1,"y1":1,"x2":8,"y2":8},
                    "text": "Hello",
                    "font_path": "fonts/requested.ttf",
                    "fallback_font_paths": ["fonts/fallback.ttf"],
                    "min_font_size": 12.0,
                    "max_font_size": 36.0,
                    "padding": 6.0,
                    "_editor_render_payload": true
                }],
                "report": {"bubbles": [{
                    "input_bbox": {"x1":1,"y1":1,"x2":8,"y2":8},
                    "font_size": 18.0,
                    "padding": 2.0,
                    "resolved_font_path": "fonts/resolved.ttf",
                    "fallback_font_paths": ["fonts/used.ttf"]
                }]}
            }),
            "page-1",
        );
        assert_eq!(
            bubbles[0]["_editor_requested_fallback_font_paths"],
            json!(["fonts/fallback.ttf"])
        );
        assert_eq!(bubbles[0]["_editor_requested_min_font_size"], 12.0);
        assert_eq!(bubbles[0]["_editor_requested_max_font_size"], 36.0);
        assert_eq!(bubbles[0]["_editor_requested_padding"], 6.0);
    }

    #[test]
    fn render_completeness_rejects_missing_or_unchanged_translation() {
        let dir = tempdir().unwrap();
        let jobs = dir.path().join("jobs");
        let source = dir.path().join("page.png");
        RgbImage::from_pixel(32, 32, image::Rgb([255, 255, 255]))
            .save(&source)
            .unwrap();
        let workflow = Workflow::new(jobs).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        workflow
            .write_analysis_artifact(
                &source,
                &json!({
                    "target_language": "vi",
                    "bubbles": [{"id":"bubble-1","bbox":{"x1":1.0,"y1":1.0,"x2":20.0,"y2":20.0}}],
                    "text_lines": [],
                    "unmatched_text": [],
                    "translation_handoff": {"items": [{
                        "id":"bubble-1",
                        "kind":"dialogue",
                        "source_text":"The character speaks to everyone in the room.",
                        "source_language":"en",
                        "bbox":{"x1":1.0,"y1":1.0,"x2":20.0,"y2":20.0}
                    }]}
                }),
            )
            .unwrap();
        let missing = json!({"request_bubbles": []});
        let error = validate_typeset_completeness(&registration.job_dir, &source, &missing)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no rendered bubble"));
        let unchanged = json!({"request_bubbles": [{
            "id":"bubble-1",
            "text":"The character speaks to everyone in the room."
        }]});
        let error = validate_typeset_completeness(&registration.job_dir, &source, &unchanged)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unchanged English prose"));
        let translated = json!({"request_bubbles": [{
            "id":"bubble-1",
            "text":"Nhân vật nói chuyện với mọi người trong phòng.",
            "preserve_source": false
        }]});
        validate_typeset_completeness(&registration.job_dir, &source, &translated).unwrap();

        workflow
            .write_analysis_artifact(
                &source,
                &json!({
                    "target_language": "vi",
                    "bubbles": [],
                    "text_lines": [{
                        "text":"This legacy prose line was detected outside a bubble.",
                        "source_language":"en",
                        "confidence":0.98,
                        "bbox":{"x1":1.0,"y1":1.0,"x2":30.0,"y2":8.0}
                    }],
                    "unmatched_text": [],
                    "translation_handoff": {"items": []}
                }),
            )
            .unwrap();
        let error = validate_typeset_completeness(
            &registration.job_dir,
            &source,
            &json!({"request_bubbles": []}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no translated or explicitly preserved item"));
        validate_typeset_completeness(
            &registration.job_dir,
            &source,
            &json!({"request_bubbles": [{
                "id":"manual-prose",
                "source_text":"This legacy prose line was detected outside a bubble.",
                "source_language":"en",
                "text":"Dòng văn bản cũ này được phát hiện bên ngoài bong bóng thoại.",
                "bbox":{"x1":1.0,"y1":1.0,"x2":30.0,"y2":8.0}
            }]}),
        )
        .unwrap();
    }

    #[test]
    fn render_completeness_accepts_one_full_afterword_editor_anchor() {
        let dir = tempdir().unwrap();
        let jobs = dir.path().join("jobs");
        let source = dir.path().join("page.png");
        RgbImage::from_pixel(128, 96, image::Rgb([255, 255, 255]))
            .save(&source)
            .unwrap();
        let workflow = Workflow::new(jobs).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        workflow
            .write_analysis_artifact(
                &source,
                &json!({
                    "target_language": "vi",
                    "bubbles": [],
                    "text_lines": [
                        {
                            "text": "Thank you for reading this afterword.",
                            "source_language": "en",
                            "bbox": {"x1": 10.0, "y1": 10.0, "x2": 90.0, "y2": 30.0}
                        },
                        {
                            "text": "I hope you look forward to the next one.",
                            "source_language": "en",
                            "bbox": {"x1": 10.0, "y1": 40.0, "x2": 90.0, "y2": 60.0}
                        }
                    ],
                    "unmatched_text": [],
                    "translation_handoff": {"items": []}
                }),
            )
            .unwrap();

        // A native editor item can be the single source anchor for a prose
        // block whose detector emitted several lines. Its geometry must fully
        // contain each line; source_text may be unavailable for a new item.
        let full_afterword = json!({
            "request_bubbles": [{
                "id": "editor-afterword",
                "bbox": {"x1": 5.0, "y1": 5.0, "x2": 95.0, "y2": 65.0},
                "text_bbox": {"x1": 5.0, "y1": 5.0, "x2": 95.0, "y2": 65.0},
                "source_text": "",
                "text": "Cảm ơn bạn đã đọc phần hậu truyện này. Hy vọng bạn sẽ đón chờ phần tiếp theo."
            }]
        });
        validate_typeset_completeness(&registration.job_dir, &source, &full_afterword).unwrap();

        let incomplete = json!({
            "request_bubbles": [{
                "id": "editor-afterword",
                "bbox": {"x1": 5.0, "y1": 5.0, "x2": 95.0, "y2": 35.0},
                "source_text": "",
                "text": "Cảm ơn bạn đã đọc phần hậu truyện này."
            }]
        });
        let error = validate_typeset_completeness(&registration.job_dir, &source, &incomplete)
            .unwrap_err()
            .to_string();
        assert!(error.contains("detected English prose line 1"));

        let overlap_only = json!({
            "request_bubbles": [{
                "id": "editor-afterword",
                "bbox": {"x1": 5.0, "y1": 25.0, "x2": 95.0, "y2": 50.0},
                "source_text": "",
                "text": "Cảm ơn bạn đã đọc phần hậu truyện này."
            }]
        });
        let error = validate_typeset_completeness(&registration.job_dir, &source, &overlap_only)
            .unwrap_err()
            .to_string();
        assert!(error.contains("detected English prose line 0"));

        let unchanged = json!({
            "request_bubbles": [{
                "id": "editor-afterword",
                "bbox": {"x1": 5.0, "y1": 5.0, "x2": 95.0, "y2": 65.0},
                "source_text": "Thank you for reading this afterword. I hope you look forward to the next one.",
                "text": "Thank you for reading this afterword. I hope you look forward to the next one."
            }]
        });
        let error = validate_typeset_completeness(&registration.job_dir, &source, &unchanged)
            .unwrap_err()
            .to_string();
        assert!(error.contains("detected English prose line 0"));
    }

    #[test]
    fn editor_completeness_reports_all_failed_page_numbers_in_one_pass() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        let page_one = source_dir.join("page-1.png");
        let page_two = source_dir.join("page-2.png");
        for source in [&page_one, &page_two] {
            RgbImage::from_pixel(32, 32, image::Rgb([255, 255, 255]))
                .save(source)
                .unwrap();
        }

        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let registration = workflow.register_analysis(&page_one, None).unwrap();
        workflow.register_analysis(&page_two, None).unwrap();
        for (index, source) in [&page_one, &page_two].into_iter().enumerate() {
            let id = format!("bubble-{}", index + 1);
            workflow
                .write_analysis_artifact(
                    source,
                    &json!({
                        "target_language": "vi",
                        "bubbles": [{"id": id, "bbox": {"x1":1.0,"y1":1.0,"x2":20.0,"y2":20.0}}],
                        "text_lines": [],
                        "unmatched_text": [],
                        "translation_handoff": {"items": [{
                            "id": id,
                            "kind": "dialogue",
                            "source_text": "The character speaks to everyone in the room.",
                            "source_language": "en",
                            "bbox": {"x1":1.0,"y1":1.0,"x2":20.0,"y2":20.0}
                        }]}
                    }),
                )
                .unwrap();
            let cleaned = RgbImage::from_pixel(32, 32, image::Rgb([240, 240, 240]));
            let mut mask = GrayImage::new(32, 32);
            mask.put_pixel(2, 2, image::Luma([255]));
            workflow
                .write_clean_artifact(source, &cleaned, &mask, 0, "crop")
                .unwrap();
            let rendered = workflow.page_artifacts_for_source(source).unwrap().4;
            cleaned.save(&rendered).unwrap();
            let clean_path = workflow.page_artifacts_for_source(source).unwrap().2;
            workflow
                .register_render(
                    &rendered,
                    &workflow.validate_clean_input(&clean_path).unwrap(),
                    json!({
                        "request_bubbles": [{
                            "id": id,
                            "source_text": "The character speaks to everyone in the room.",
                            "source_language": "en",
                            "text": "Nhân vật nói chuyện với mọi người trong phòng.",
                            "bbox": {"x1":1.0,"y1":1.0,"x2":20.0,"y2":20.0}
                        }]
                    }),
                    json!({}),
                )
                .unwrap();

            // Simulate two already-rendered legacy pages that each need a
            // repair. Approval must report both in one response.
            let sidecar = render_sidecar(&rendered);
            let mut render: serde_json::Value =
                serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
            render["typeset"] = json!({"request_bubbles": []});
            fs::write(&sidecar, serde_json::to_vec(&render).unwrap()).unwrap();
        }

        let error = workflow
            .validate_editor_completeness(
                &registration.job_dir,
                &json!({
                    "pages": [{}, {}]
                }),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("2 page(s)"), "unexpected error: {error}");
        assert!(error.contains("page 1"), "unexpected error: {error}");
        assert!(error.contains("page 2"), "unexpected error: {error}");
        assert_eq!(error.matches("no rendered bubble").count(), 2);
    }

    #[test]
    fn managed_job_selector_rejects_traversal_and_accepts_job_directory() {
        let dir = tempdir().unwrap();
        let jobs = dir.path().join("jobs");
        let job = jobs.join("1b5c2f14ee4a4b469353b3786fdf1025");
        fs::create_dir_all(&job).unwrap();
        fs::write(
            manifest_path(&job),
            serde_json::to_vec(&JobManifest {
                stage: "rendered".into(),
                source_dir: dir.path().into(),
                pages: Default::default(),
                expected_pages: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let workflow = Workflow::new(jobs.clone()).unwrap();
        assert_eq!(
            workflow
                .resolve_managed_job_id("1b5c2f14ee4a4b469353b3786fdf1025")
                .unwrap(),
            canonical_path(&job).unwrap()
        );
        assert_eq!(
            workflow
                .resolve_managed_job_path(&manifest_path(&job).to_string_lossy())
                .unwrap(),
            canonical_path(&job).unwrap()
        );
        assert!(workflow.resolve_managed_job_path("../").is_err());
        assert!(workflow.resolve_managed_job_id("..\\secret").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn owned_output_creation_rejects_symlink_escape_before_mkdir() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let jobs = dir.path().join("jobs");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let workflow = Workflow::new(jobs.clone()).unwrap();
        symlink(&outside, jobs.join("escape")).unwrap();

        let output = jobs.join("escape").join("created.json");
        assert!(
            workflow
                .require_output_owned(&output, "test output")
                .is_err()
        );
        assert!(!outside.join("created.json").exists());
    }

    #[cfg(windows)]
    #[test]
    fn clean_artifact_accepts_normal_and_verbatim_windows_paths() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("page.png");
        let mut source_image = RgbaImage::from_pixel(8, 8, Rgba([255, 255, 255, 255]));
        source_image.put_pixel(3, 3, Rgba([0, 0, 0, 255]));
        source_image.save(&source).unwrap();
        let cleaned = RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 255]));
        let mut mask = GrayImage::new(8, 8);
        mask.put_pixel(3, 3, image::Luma([255]));
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let (cleaned_path, _, _) = workflow
            .write_clean_artifact(&source, &cleaned, &mask, 3, "crop")
            .unwrap();
        let verbatim_cleaned = fs::canonicalize(&cleaned_path).unwrap();
        workflow.validate_clean_input(&cleaned_path).unwrap();
        workflow.validate_clean_input(&verbatim_cleaned).unwrap();

        let sidecar_path = clean_sidecar(&cleaned_path);
        let mut sidecar: serde_json::Value =
            serde_json::from_slice(&fs::read(&sidecar_path).unwrap()).unwrap();
        sidecar["source_image"] = serde_json::json!(fs::canonicalize(&source).unwrap());
        sidecar["cleaned_image"] = serde_json::json!(verbatim_cleaned);
        sidecar["mask_path"] =
            serde_json::json!(fs::canonicalize(sidecar["mask_path"].as_str().unwrap()).unwrap());
        fs::write(&sidecar_path, serde_json::to_vec_pretty(&sidecar).unwrap()).unwrap();
        workflow.validate_clean_input(&cleaned_path).unwrap();
    }

    #[test]
    fn editor_state_synthesizes_legacy_pages_and_empty_bubbles() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        for name in ["page-1.png", "page-2.png"] {
            let mut source = RgbaImage::from_pixel(8, 8, Rgba([255, 255, 255, 255]));
            source.put_pixel(3, 3, Rgba([0, 0, 0, 255]));
            source.save(source_dir.join(name)).unwrap();
        }
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let page_one = source_dir.join("page-1.png");
        let page_two = source_dir.join("page-2.png");
        let first = workflow.register_analysis(&page_one, None).unwrap();
        workflow.register_analysis(&page_two, None).unwrap();
        // Exercise the legacy flat-artifact branch explicitly. Existing jobs
        // keep their marker and may continue to store renders at the job root.
        fs::rename(
            manifest_path(&first.job_dir),
            legacy_manifest_path(&first.job_dir),
        )
        .unwrap();
        let mut rendered_paths = Vec::new();
        for source in [&page_one, &page_two] {
            let cleaned = RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 255]));
            let mut mask = GrayImage::new(8, 8);
            mask.put_pixel(3, 3, image::Luma([255]));
            let (cleaned_path, _, _) = workflow
                .write_clean_artifact(source, &cleaned, &mask, 3, "crop")
                .unwrap();
            let rendered = first
                .job_dir
                .join(format!("legacy-{}.png", page_key(source)));
            cleaned.save(&rendered).unwrap();
            workflow
                .register_render(
                    &rendered,
                    &workflow.validate_clean_input(&cleaned_path).unwrap(),
                    serde_json::Value::Null,
                    json!({}),
                )
                .unwrap();
            rendered_paths.push(rendered);
        }
        let state = workflow.editor_state(&rendered_paths[0], None).unwrap();
        let pages = state["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 2);
        assert!(pages.iter().all(|page| page["bubbles"].is_array()));
        assert!(
            pages
                .iter()
                .all(|page| page["rendered_image_path"].is_string())
        );
        let reopened_job = workflow
            .resolve_managed_job_path(&first.job_dir.to_string_lossy())
            .unwrap();
        let selected = workflow
            .verified_render_for_job(&reopened_job, None)
            .unwrap();
        assert!(selected.is_file());
    }

    #[test]
    fn editor_render_plan_reuses_clean_sidecars_and_selects_changed_pages() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("comic");
        fs::create_dir_all(&source_dir).unwrap();
        let first_source = source_dir.join("page-01.png");
        let second_source = source_dir.join("page-02.png");
        for source in [&first_source, &second_source] {
            let mut image = RgbaImage::from_pixel(8, 8, Rgba([255, 255, 255, 255]));
            image.put_pixel(3, 3, Rgba([0, 0, 0, 255]));
            image.save(source).unwrap();
        }

        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let registration = workflow.register_analysis(&first_source, None).unwrap();
        workflow.register_analysis(&second_source, None).unwrap();
        for source in [&first_source, &second_source] {
            let cleaned = RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 255]));
            let mut mask = GrayImage::new(8, 8);
            mask.put_pixel(3, 3, image::Luma([255]));
            let (cleaned_path, _, _) = workflow
                .write_clean_artifact(source, &cleaned, &mask, 1, "full")
                .unwrap();
            let rendered_path = workflow.page_artifacts_for_source(source).unwrap().4;
            cleaned.save(&rendered_path).unwrap();
            let clean = workflow.validate_clean_input(&cleaned_path).unwrap();
            workflow
                .register_render(&rendered_path, &clean, json!({}), json!({}))
                .unwrap();
        }

        let rendered = workflow.page_artifacts_for_source(&first_source).unwrap().4;
        let mut state = workflow.editor_state(&rendered, None).unwrap();
        let supplied = json!({
            "font_path": "fonts/new-global.ttf",
            "_editor_requested_global_font_path": "fonts/old-global.ttf"
        });
        let supplied_state = workflow.editor_state(&rendered, Some(&supplied)).unwrap();
        assert_eq!(supplied_state["font_path"], "fonts/new-global.ttf");
        assert_eq!(
            supplied_state["_editor_requested_global_font_path"],
            "fonts/new-global.ttf"
        );
        state["pages"][0]["render_dirty"] = json!(true);
        assert!(
            workflow
                .editor_render_plan(&registration.job_dir, &state)
                .unwrap()
                .is_empty()
        );

        state["pages"][0]["correction_strokes"] = json!([{
            "mode": "cover",
            "size": 4,
            "points": [{"x": 2, "y": 2}]
        }]);
        assert_eq!(
            workflow
                .editor_render_plan(&registration.job_dir, &state)
                .unwrap(),
            vec![0]
        );

        let second_rendered = workflow
            .page_artifacts_for_source(&second_source)
            .unwrap()
            .4;
        fs::remove_file(second_rendered).unwrap();
        assert_eq!(
            workflow
                .editor_render_plan(&registration.job_dir, &state)
                .unwrap(),
            vec![0, 1]
        );
    }

    #[test]
    fn editor_render_plan_ignores_legacy_report_layout_across_many_pages() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("comic");
        fs::create_dir_all(&source_dir).unwrap();
        let names = [
            "page-10.png",
            "page-2.png",
            "page-1.png",
            "page-3.png",
            "page-4.png",
            "page-5.png",
            "page-6.png",
            "page-7.png",
        ];
        let sources = names
            .iter()
            .map(|name| source_dir.join(name))
            .collect::<Vec<_>>();
        for source in &sources {
            let mut image = RgbaImage::from_pixel(16, 16, Rgba([255, 255, 255, 255]));
            image.put_pixel(3, 3, Rgba([0, 0, 0, 255]));
            image.save(source).unwrap();
        }

        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let registration = workflow.register_analysis(&sources[0], None).unwrap();
        for source in sources.iter().skip(1) {
            workflow.register_analysis(source, None).unwrap();
        }
        for (index, source) in sources.iter().enumerate() {
            let cleaned = RgbImage::from_pixel(16, 16, image::Rgb([255, 255, 255]));
            let mut mask = GrayImage::new(16, 16);
            mask.put_pixel(3, 3, image::Luma([255]));
            let (cleaned_path, _, _) = workflow
                .write_clean_artifact(source, &cleaned, &mask, 1, "full")
                .unwrap();
            let rendered_path = workflow.page_artifacts_for_source(source).unwrap().4;
            cleaned.save(&rendered_path).unwrap();
            let request = json!({
                "id": format!("bubble-{index}"),
                "source_text": "OCR source",
                "kind": "dialogue",
                "preserve_source": false,
                "preserve_by_default": false,
                "bbox": {"x1": 1.0, "y1": 1.0, "x2": 14.0, "y2": 14.0},
                "text": "Bản dịch",
                "font_path": "fonts/requested.ttf",
                "min_font_size": 8.0,
                "max_font_size": 72.0,
                "shape": "ellipse"
            });
            let report_bubble = json!({
                "index": 0,
                "input_bbox": request["bbox"],
                "bubble_bbox": request["bbox"],
                "text_bbox": serde_json::Value::Null,
                "safe_bbox": {"x1": 2.0, "y1": 2.0, "x2": 13.0, "y2": 13.0},
                "safe_mask_bbox": {"x1": 2.0, "y1": 2.0, "x2": 13.0, "y2": 13.0},
                "padding": 6.0,
                "font_size": 23.5,
                "lines": ["Bản dịch"],
                "line_count": 1,
                "placement_center": {"x": 8.0, "y": 8.0},
                "ink_bbox": {"x1": 4.0, "y1": 4.0, "x2": 10.0, "y2": 10.0},
                "requested_font_path": "fonts/requested.ttf",
                "requested_primary_font": "fonts/requested.ttf",
                "font_path": "fonts/requested.ttf",
                "resolved_font_path": "fonts/resolved.ttf",
                "fallback_font_paths": ["fonts/resolved.ttf"],
                "fallback_fonts_used": ["fonts/resolved.ttf"],
                "font_runs": [{"font_index": 1, "text": "Bản dịch"}],
                "resolved_text_color": "black",
                "sampled_luminance": 220,
                "shape": "ellipse"
            });
            workflow
                .register_render(
                    &rendered_path,
                    &workflow.validate_clean_input(&cleaned_path).unwrap(),
                    json!({
                        "request_bubbles": [request],
                        "report": {"bubbles": [report_bubble]}
                    }),
                    json!({
                        "status": "pass",
                        "correction_strokes": [{
                            "mode": "cover",
                            "size": 4,
                            "points": [{"x": 2, "y": 2}]
                        }]
                    }),
                )
                .unwrap();
        }

        let rendered = workflow.page_artifacts_for_source(&sources[0]).unwrap().4;
        let mut state = workflow.editor_state(&rendered, None).unwrap();
        for page in state["pages"].as_array_mut().unwrap() {
            let bubble = page["bubbles"][0].as_object_mut().unwrap();
            // Model a legacy project snapshot: fitted values and report
            // metadata were serialized with different renderer versions.
            bubble.insert("font_size".into(), json!(11.5));
            bubble.insert("padding".into(), json!(2.0));
            bubble.insert("font_path".into(), json!("fonts/legacy-resolved.ttf"));
            bubble.insert("rendered_font_path".into(), json!("fonts/resolved.ttf"));
            bubble.insert("min_font_size".into(), json!(11.5));
            bubble.insert("max_font_size".into(), json!(11.5));
            bubble.insert(
                "safe_bbox".into(),
                json!({"x1": 99, "y1": 99, "x2": 100, "y2": 100}),
            );
            page["render_dirty"] = json!(true);
            page["typeset_warnings"] = json!([{"reason": "legacy-report"}]);
        }
        assert!(
            workflow
                .editor_render_plan(&registration.job_dir, &state)
                .unwrap()
                .is_empty()
        );

        state["pages"][3]["bubbles"][0]["translation"] = json!("One real edit");
        assert_eq!(
            workflow
                .editor_render_plan(&registration.job_dir, &state)
                .unwrap(),
            vec![3]
        );

        state["pages"][3]["bubbles"][0]["translation"] = json!("Bản dịch");
        state["pages"][3]["correction_strokes"][0]["points"][0]["x"] = json!(3);
        assert_eq!(
            workflow
                .editor_render_plan(&registration.job_dir, &state)
                .unwrap(),
            vec![3]
        );
    }
}
