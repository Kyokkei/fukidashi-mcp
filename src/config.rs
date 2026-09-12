use std::{
    env, fs,
    path::{Path, PathBuf},
};

use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::error::{FukidashiError, Result};

pub const ENV_STORAGE_ROOT: &str = "FUKIDASHI_STORAGE_ROOT";
pub const ENV_MODELS: &str = "FUKIDASHI_MODELS_DIR";
pub const ENV_JOBS_ROOT: &str = "FUKIDASHI_JOBS_ROOT";
pub const ENV_JOBS_DIR: &str = "FUKIDASHI_JOBS_DIR";
pub const ENV_CACHE_DIR: &str = "FUKIDASHI_CACHE_DIR";
pub const ENV_TEMP_DIR: &str = "FUKIDASHI_TEMP_DIR";
pub const ENV_RUNTIME_DIR: &str = "FUKIDASHI_RUNTIME_DIR";
pub const ENV_EXPORTS_DIR: &str = "FUKIDASHI_EXPORTS_DIR";
pub const ENV_ORT: &str = "ORT_DYLIB_PATH";
pub const ENV_ORT_GPU_DEPS: &str = "ORT_GPU_DEPS_DIR";
pub const ENV_PROVIDER: &str = "FUKIDASHI_PROVIDER";
pub const ENV_TARGET_LANGUAGE: &str = "FUKIDASHI_TARGET_LANGUAGE";
pub const ENV_SESSION_RECYCLE_PAGES: &str = "FUKIDASHI_SESSION_RECYCLE_PAGES";
pub const ENV_GPU_MEMORY_LIMIT_MIB: &str = "FUKIDASHI_GPU_MEMORY_LIMIT_MIB";
pub const ENV_RETAIN_STAGE_SESSIONS: &str = "FUKIDASHI_RETAIN_STAGE_SESSIONS";
pub const ENV_FONT_DIRS: &str = "FUKIDASHI_FONT_DIRS";
pub const ENV_FONT_PATH: &str = "FUKIDASHI_FONT_PATH";
/// Optional absolute path to a provisioned gallery-dl executable.
pub const ENV_GALLERY_DL: &str = "FUKIDASHI_GALLERY_DL";

const DEFAULT_SESSION_RECYCLE_PAGES: usize = 1;
const DEFAULT_GPU_MEMORY_LIMIT_MIB: usize = 2_048;

#[derive(Debug, Clone, Args)]
pub struct ConfigArgs {
    /// Root containing separately provisioned ONNX models.
    #[arg(long)]
    pub models_dir: Option<PathBuf>,
    /// Optional absolute ONNX Runtime dynamic library.
    #[arg(long)]
    pub ort_dylib: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub storage_root: PathBuf,
    pub models_dir: PathBuf,
    pub ort_dylib: Option<PathBuf>,
    pub config_file: PathBuf,
    pub configured_jobs_dir: Option<PathBuf>,
    pub configured_cache_dir: Option<PathBuf>,
    pub configured_temp_dir: Option<PathBuf>,
    pub configured_runtime_dir: Option<PathBuf>,
    pub configured_exports_dir: Option<PathBuf>,
    pub configured_font_dirs: Vec<PathBuf>,
    pub configured_provider: Option<String>,
    pub storage_source: String,
    pub models_source: String,
    pub ort_source: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UserConfig {
    storage_root: Option<PathBuf>,
    models_dir: Option<PathBuf>,
    jobs_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    temp_dir: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    exports_dir: Option<PathBuf>,
    #[serde(default)]
    font_dirs: Vec<PathBuf>,
    provider: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigReport {
    pub config_file: PathBuf,
    pub storage_root: PathBuf,
    pub storage_source: String,
    pub models_dir: PathBuf,
    pub models_source: String,
    pub jobs_dir: PathBuf,
    pub jobs_source: String,
    pub cache_dir: PathBuf,
    pub cache_source: String,
    pub temp_dir: PathBuf,
    pub temp_source: String,
    pub runtime_dir: PathBuf,
    pub runtime_source: String,
    pub exports_dir: PathBuf,
    pub exports_source: String,
    pub font_dirs: Vec<PathBuf>,
    pub font_source: String,
    pub provider: String,
    pub provider_source: String,
    pub restart_required: bool,
    pub restart_fields: Vec<String>,
    pub missing_model_files: Vec<PathBuf>,
    pub runtime_library: Option<PathBuf>,
    pub gallery_dl_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ConfigureRequest {
    /// One root from which models/, jobs/, cache/, runtime/, fonts/ and exports/ are derived.
    pub storage_root: Option<String>,
    /// Advanced override. Prefer storage_root for normal setup.
    pub models_dir: Option<String>,
    /// Advanced override for generated job artifacts.
    pub jobs_dir: Option<String>,
    /// Advanced override for cache and temporary files.
    pub cache_dir: Option<String>,
    /// Advanced override for short-lived temporary files.
    pub temp_dir: Option<String>,
    /// Advanced override for native runtime libraries.
    pub runtime_dir: Option<String>,
    /// Advanced override for exported archives and HTML files.
    pub exports_dir: Option<String>,
    /// Additional directories containing approved font files.
    #[serde(default)]
    pub font_dirs: Vec<String>,
    /// cpu (default), auto, or cuda.
    pub provider: Option<String>,
}

impl Config {
    pub fn resolve(args: &ConfigArgs) -> Result<Self> {
        Self::resolve_from(args, platform_config_file())
    }

    fn resolve_from(args: &ConfigArgs, config_file: PathBuf) -> Result<Self> {
        let persisted = load_user_config(&config_file)?;
        let (storage_root, storage_source) = if let Some(path) = env_path(ENV_STORAGE_ROOT) {
            (path, "environment".to_owned())
        } else if let Some(path) = persisted.storage_root.clone() {
            (path, "user-config".to_owned())
        } else if let Some(path) = platform_storage_root() {
            (path, "platform-default".to_owned())
        } else {
            return Err(FukidashiError::InvalidInput(
                "cannot determine storage root".into(),
            ));
        };
        let storage_root = absolute(&storage_root)?;
        let (models, models_source) = if let Some(path) = args.models_dir.clone() {
            (path, "cli".to_owned())
        } else if let Some(path) = env_path(ENV_MODELS) {
            (path, "environment".to_owned())
        } else if let Some(path) = persisted.models_dir.clone() {
            (path, "user-config".to_owned())
        } else {
            (storage_root.join("models"), "storage-default".to_owned())
        };
        let models_dir = absolute(&models)?;
        let ort_candidate = args
            .ort_dylib
            .clone()
            .map(|p| (Some(p), "cli".to_owned()))
            .or_else(|| env_path(ENV_ORT).map(|p| (Some(p), "environment".to_owned())))
            .or_else(|| {
                env_path(ENV_RUNTIME_DIR).and_then(|p| {
                    discover_ort_library_in(&p)
                        .map(|library| (Some(library), "environment-runtime".to_owned()))
                })
            })
            .or_else(|| {
                persisted
                    .runtime_dir
                    .as_deref()
                    .and_then(discover_ort_library_in)
                    .map(|p| (Some(p), "user-config-runtime".to_owned()))
            })
            .or_else(|| {
                discover_ort_library_in(&storage_root.join("runtime"))
                    .map(|p| (Some(p), "storage-runtime-discovery".to_owned()))
            })
            .or_else(|| discover_ort_library().map(|p| (Some(p), "platform-discovery".to_owned())))
            .unwrap_or((None, "unconfigured".to_owned()));
        let ort_dylib = ort_candidate.0.map(|path| absolute(&path)).transpose()?;
        Ok(Self {
            storage_root,
            models_dir,
            ort_dylib,
            config_file,
            configured_jobs_dir: persisted.jobs_dir.map(|path| absolute(&path)).transpose()?,
            configured_cache_dir: persisted
                .cache_dir
                .map(|path| absolute(&path))
                .transpose()?,
            configured_temp_dir: persisted.temp_dir.map(|path| absolute(&path)).transpose()?,
            configured_runtime_dir: persisted
                .runtime_dir
                .map(|path| absolute(&path))
                .transpose()?,
            configured_exports_dir: persisted
                .exports_dir
                .map(|path| absolute(&path))
                .transpose()?,
            configured_font_dirs: persisted
                .font_dirs
                .into_iter()
                .map(|path| absolute(&path))
                .collect::<Result<Vec<_>>>()?,
            configured_provider: persisted.provider.as_deref().and_then(normalize_provider),
            storage_source,
            models_source,
            ort_source: ort_candidate.1,
        })
    }

    pub fn detector_model(&self) -> PathBuf {
        self.models_dir.join("detection/detector-v4-s_int8.onnx")
    }
    pub fn lama_model(&self) -> PathBuf {
        self.models_dir.join("inpainting/lama-manga-dynamic.onnx")
    }
    pub fn db_model(&self) -> PathBuf {
        self.models_dir
            .join("ocr/ppocr-v5-onnx/ch_PP-OCRv5_mobile_det.onnx")
    }
    pub fn ocr_encoder_model(&self) -> PathBuf {
        self.models_dir
            .join("ocr/manga-ocr-mobile-onnx/encoder.onnx")
    }
    pub fn ocr_init_model(&self) -> PathBuf {
        self.models_dir
            .join("ocr/manga-ocr-mobile-onnx/decoder_init.onnx")
    }
    pub fn ocr_step_model(&self) -> PathBuf {
        self.models_dir
            .join("ocr/manga-ocr-mobile-onnx/decoder_step.onnx")
    }
    pub fn ocr_vocab(&self) -> PathBuf {
        self.models_dir.join("ocr/manga-ocr-mobile-onnx/vocab.txt")
    }
    pub fn english_model(&self) -> PathBuf {
        self.models_dir
            .join("ocr/ppocr-v5-onnx/en_PP-OCRv5_rec_mobile_infer.onnx")
    }
    pub fn english_dictionary(&self) -> PathBuf {
        self.models_dir
            .join("ocr/ppocr-v5-onnx/ppocrv5_en_dict.txt")
    }
    pub fn baberu_dir(&self) -> PathBuf {
        self.models_dir.join("ocr/baberu-ocr")
    }
    pub fn baberu_vision_model(&self) -> PathBuf {
        self.baberu_dir().join("vision_int4.onnx")
    }
    pub fn baberu_prefill_model(&self) -> PathBuf {
        self.baberu_dir().join("decoder_prefill_int8.onnx")
    }
    pub fn baberu_step_model(&self) -> PathBuf {
        self.baberu_dir().join("decoder_step_int8.onnx")
    }
    pub fn baberu_vocab(&self) -> PathBuf {
        self.baberu_dir().join("vocab.json")
    }
    pub fn baberu_tokenizer_config(&self) -> PathBuf {
        self.baberu_dir().join("tokenizer_config.json")
    }

    pub fn jobs_dir(&self) -> PathBuf {
        env_path(ENV_JOBS_ROOT)
            .or_else(|| env_path(ENV_JOBS_DIR))
            .or_else(|| self.configured_jobs_dir.clone())
            .map(|path| absolute_lossy(&path))
            .unwrap_or_else(|| self.storage_root.join("jobs"))
    }
    pub fn cache_dir(&self) -> PathBuf {
        env_path(ENV_CACHE_DIR)
            .or_else(|| self.configured_cache_dir.clone())
            .map(|path| absolute_lossy(&path))
            .unwrap_or_else(|| self.storage_root.join("cache"))
    }
    pub fn temp_dir(&self) -> PathBuf {
        env_path(ENV_TEMP_DIR)
            .or_else(|| self.configured_temp_dir.clone())
            .map(|path| absolute_lossy(&path))
            .unwrap_or_else(|| self.storage_root.join("temp"))
    }
    pub fn runtime_dir(&self) -> PathBuf {
        env_path(ENV_RUNTIME_DIR)
            .or_else(|| self.configured_runtime_dir.clone())
            .map(|path| absolute_lossy(&path))
            .unwrap_or_else(|| self.storage_root.join("runtime"))
    }
    pub fn exports_dir(&self) -> PathBuf {
        env_path(ENV_EXPORTS_DIR)
            .or_else(|| self.configured_exports_dir.clone())
            .map(|path| absolute_lossy(&path))
            .unwrap_or_else(|| self.storage_root.join("exports"))
    }
    pub fn font_dirs(&self) -> Vec<PathBuf> {
        let mut result = vec![self.storage_root.join("fonts")];
        result.extend(font_dirs_from_env());
        result.extend(self.configured_font_dirs.iter().cloned());
        result.extend(platform_font_dirs());
        deduplicate_paths(&mut result);
        result
    }
    pub fn provider(&self) -> String {
        env::var(ENV_PROVIDER)
            .ok()
            .and_then(|v| normalize_provider(&v))
            .or_else(|| self.configured_provider.clone())
            .unwrap_or_else(|| "cpu".to_owned())
    }
    pub fn prefer_gpu(&self) -> bool {
        matches!(self.provider().as_str(), "auto" | "cuda" | "gpu")
    }
    pub fn ort_gpu_dependencies_dir(&self) -> PathBuf {
        env::var_os(ENV_ORT_GPU_DEPS)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.runtime_dir().join("gpu-deps"))
    }
    pub fn configured_target_language(&self) -> String {
        env::var(ENV_TARGET_LANGUAGE)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "vi".to_owned())
    }
    pub fn session_recycle_pages(&self) -> usize {
        env_usize(ENV_SESSION_RECYCLE_PAGES, DEFAULT_SESSION_RECYCLE_PAGES).min(10_000)
    }
    pub fn gpu_memory_limit_bytes(&self) -> usize {
        env_usize(ENV_GPU_MEMORY_LIMIT_MIB, DEFAULT_GPU_MEMORY_LIMIT_MIB)
            .saturating_mul(1024 * 1024)
    }
    pub fn retain_stage_sessions(&self) -> bool {
        env::var(ENV_RETAIN_STAGE_SESSIONS)
            .ok()
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
    }

    /// Materialize the configured runtime namespaces before starting work.
    /// Atomic artifact writers still stage beside their final destination so
    /// replacement remains atomic even when temp_dir is on another volume.
    pub fn ensure_runtime_dirs(&self) -> Result<()> {
        for path in [
            self.cache_dir(),
            self.temp_dir(),
            self.runtime_dir(),
            self.exports_dir(),
        ] {
            fs::create_dir_all(path)?;
        }
        Ok(())
    }

    pub fn config_report(&self) -> ConfigReport {
        let missing_model_files = self
            .doctor()
            .into_iter()
            .filter_map(|line| line.strip_suffix(": missing").map(PathBuf::from))
            .collect();
        ConfigReport {
            config_file: self.config_file.clone(),
            storage_root: self.storage_root.clone(),
            storage_source: self.storage_source.clone(),
            models_dir: self.models_dir.clone(),
            models_source: self.models_source.clone(),
            jobs_dir: self.jobs_dir(),
            jobs_source: source_for(
                ENV_JOBS_ROOT,
                ENV_JOBS_DIR,
                self.configured_jobs_dir.is_some(),
            ),
            cache_dir: self.cache_dir(),
            cache_source: source_for(ENV_CACHE_DIR, "", self.configured_cache_dir.is_some()),
            temp_dir: self.temp_dir(),
            temp_source: source_for(ENV_TEMP_DIR, "", self.configured_temp_dir.is_some()),
            runtime_dir: self.runtime_dir(),
            runtime_source: source_for(ENV_RUNTIME_DIR, "", self.configured_runtime_dir.is_some()),
            exports_dir: self.exports_dir(),
            exports_source: source_for(ENV_EXPORTS_DIR, "", self.configured_exports_dir.is_some()),
            font_dirs: self.font_dirs(),
            font_source: font_source(self),
            provider: self.provider(),
            provider_source: provider_source(self),
            restart_required: false,
            restart_fields: Vec::new(),
            missing_model_files,
            runtime_library: self.ort_dylib.clone(),
            gallery_dl_path: self.gallery_dl_path(),
        }
    }

    /// Locate the optional direct-URL ingress helper without downloading or
    /// modifying it.  An explicit environment path wins, followed by the
    /// managed storage bin directory and then the user's PATH.
    pub fn gallery_dl_path(&self) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if let Some(path) = env_path(ENV_GALLERY_DL) {
            candidates.push(path);
        }
        let executable = if cfg!(windows) {
            "gallery-dl.exe"
        } else {
            "gallery-dl"
        };
        candidates.push(self.storage_root.join("bin").join(executable));
        if cfg!(windows) {
            candidates.push(self.storage_root.join("bin").join("gallery-dl"));
        }
        if let Some(path) = env::var_os("PATH") {
            for directory in env::split_paths(&path) {
                candidates.push(directory.join(executable));
                if cfg!(windows) {
                    candidates.push(directory.join("gallery-dl"));
                }
            }
        }
        candidates
            .into_iter()
            .find(|candidate| candidate.is_file())
            .map(|candidate| absolute_lossy(&candidate))
    }

    pub fn configure_user(&self, request: &ConfigureRequest) -> Result<ConfigReport> {
        let mut persisted = load_user_config(&self.config_file)?;
        if let Some(value) = request.storage_root.as_deref() {
            persisted.storage_root = Some(validate_dir_path(value, "storage_root", true)?);
        }
        if let Some(value) = request.models_dir.as_deref() {
            persisted.models_dir = Some(validate_dir_path(value, "models_dir", false)?);
        }
        if let Some(value) = request.jobs_dir.as_deref() {
            persisted.jobs_dir = Some(validate_dir_path(value, "jobs_dir", true)?);
        }
        if let Some(value) = request.cache_dir.as_deref() {
            persisted.cache_dir = Some(validate_dir_path(value, "cache_dir", true)?);
        }
        if let Some(value) = request.temp_dir.as_deref() {
            persisted.temp_dir = Some(validate_dir_path(value, "temp_dir", true)?);
        }
        if let Some(value) = request.runtime_dir.as_deref() {
            persisted.runtime_dir = Some(validate_dir_path(value, "runtime_dir", false)?);
        }
        if let Some(value) = request.exports_dir.as_deref() {
            persisted.exports_dir = Some(validate_dir_path(value, "exports_dir", true)?);
        }
        if !request.font_dirs.is_empty() {
            persisted.font_dirs = request
                .font_dirs
                .iter()
                .map(|v| validate_dir_path(v, "font_dirs", false))
                .collect::<Result<Vec<_>>>()?;
        }
        if let Some(provider) = request.provider.as_deref() {
            persisted.provider = Some(normalize_provider(provider).ok_or_else(|| {
                FukidashiError::InvalidInput("provider must be one of cpu, auto, cuda".into())
            })?);
        }
        validate_distinct_dirs(&persisted)?;
        if let Some(storage_root) = persisted.storage_root.as_deref() {
            create_storage_layout(storage_root)?;
        }
        atomic_user_config(&self.config_file, &persisted)?;
        let resolved = Config::resolve_from(
            &ConfigArgs {
                models_dir: None,
                ort_dylib: None,
            },
            self.config_file.clone(),
        )?;
        resolved.ensure_runtime_dirs()?;
        let mut report = resolved.config_report();
        report.restart_required = true;
        report.restart_fields = vec![
            "storage_root/models_dir/jobs_dir/cache_dir/temp_dir/runtime_dir/exports_dir/font_dirs/provider"
                .to_owned(),
        ];
        Ok(report)
    }

    pub fn doctor(&self) -> Vec<String> {
        let mut lines = [
            self.detector_model(),
            self.lama_model(),
            self.db_model(),
            self.models_dir.join("detection/script_id/osd_lstm.onnx"),
            self.models_dir.join("detection/script_id/osd_labels.json"),
            self.models_dir
                .join("ocr/ppocr-v6-onnx/PP-OCRv6_small_rec.onnx"),
            self.models_dir
                .join("ocr/ppocr-v6-onnx/PP-OCRv6_small_rec.txt"),
            self.models_dir
                .join("ocr/ppocr-v5-onnx/korean_PP-OCRv5_rec_mobile_infer.onnx"),
            self.models_dir
                .join("ocr/ppocr-v5-onnx/ppocrv5_korean_dict.txt"),
            self.models_dir
                .join("ocr/ppocr-v5-onnx/latin_PP-OCRv5_rec_mobile_infer.onnx"),
            self.models_dir
                .join("ocr/ppocr-v5-onnx/ppocrv5_latin_dict.txt"),
            self.english_model(),
            self.english_dictionary(),
            self.ocr_encoder_model(),
            self.ocr_init_model(),
            self.ocr_step_model(),
            self.ocr_vocab(),
            self.baberu_vision_model(),
            self.baberu_prefill_model(),
            self.baberu_step_model(),
            self.baberu_vocab(),
            self.baberu_tokenizer_config(),
        ]
        .into_iter()
        .map(|p| {
            format!(
                "{}: {}",
                p.display(),
                if p.is_file() { "present" } else { "missing" }
            )
        })
        .collect::<Vec<_>>();
        lines.push(match self.gallery_dl_path() {
            Some(path) => format!("gallery-dl: {}: present", path.display()),
            None => "gallery-dl: not found (optional for direct-URL ingress)".to_owned(),
        });
        lines
    }
}

fn source_for(primary: &str, secondary: &str, configured: bool) -> String {
    if env_path(primary).is_some() || (!secondary.is_empty() && env_path(secondary).is_some()) {
        "environment".to_owned()
    } else if configured {
        "user-config".to_owned()
    } else {
        "storage-default".to_owned()
    }
}

fn provider_source(config: &Config) -> String {
    if let Ok(value) = env::var(ENV_PROVIDER)
        && normalize_provider(&value).is_some()
    {
        "environment".to_owned()
    } else if config.configured_provider.is_some() {
        "user-config".to_owned()
    } else {
        "default".to_owned()
    }
}

fn font_source(config: &Config) -> String {
    if env_path(ENV_FONT_DIRS).is_some() || env_path(ENV_FONT_PATH).is_some() {
        "environment".to_owned()
    } else if !config.configured_font_dirs.is_empty() {
        "user-config".to_owned()
    } else {
        "storage-default+platform-discovery".to_owned()
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn absolute_lossy(path: &Path) -> PathBuf {
    absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn create_storage_layout(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    for name in [
        "models", "jobs", "cache", "temp", "runtime", "fonts", "exports",
    ] {
        fs::create_dir_all(root.join(name))?;
    }
    Ok(())
}

fn deduplicate_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = Vec::<String>::new();
    paths.retain(|path| {
        let key = absolute_lossy(path).to_string_lossy().into_owned();
        let key = if cfg!(windows) {
            key.to_ascii_lowercase()
        } else {
            key
        };
        if seen.iter().any(|previous| previous == &key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

fn font_dirs_from_env() -> Vec<PathBuf> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    env_path(ENV_FONT_DIRS)
        .into_iter()
        .flat_map(|value| {
            value
                .to_string_lossy()
                .split(separator)
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect()
}
fn normalize_provider(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "cpu" => Some("cpu".into()),
        "auto" => Some("auto".into()),
        "cuda" | "gpu" => Some("cuda".into()),
        _ => None,
    }
}
fn validate_dir_path(value: &str, field: &str, create: bool) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(FukidashiError::InvalidInput(format!(
            "{field} must be an absolute path"
        )));
    }
    if path.exists() {
        if !path.is_dir() {
            return Err(FukidashiError::InvalidInput(format!(
                "{field} must point to a directory: {}",
                path.display()
            )));
        }
        return path.canonicalize().map_err(Into::into);
    } else {
        if create {
            fs::create_dir_all(&path)?;
        }
    }
    Ok(path)
}
fn validate_distinct_dirs(config: &UserConfig) -> Result<()> {
    let mut seen = Vec::<PathBuf>::new();
    for path in [
        config.storage_root.as_ref(),
        config.models_dir.as_ref(),
        config.jobs_dir.as_ref(),
        config.cache_dir.as_ref(),
        config.temp_dir.as_ref(),
        config.runtime_dir.as_ref(),
        config.exports_dir.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let normalized = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.iter().any(|other| same_path(other, &normalized)) {
            return Err(FukidashiError::InvalidInput(format!(
                "configured directories must be distinct: {}",
                path.display()
            )));
        }
        seen.push(normalized);
    }
    Ok(())
}
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}
fn load_user_config(path: &Path) -> Result<UserConfig> {
    if !path.is_file() {
        return Ok(UserConfig::default());
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|e| {
        FukidashiError::InvalidInput(format!("invalid config {}: {e}", path.display()))
    })
}
fn atomic_user_config(path: &Path, value: &UserConfig) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| FukidashiError::InvalidInput("config path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map(|_| ()).map_err(|e| e.error.into())
}
fn platform_config_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .map(|p| p.join("Library/Application Support"))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(target_os = "windows")]
    {
        dirs::config_local_dir()
            .or_else(dirs::data_local_dir)
            .or_else(|| dirs::home_dir().map(|p| p.join("AppData/Local")))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
    }
}
fn platform_config_file() -> PathBuf {
    platform_config_dir().join("Fukidashi/config.json")
}
fn platform_storage_root() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .or_else(dirs::data_local_dir)
            .map(|p| p.join("Fukidashi"))
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|p| p.join("Library/Application Support/Fukidashi"))
    }
    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir()
            .or_else(dirs::home_dir)
            .map(|p| p.join("Fukidashi"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        dirs::data_local_dir()
            .or_else(dirs::home_dir)
            .map(|p| p.join("Fukidashi"))
    }
}
fn discover_ort_library_in(directory: &Path) -> Option<PathBuf> {
    if !directory.is_dir() {
        return None;
    }
    let names: &[&str] = if cfg!(target_os = "windows") {
        &["onnxruntime.dll"]
    } else if cfg!(target_os = "macos") {
        &["libonnxruntime.dylib", "onnxruntime.dylib"]
    } else {
        &["libonnxruntime.so", "libonnxruntime.so.1"]
    };
    for name in names {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let mut candidates = fs::read_dir(directory)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if cfg!(target_os = "windows") {
                name.starts_with("onnxruntime") && name.ends_with(".dll")
            } else if cfg!(target_os = "macos") {
                name.starts_with("libonnxruntime") && name.ends_with(".dylib")
            } else {
                name.starts_with("libonnxruntime.so")
            }
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.into_iter().next()
}
fn discover_ort_library() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(executable) = env::current_exe()
        && let Some(parent) = executable.parent()
    {
        roots.push(parent.to_path_buf());
    }
    if let Some(storage) = platform_storage_root() {
        roots.push(storage.join("runtime"));
    }
    roots
        .into_iter()
        .find_map(|root| discover_ort_library_in(&root))
}
pub fn configured_font_dirs_from_disk() -> Vec<PathBuf> {
    load_user_config(&platform_config_file())
        .map(|c| c.font_dirs)
        .unwrap_or_default()
}
pub fn font_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.extend(font_dirs_from_env());
    dirs.extend(configured_font_dirs_from_disk());
    dirs.extend(platform_font_dirs());
    deduplicate_paths(&mut dirs);
    dirs
}

fn platform_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = env::var_os("WINDIR").or_else(|| env::var_os("SystemRoot")) {
            dirs.push(PathBuf::from(root).join("Fonts"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(home) = env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/fonts"));
        }
        dirs.extend([
            PathBuf::from("/usr/local/share/fonts"),
            PathBuf::from("/usr/share/fonts"),
        ]);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
        dirs.extend([
            PathBuf::from("/Library/Fonts"),
            PathBuf::from("/System/Library/Fonts"),
        ]);
    }
    dirs
}
fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}
fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        let cwd = env::current_dir()?;
        let joined = cwd.join(path);
        Ok(joined.canonicalize().unwrap_or(joined))
    }
}
pub fn ensure_file(path: &Path) -> Result<()> {
    if fs::metadata(path).map(|m| m.is_file()).unwrap_or(false) {
        Ok(())
    } else {
        Err(FukidashiError::MissingAsset {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_storage_uses_one_fukidashi_namespace() {
        let root = platform_storage_root().expect("platform data directory");
        assert_eq!(
            root.file_name().and_then(|name| name.to_str()),
            Some("Fukidashi")
        );
    }

    #[test]
    fn gallery_dl_discovery_prefers_the_managed_storage_bin() {
        if env::var_os(ENV_GALLERY_DL).is_some() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().join("storage"),
            models_dir: temp.path().join("storage/models"),
            ort_dylib: None,
            config_file: temp.path().join("config.json"),
            configured_jobs_dir: None,
            configured_cache_dir: None,
            configured_temp_dir: None,
            configured_runtime_dir: None,
            configured_exports_dir: None,
            configured_font_dirs: Vec::new(),
            configured_provider: None,
            storage_source: "test".into(),
            models_source: "test".into(),
            ort_source: "test".into(),
        };
        let name = if cfg!(windows) {
            "gallery-dl.exe"
        } else {
            "gallery-dl"
        };
        let helper = config.storage_root.join("bin").join(name);
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        fs::write(&helper, b"fixture").unwrap();
        assert_eq!(config.gallery_dl_path(), Some(helper));
        assert!(
            config
                .doctor()
                .iter()
                .any(|line| line.contains("gallery-dl") && line.contains("present"))
        );
    }

    #[test]
    fn configure_persists_atomic_storage_and_provider() {
        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config/Fukidashi/config.json");
        let config = Config {
            storage_root: temp.path().join("old"),
            models_dir: temp.path().join("old/models"),
            ort_dylib: None,
            config_file: config_file.clone(),
            configured_jobs_dir: None,
            configured_cache_dir: None,
            configured_temp_dir: None,
            configured_runtime_dir: None,
            configured_exports_dir: None,
            configured_font_dirs: Vec::new(),
            configured_provider: None,
            storage_source: "test".into(),
            models_source: "test".into(),
            ort_source: "test".into(),
        };
        let new_root = temp.path().join("new");
        let report = config
            .configure_user(&ConfigureRequest {
                storage_root: Some(new_root.to_string_lossy().into()),
                provider: Some("cpu".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(report.restart_required);
        assert_eq!(report.provider, "cpu");
        let saved = fs::read_to_string(config_file).unwrap();
        assert!(saved.contains("storage_root"));
        assert!(saved.contains("cpu"));
        assert!(new_root.join("cache").is_dir());
        assert!(new_root.join("temp").is_dir());
        assert!(new_root.join("runtime").is_dir());
        assert!(new_root.join("exports").is_dir());
    }

    #[test]
    fn configured_cache_and_temp_dirs_are_materialized_at_startup() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().join("storage"),
            models_dir: temp.path().join("storage/models"),
            ort_dylib: None,
            config_file: temp.path().join("config.json"),
            configured_jobs_dir: None,
            configured_cache_dir: Some(temp.path().join("separate-cache")),
            configured_temp_dir: Some(temp.path().join("separate-temp")),
            configured_runtime_dir: None,
            configured_exports_dir: None,
            configured_font_dirs: Vec::new(),
            configured_provider: None,
            storage_source: "test".into(),
            models_source: "test".into(),
            ort_source: "test".into(),
        };
        fs::write(
            &config.config_file,
            serde_json::to_vec(&UserConfig {
                temp_dir: Some(temp.path().join("separate-temp")),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        config.ensure_runtime_dirs().unwrap();
        assert!(config.cache_dir().is_dir());
        assert!(config.temp_dir().is_dir());
    }

    #[test]
    fn configure_rejects_relative_and_invalid_provider() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            models_dir: temp.path().join("models"),
            ort_dylib: None,
            config_file: temp.path().join("config.json"),
            configured_jobs_dir: None,
            configured_cache_dir: None,
            configured_temp_dir: None,
            configured_runtime_dir: None,
            configured_exports_dir: None,
            configured_font_dirs: Vec::new(),
            configured_provider: None,
            storage_source: "test".into(),
            models_source: "test".into(),
            ort_source: "test".into(),
        };
        assert!(
            config
                .configure_user(&ConfigureRequest {
                    storage_root: Some("relative".into()),
                    ..Default::default()
                })
                .is_err()
        );
        assert!(
            config
                .configure_user(&ConfigureRequest {
                    provider: Some("metal".into()),
                    ..Default::default()
                })
                .is_err()
        );
    }
}
