//! Local PP-OCR inference and automatic multilingual routing.
#[cfg(feature = "onnx")]
use crate::config;
#[cfg(feature = "onnx")]
use crate::vision::detect::Detection;
use crate::{
    config::Config,
    domain::Rect,
    error::{FukidashiError, Result},
};
#[cfg(feature = "onnx")]
use image::{DynamicImage, GrayImage, RgbImage, imageops::FilterType};
use serde::Serialize;
#[cfg(feature = "onnx")]
use sha2::{Digest, Sha256};
#[cfg(feature = "onnx")]
use std::cell::Cell;
use std::path::Path;
#[cfg(feature = "onnx")]
use std::{collections::HashMap, fs};

#[cfg(feature = "onnx")]
const MAX_IMAGE_PIXELS: u64 = 64_000_000;
#[cfg(feature = "onnx")]
const MAX_RECOGNIZER_WIDTH: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecognizerKind {
    Han,
    Korean,
    Latin,
    English,
    Manga,
    Baberu,
}
impl RecognizerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Han => "ppocr-v6-han",
            Self::Korean => "ppocr-v5-korean",
            Self::Latin => "ppocr-v5-latin",
            Self::English => "ppocr-v5-english",
            Self::Manga => "manga-ocr-mobile",
            Self::Baberu => "baberu-ocr",
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct OcrRegion {
    pub id: String,
    pub bbox: Rect,
    pub text: String,
    pub source_language: String,
    pub script: String,
    pub recognizer: String,
    pub confidence: f32,
    pub uncertainty: f32,
    pub detector_label: i64,
    pub detector_confidence: f32,
    pub reading_order: usize,
    pub vision_correction: VisionCorrection,
}
#[derive(Debug, Clone, Serialize)]
pub struct VisionCorrection {
    pub image_path: String,
    pub bbox: Rect,
    pub contract: &'static str,
}
#[derive(Debug, Clone, Serialize)]
pub struct TranslationHandoff {
    pub target_language: String,
    pub status: &'static str,
    pub items: Vec<TranslationItem>,
}
#[derive(Debug, Clone, Serialize)]
pub struct TranslationItem {
    pub id: String,
    pub source_text: String,
    pub ocr_text: String,
    /// Recognition confidence remains available to non-vision clients.
    pub confidence: f32,
    pub corrected_source_text: Option<String>,
    pub correction_applied: bool,
    pub source_language: String,
    pub bbox: Rect,
    pub translation: Option<String>,
    pub status: &'static str,
    pub vision_correction: VisionCorrection,
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
impl TranslationItem {
    fn from_region(region: &OcrRegion) -> Self {
        Self::from_region_with_correction(region, None)
    }
    fn from_region_with_correction(region: &OcrRegion, corrected: Option<&str>) -> Self {
        let (source_text, correction_applied) = effective_source_text(&region.text, corrected);
        let corrected_source_text = corrected
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned);
        Self {
            id: region.id.clone(),
            source_text,
            ocr_text: region.text.clone(),
            confidence: region.confidence,
            corrected_source_text,
            correction_applied,
            source_language: region.source_language.clone(),
            bbox: region.bbox,
            translation: None,
            status: "pending",
            vision_correction: region.vision_correction.clone(),
        }
    }
}
pub fn effective_source_text(
    ocr_text: &str,
    corrected_source_text: Option<&str>,
) -> (String, bool) {
    if let Some(corrected) = corrected_source_text.filter(|text| !text.trim().is_empty()) {
        (corrected.to_owned(), true)
    } else {
        (ocr_text.to_owned(), false)
    }
}

/// Apply optional vision-model corrections to an analysis. The stable region
/// id addresses the same item returned by OCR; empty corrections are ignored.
/// Keeping this input optional means ordinary text-only clients only consume
/// `ocr_text` and do not need a vision model or correction payload.
pub fn apply_source_corrections(
    analysis: &mut PageAnalysis,
    corrections: &std::collections::HashMap<String, String>,
) {
    for item in &mut analysis.translation_handoff.items {
        let Some(corrected) = corrections.get(&item.id) else {
            continue;
        };
        let (source_text, correction_applied) =
            effective_source_text(&item.ocr_text, Some(corrected.as_str()));
        item.source_text = source_text;
        item.correction_applied = correction_applied;
        item.corrected_source_text = correction_applied.then(|| corrected.clone());
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct PageAnalysis {
    pub source_language: String,
    pub target_language: String,
    pub bubbles: Vec<OcrRegion>,
    pub text_lines: Vec<OcrRegion>,
    pub unmatched_text: Vec<OcrRegion>,
    pub translation_handoff: TranslationHandoff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageWorkerMode {
    CpuOnly,
    GpuOnly,
    CpuAndGpu,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConcurrentPageAnalysis {
    pub page_index: usize,
    pub image_path: String,
    pub worker: String,
    pub provider: String,
    pub analysis: PageAnalysis,
}

#[derive(Debug, Clone, Serialize)]
pub struct InpaintExecution {
    pub provider: &'static str,
    pub fallback: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CropCleanExecution {
    pub provider: &'static str,
    pub fallback: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    pub crops: Vec<Rect>,
}

pub fn resolve_target_language(explicit: Option<&str>, configured: &str) -> Result<String> {
    let selected = explicit
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(configured)
        .trim();
    if selected.is_empty()
        || selected.len() > 32
        || !selected
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(FukidashiError::InvalidInput(
            "target_language must be a BCP-47-like language tag".into(),
        ));
    }
    Ok(selected.to_ascii_lowercase())
}

#[derive(Debug, Default)]
pub struct OcrEngine {
    #[cfg(feature = "onnx")]
    pipeline: Option<Pipeline>,
    #[cfg(feature = "onnx")]
    lama: Option<ort::session::Session>,
    #[cfg(feature = "onnx")]
    lama_on_cpu: bool,
    completed_heavy_calls: usize,
}

#[cfg(feature = "onnx")]
fn stable_region_id(prefix: &str, bbox: Rect, occurrence: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    for value in [bbox.x1, bbox.y1, bbox.x2, bbox.y2] {
        hasher.update(value.to_bits().to_le_bytes());
    }
    hasher.update(occurrence.to_le_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("{prefix}-{}", &digest[..16])
}

#[cfg(feature = "onnx")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPolicy {
    Cpu,
    Cuda,
}

#[cfg(feature = "onnx")]
thread_local! {
    static SESSION_POLICY: Cell<SessionPolicy> = const { Cell::new(SessionPolicy::Cpu) };
    static SESSION_POLICY_OVERRIDE: Cell<Option<SessionPolicy>> = const { Cell::new(None) };
    static SESSION_FELL_BACK: Cell<bool> = const { Cell::new(false) };
}
impl OcrEngine {
    /// Drop model sessions and their per-session CPU/GPU arenas.
    pub fn release_sessions(&mut self) {
        #[cfg(feature = "onnx")]
        {
            self.pipeline = None;
            self.lama = None;
            self.lama_on_cpu = false;
        }
        self.completed_heavy_calls = 0;
    }

    /// Mark a page-level operation complete and recycle at the configured boundary.
    pub fn finish_heavy_call(&mut self, recycle_pages: usize) -> bool {
        if recycle_pages == 0 {
            return false;
        }
        self.completed_heavy_calls = self.completed_heavy_calls.saturating_add(1);
        if self.completed_heavy_calls >= recycle_pages {
            self.release_sessions();
            true
        } else {
            false
        }
    }

    #[cfg(feature = "onnx")]
    pub fn analyze(
        &mut self,
        config: &Config,
        image_path: &Path,
        source: Option<&str>,
        target: Option<&str>,
    ) -> Result<PageAnalysis> {
        let target = resolve_target_language(target, &config.configured_target_language())?;
        if !image_path.is_file() {
            return Err(FukidashiError::MissingAsset {
                path: image_path.to_path_buf(),
            });
        }
        let (width, height) = image::image_dimensions(image_path)?;
        if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
            return Err(FukidashiError::ResourceLimit(
                "image exceeds OCR pixel limit".into(),
            ));
        }
        let image = image::open(image_path)?.to_rgb8();
        let pipeline = self.pipeline.get_or_insert_with(Pipeline::default);
        let policy = SESSION_POLICY_OVERRIDE.with(Cell::get).unwrap_or_else(|| {
            if config.prefer_gpu() {
                SessionPolicy::Cuda
            } else {
                SessionPolicy::Cpu
            }
        });
        SESSION_POLICY.with(|slot| {
            let previous = slot.replace(policy);
            SESSION_FELL_BACK.with(|fallback| fallback.set(false));
            let result = pipeline.analyze(config, &image, image_path, source, target);
            slot.set(previous);
            result
        })
    }

    #[cfg(feature = "onnx")]
    pub fn analyze_with_policy(
        &mut self,
        config: &Config,
        image_path: &Path,
        source: Option<&str>,
        target: Option<&str>,
        policy: PageWorkerMode,
    ) -> Result<PageAnalysis> {
        let session_policy = match policy {
            PageWorkerMode::CpuOnly => SessionPolicy::Cpu,
            PageWorkerMode::GpuOnly | PageWorkerMode::CpuAndGpu => SessionPolicy::Cuda,
        };
        SESSION_POLICY_OVERRIDE.with(|override_slot| {
            let previous_override = override_slot.replace(Some(session_policy));
            let result = self.analyze(config, image_path, source, target);
            override_slot.set(previous_override);
            result
        })
    }
    #[cfg(feature = "onnx")]
    pub fn clean(
        &mut self,
        config: &Config,
        image_path: &Path,
        mask_path: Option<&Path>,
        dilation: u8,
    ) -> Result<(RgbImage, GrayImage, InpaintExecution)> {
        if !image_path.is_file() {
            return Err(FukidashiError::MissingAsset {
                path: image_path.to_path_buf(),
            });
        }
        let original = image::open(image_path)?.to_rgb8();
        crate::vision::inpaint::image_pixel_limit(original.width(), original.height())?;
        let mask = if let Some(path) = mask_path {
            if !path.is_file() {
                return Err(FukidashiError::MissingAsset {
                    path: path.to_path_buf(),
                });
            }
            let mask = image::open(path)?.to_luma8();
            crate::vision::inpaint::validate_mask(&mask, original.width(), original.height())?;
            crate::vision::inpaint::validate_stroke_mask(&mask)?;
            crate::vision::inpaint::dilate_mask(&mask, dilation)?
        } else {
            let analysis = self.analyze(config, image_path, None, None)?;
            let regions = analysis
                .text_lines
                .iter()
                .map(|region| region.bbox)
                .collect::<Vec<_>>();
            if regions.is_empty() {
                return Err(FukidashiError::InvalidInput(
                    "no text-stroke evidence available; refusing destructive bubble-box fallback"
                        .into(),
                ));
            }
            let mask = crate::vision::inpaint::mask_from_text_regions(
                original.width(),
                original.height(),
                &original,
                &regions,
                dilation,
            )?;
            if mask.pixels().all(|pixel| pixel[0] == 0) {
                return Err(FukidashiError::InvalidInput(
                    "text-stroke detector produced an empty mask; refusing destructive fallback"
                        .into(),
                ));
            }
            mask
        };
        // When mask generation ran OCR, release that model family before LaMa
        // is loaded. This avoids the previous detector + OCR + LaMa peak.
        if !config.retain_stage_sessions() {
            self.pipeline = None;
        }
        let previous_policy = SESSION_POLICY.with(|slot| {
            slot.replace(if config.prefer_gpu() {
                SessionPolicy::Cuda
            } else {
                SessionPolicy::Cpu
            })
        });
        let result = self.inpaint(config, &original, &mask);
        SESSION_POLICY.with(|slot| slot.set(previous_policy));
        let (cleaned, execution) = result?;
        Ok((cleaned, mask, execution))
    }

    #[cfg(feature = "onnx")]
    #[allow(clippy::too_many_arguments)]
    pub fn clean_crops(
        &mut self,
        config: &Config,
        image_path: &Path,
        mask_path: Option<&Path>,
        text_regions: &[Rect],
        dilation: u8,
        crop_padding: u32,
        crop_minimum_size: u32,
    ) -> Result<(RgbImage, GrayImage, CropCleanExecution)> {
        if !image_path.is_file() {
            return Err(FukidashiError::MissingAsset {
                path: image_path.to_path_buf(),
            });
        }
        let original = image::open(image_path)?.to_rgb8();
        crate::vision::inpaint::image_pixel_limit(original.width(), original.height())?;
        let mask = if let Some(path) = mask_path {
            if !path.is_file() {
                return Err(FukidashiError::MissingAsset {
                    path: path.to_path_buf(),
                });
            }
            let mask = image::open(path)?.to_luma8();
            crate::vision::inpaint::validate_mask(&mask, original.width(), original.height())?;
            crate::vision::inpaint::validate_stroke_mask(&mask)?;
            crate::vision::inpaint::dilate_mask(&mask, dilation)?
        } else {
            if text_regions.is_empty() {
                return Err(FukidashiError::InvalidInput(
                    "crop cleaning requires mask_path, text_regions, or analysis_path".into(),
                ));
            }
            crate::vision::inpaint::adaptive_mask_from_text_regions(
                original.width(),
                original.height(),
                &original,
                text_regions,
                dilation,
            )?
        };
        if mask.pixels().all(|pixel| pixel[0] == 0) {
            return Err(FukidashiError::InvalidInput(
                "crop text-stroke mask is empty".into(),
            ));
        }
        let crops = crate::vision::inpaint::crop_regions_from_mask(
            &mask,
            text_regions,
            crop_padding,
            crop_minimum_size,
            8,
        )?;
        if crops.is_empty() {
            return Err(FukidashiError::InvalidInput(
                "crop cleaning found no masked crop regions".into(),
            ));
        }
        self.pipeline = None;
        let mut output = original.clone();
        let mut provider = "CUDAExecutionProvider";
        let mut fallback = false;
        let mut fallback_reason = None;
        for crop in &crops {
            let x = crop.x1 as u32;
            let y = crop.y1 as u32;
            let width = (crop.x2 - crop.x1) as u32;
            let height = (crop.y2 - crop.y1) as u32;
            let crop_image = image::imageops::crop_imm(&original, x, y, width, height).to_image();
            let crop_mask = image::imageops::crop_imm(&mask, x, y, width, height).to_image();
            let previous_policy = SESSION_POLICY.with(|slot| {
                slot.replace(if config.prefer_gpu() {
                    SessionPolicy::Cuda
                } else {
                    SessionPolicy::Cpu
                })
            });
            let result = self.inpaint(config, &crop_image, &crop_mask);
            SESSION_POLICY.with(|slot| slot.set(previous_policy));
            let (cleaned, execution) = result?;
            if execution.provider == "CPUExecutionProvider" {
                provider = "CPUExecutionProvider";
            }
            fallback |= execution.fallback;
            if fallback_reason.is_none() {
                fallback_reason = execution.fallback_reason;
            }
            crate::vision::inpaint::composite_masked_crop(&mut output, &cleaned, &crop_mask, x, y)?;
        }
        Ok((
            output,
            mask,
            CropCleanExecution {
                provider,
                fallback,
                fallback_reason,
                crops,
            },
        ))
    }

    #[cfg(feature = "onnx")]
    pub fn clean_with_regions(
        &mut self,
        config: &Config,
        image_path: &Path,
        text_regions: &[Rect],
        dilation: u8,
    ) -> Result<(RgbImage, GrayImage, InpaintExecution)> {
        if !image_path.is_file() {
            return Err(FukidashiError::MissingAsset {
                path: image_path.to_path_buf(),
            });
        }
        if text_regions.is_empty() {
            return Err(FukidashiError::InvalidInput(
                "geometry cleaning requires text regions".into(),
            ));
        }
        let original = image::open(image_path)?.to_rgb8();
        crate::vision::inpaint::image_pixel_limit(original.width(), original.height())?;
        let mask = crate::vision::inpaint::adaptive_mask_from_text_regions(
            original.width(),
            original.height(),
            &original,
            text_regions,
            dilation,
        )?;
        if mask.pixels().all(|pixel| pixel[0] == 0) {
            return Err(FukidashiError::InvalidInput(
                "geometry text-stroke mask is empty".into(),
            ));
        }
        self.pipeline = None;
        let (cleaned, execution) = self.inpaint(config, &original, &mask)?;
        Ok((cleaned, mask, execution))
    }
    #[cfg(not(feature = "onnx"))]
    pub fn analyze(
        &mut self,
        _: &Config,
        image_path: &Path,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<PageAnalysis> {
        Err(FukidashiError::RuntimeUnavailable(format!(
            "ONNX inference is disabled; rebuild with feature onnx for {}",
            image_path.display()
        )))
    }
    #[cfg(not(feature = "onnx"))]
    pub fn clean(
        &mut self,
        _: &Config,
        image_path: &Path,
        _: Option<&Path>,
        _: u8,
    ) -> Result<(image::RgbImage, image::GrayImage, InpaintExecution)> {
        Err(FukidashiError::RuntimeUnavailable(format!(
            "ONNX inference is disabled; rebuild with feature onnx for {}",
            image_path.display()
        )))
    }

    #[cfg(not(feature = "onnx"))]
    #[allow(clippy::too_many_arguments)]
    pub fn clean_crops(
        &mut self,
        _: &Config,
        image_path: &Path,
        _: Option<&Path>,
        _: &[Rect],
        _: u8,
        _: u32,
        _: u32,
    ) -> Result<(image::RgbImage, image::GrayImage, CropCleanExecution)> {
        Err(FukidashiError::RuntimeUnavailable(format!(
            "ONNX inference is disabled; rebuild with feature onnx for {}",
            image_path.display()
        )))
    }

    #[cfg(not(feature = "onnx"))]
    pub fn clean_with_regions(
        &mut self,
        _: &Config,
        image_path: &Path,
        _: &[Rect],
        _: u8,
    ) -> Result<(image::RgbImage, image::GrayImage, InpaintExecution)> {
        Err(FukidashiError::RuntimeUnavailable(format!(
            "ONNX inference is disabled; rebuild with feature onnx for {}",
            image_path.display()
        )))
    }
}

#[cfg(feature = "onnx")]
pub fn analyze_pages_concurrent(
    config: &Config,
    image_paths: &[std::path::PathBuf],
    mode: PageWorkerMode,
) -> Result<Vec<ConcurrentPageAnalysis>> {
    use std::sync::{Arc, Mutex, mpsc::sync_channel};
    use std::thread;

    if image_paths.is_empty() {
        return Ok(Vec::new());
    }
    let workers = match mode {
        PageWorkerMode::CpuOnly => vec![(SessionPolicy::Cpu, "cpu-worker")],
        PageWorkerMode::GpuOnly => vec![(SessionPolicy::Cuda, "cuda-worker")],
        PageWorkerMode::CpuAndGpu => vec![
            (SessionPolicy::Cpu, "cpu-worker"),
            (SessionPolicy::Cuda, "cuda-worker"),
        ],
    };
    let (work_tx, work_rx) = sync_channel::<(usize, std::path::PathBuf)>(2);
    let work_rx = Arc::new(Mutex::new(work_rx));
    let (result_tx, result_rx) = sync_channel(image_paths.len());
    let mut handles = Vec::with_capacity(workers.len());
    for (policy, worker) in workers {
        let work_rx = Arc::clone(&work_rx);
        let result_tx = result_tx.clone();
        let config = config.clone();
        handles.push(thread::spawn(move || {
            let mut engine = OcrEngine::default();
            loop {
                let item = work_rx.lock().expect("page work queue lock").recv();
                let Ok((page_index, image_path)) = item else {
                    break;
                };
                let result = engine.analyze_with_policy(
                    &config,
                    &image_path,
                    None,
                    None,
                    if policy == SessionPolicy::Cpu {
                        PageWorkerMode::CpuOnly
                    } else {
                        PageWorkerMode::GpuOnly
                    },
                );
                engine.finish_heavy_call(config.session_recycle_pages());
                let fell_back = SESSION_FELL_BACK.with(Cell::get);
                let provider = if policy == SessionPolicy::Cuda && !fell_back {
                    "CUDAExecutionProvider"
                } else if policy == SessionPolicy::Cuda {
                    "CPUExecutionProvider (GPU worker fallback)"
                } else {
                    "CPUExecutionProvider"
                };
                result_tx
                    .send((page_index, image_path, worker, provider, result))
                    .expect("page result receiver");
            }
        }));
    }
    drop(result_tx);
    for (page_index, path) in image_paths.iter().enumerate() {
        work_tx
            .send((page_index, path.clone()))
            .map_err(|e| FukidashiError::Inference(format!("page work queue closed: {e}")))?;
    }
    drop(work_tx);
    let mut results = Vec::with_capacity(image_paths.len());
    for _ in image_paths {
        let (page_index, image_path, worker, provider, result) = result_rx
            .recv()
            .map_err(|e| FukidashiError::Inference(format!("page result queue closed: {e}")))?;
        results.push(result.map(|analysis| ConcurrentPageAnalysis {
            page_index,
            image_path: image_path.display().to_string(),
            worker: worker.to_string(),
            provider: provider.to_string(),
            analysis,
        }));
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| FukidashiError::Inference("page worker panicked".into()))?;
    }
    let mut ordered = Vec::with_capacity(results.len());
    for result in results {
        ordered.push(result?);
    }
    ordered.sort_by_key(|result| result.page_index);
    Ok(ordered)
}

#[cfg(feature = "onnx")]
impl OcrEngine {
    fn inpaint(
        &mut self,
        config: &Config,
        original: &RgbImage,
        mask: &GrayImage,
    ) -> Result<(RgbImage, InpaintExecution)> {
        if mask.pixels().all(|pixel| pixel[0] == 0) {
            return Ok((
                original.clone(),
                InpaintExecution {
                    provider: "none",
                    fallback: false,
                    fallback_reason: None,
                },
            ));
        }
        ensure_runtime(config)?;
        if self.lama.is_none() {
            let path = config.lama_model();
            config::ensure_file(&path)?;
            SESSION_FELL_BACK.with(|fallback| fallback.set(false));
            self.lama = Some(session(config, &path)?);
            self.lama_on_cpu = SESSION_POLICY.with(Cell::get) == SessionPolicy::Cpu
                || SESSION_FELL_BACK.with(Cell::get);
        }
        let (pad_h, pad_w) = crate::vision::inpaint::modulo_padding(
            original.height() as usize,
            original.width() as usize,
            8,
        );
        let height = original.height() as usize + pad_h;
        let width = original.width() as usize + pad_w;
        if u64::try_from(width).unwrap_or(u64::MAX) * u64::try_from(height).unwrap_or(u64::MAX)
            > MAX_IMAGE_PIXELS
        {
            return Err(FukidashiError::ResourceLimit(
                "padded inpainting tensor exceeds pixel limit".into(),
            ));
        }
        let image_data = crate::vision::inpaint::pad_symmetric_rgb(original, pad_h, pad_w);
        let mask_data = crate::vision::inpaint::pad_symmetric_mask(mask, pad_h, pad_w);
        let fell_back_at_creation = self.lama_on_cpu;
        let first = run_lama_session(
            self.lama.as_mut().expect("initialized LaMa session"),
            &image_data,
            &mask_data,
            width,
            height,
        );
        let (padded, execution) = match first {
            Ok(padded) => (
                padded,
                InpaintExecution {
                    provider: if fell_back_at_creation {
                        "CPUExecutionProvider"
                    } else {
                        "CUDAExecutionProvider"
                    },
                    fallback: fell_back_at_creation,
                    fallback_reason: None,
                },
            ),
            Err(gpu_error)
                if should_retry_inpaint_on_cpu(
                    SESSION_POLICY.with(Cell::get),
                    fell_back_at_creation,
                    &gpu_error.to_string(),
                ) =>
            {
                let reason = gpu_error.to_string();
                tracing::warn!(error = %reason, "CUDA LaMa inference failed; retrying once on CPU");
                self.lama = None;
                let path = config.lama_model();
                let cpu_session = SESSION_POLICY.with(|slot| {
                    let previous = slot.replace(SessionPolicy::Cpu);
                    let result = session(config, &path);
                    slot.set(previous);
                    result
                })?;
                self.lama = Some(cpu_session);
                self.lama_on_cpu = true;
                let padded = run_lama_session(
                    self.lama.as_mut().expect("CPU LaMa session"),
                    &image_data,
                    &mask_data,
                    width,
                    height,
                )
                .map_err(|cpu_error| {
                    FukidashiError::Inference(format!(
                        "CUDA LaMa inference failed ({reason}); CPU retry failed ({cpu_error})"
                    ))
                })?;
                (
                    padded,
                    InpaintExecution {
                        provider: "CPUExecutionProvider",
                        fallback: true,
                        fallback_reason: Some(provider_failure_summary(&reason).into()),
                    },
                )
            }
            Err(error) => return Err(error),
        };
        let generated =
            image::imageops::crop_imm(&padded, 0, 0, original.width(), original.height())
                .to_image();
        let composed = crate::vision::inpaint::compose(
            original.as_raw(),
            generated.as_raw(),
            mask.as_raw(),
            3,
        )?;
        let image =
            RgbImage::from_raw(original.width(), original.height(), composed).ok_or_else(|| {
                FukidashiError::TensorContract("failed to construct cleaned image".into())
            })?;
        Ok((image, execution))
    }
}

#[cfg(feature = "onnx")]
fn run_lama_session(
    session: &mut ort::session::Session,
    image_data: &[f32],
    mask_data: &[f32],
    width: usize,
    height: usize,
) -> Result<RgbImage> {
    let image_tensor = ort::value::Tensor::from_array((
        vec![1_i64, 3, height as i64, width as i64],
        image_data.to_vec(),
    ))
    .map_err(ort_error)?;
    let mask_tensor = ort::value::Tensor::from_array((
        vec![1_i64, 1, height as i64, width as i64],
        mask_data.to_vec(),
    ))
    .map_err(ort_error)?;
    let outputs = session
        .run(ort::inputs![image_tensor, mask_tensor])
        .map_err(ort_error)?;
    let values = outputs[0].try_extract_array::<f32>().map_err(ort_error)?;
    crate::vision::inpaint::output_rgb(
        values.as_slice().ok_or_else(|| {
            FukidashiError::TensorContract("LaMa output is not contiguous".into())
        })?,
        values.shape(),
        width,
        height,
    )
}

#[cfg(feature = "onnx")]
#[derive(Debug, Default)]
struct Pipeline {
    detector: Option<ort::session::Session>,
    db: Option<ort::session::Session>,
    osd: Option<ort::session::Session>,
    osd_labels: Vec<String>,
    recognizers: HashMap<RecognizerKind, Recognizer>,
    manga: Option<MangaRecognizer>,
    baberu: Option<BaberuRecognizer>,
}
#[cfg(feature = "onnx")]
#[derive(Debug)]
struct Recognizer {
    session: ort::session::Session,
    chars: Vec<String>,
}
#[cfg(feature = "onnx")]
#[derive(Debug)]
struct MangaRecognizer {
    encoder: ort::session::Session,
    decoder_init: ort::session::Session,
    decoder_step: ort::session::Session,
    vocab: Vec<String>,
}
#[cfg(feature = "onnx")]
#[derive(Debug)]
struct BaberuRecognizer {
    vision: ort::session::Session,
    decoder_prefill: ort::session::Session,
    decoder_step: ort::session::Session,
    vocab: Vec<String>,
    content_ids: Vec<bool>,
}
#[cfg(feature = "onnx")]
fn ort_error(e: impl std::fmt::Display) -> FukidashiError {
    FukidashiError::Inference(e.to_string())
}
#[cfg(feature = "onnx")]
fn ensure_runtime(config: &Config) -> Result<()> {
    let dylib = config.ort_dylib.as_deref().ok_or_else(|| {
        FukidashiError::RuntimeUnavailable(
            "ONNX Runtime was not found: install a platform-native runtime in the configured runtime directory, set ORT_DYLIB_PATH, or run with FUKIDASHI_PROVIDER=cpu and the bundled/default runtime".into(),
        )
    })?;
    config::ensure_file(dylib)?;
    // The effective thread policy includes the CPU-only override used by the
    // bounded page runner. A CPU session must not probe, preload, or log CUDA
    // dependencies merely because this binary was compiled with the optional
    // `cuda` feature.
    #[cfg(feature = "cuda")]
    let cuda_requested = SESSION_POLICY.with(|policy| policy.get() == SessionPolicy::Cuda);
    static INIT: std::sync::OnceLock<std::result::Result<String, String>> =
        std::sync::OnceLock::new();
    match INIT.get_or_init(|| {
        #[cfg(feature = "cuda")]
        let cuda_provider_ready = if cuda_requested {
            let dependency_root = config.ort_gpu_dependencies_dir();
            let dependency_dirs = cuda_dependency_dirs(&dependency_root);
            #[cfg(target_os = "windows")]
            {
                use windows_sys::Win32::System::LibraryLoader::{
                    AddDllDirectory, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                    LOAD_LIBRARY_SEARCH_USER_DIRS, SetDefaultDllDirectories,
                };
                let _ = unsafe {
                    SetDefaultDllDirectories(
                        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS | LOAD_LIBRARY_SEARCH_USER_DIRS,
                    )
                };
                let add_directory = |path: &Path| {
                    use std::os::windows::ffi::OsStrExt;
                    let wide = path
                        .as_os_str()
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect::<Vec<_>>();
                    let _ = unsafe { AddDllDirectory(wide.as_ptr()) };
                };
                for directory in &dependency_dirs {
                    add_directory(directory);
                }
            }
            let dependency_error = dependency_dirs
                .iter()
                .filter_map(|directory| preload_dll_directory(directory))
                .next();
            if let Some(error) = dependency_error.as_ref() {
                eprintln!("onnxruntime: CUDA dependency preload failed: {error}");
            }
            // Let ORT load the provider after its core environment is
            // committed. Loading the provider before the core can make the
            // CUDA DLL's Windows initializer fail even when all dependencies
            // are present and individually loadable.
            !dependency_dirs.is_empty()
        } else {
            false
        };
        #[cfg(not(feature = "cuda"))]
        let cuda_provider_ready = false;
        CUDA_PROVIDER_READY.set(cuda_provider_ready).ok();
        let builder = ort::init_from(dylib).map_err(|e| e.to_string())?;
        let committed = builder.with_telemetry(false).commit();
        if !committed {
            return Err(
                "ONNX Runtime environment was already committed before dynamic loading".into(),
            );
        }
        let environment = ort::environment::Environment::current().map_err(|e| e.to_string())?;
        let devices = environment
            .devices()
            .map(|device| {
                let hardware = device.hardware_device();
                format!(
                    "{}/{:?}/{}#{}",
                    device.ep().unwrap_or("unknown"),
                    hardware.ty(),
                    hardware.vendor().unwrap_or("unknown"),
                    hardware.id()
                )
            })
            .collect::<Vec<_>>();
        Ok(if devices.is_empty() {
            "no execution-provider devices discovered".to_string()
        } else {
            format!(
                "execution-provider devices: {}; CUDA provider preload: {}",
                devices.join(", "),
                if cuda_provider_ready {
                    "ready"
                } else {
                    "unavailable"
                }
            )
        })
    }) {
        Ok(report) => {
            let policy = if SESSION_POLICY.with(|policy| policy.get() == SessionPolicy::Cuda) {
                "GPU-preferred"
            } else {
                "CPU"
            };
            eprintln!("onnxruntime: {policy} session policy enabled; {report}");
            Ok(())
        }
        Err(e) => Err(FukidashiError::RuntimeUnavailable(format!(
            "failed to load ONNX Runtime: {e}"
        ))),
    }
}
#[cfg(feature = "onnx")]
fn cuda_dependency_dirs(root: &Path) -> Vec<std::path::PathBuf> {
    let relative = if cfg!(target_os = "windows") {
        vec![
            "python/nvidia/cublas/bin",
            "python/nvidia/cuda_runtime/bin",
            "python/nvidia/cuda_nvrtc/bin",
            "python/nvidia/cufft/bin",
            "python/nvidia/curand/bin",
            "python/nvidia/nvjitlink/bin",
            "python/nvidia/cudnn/bin",
            "python/nvidia/cu13/bin/x86_64",
            "bin",
        ]
    } else {
        vec![
            "python/nvidia/cublas/lib",
            "python/nvidia/cuda_runtime/lib",
            "python/nvidia/cuda_nvrtc/lib",
            "python/nvidia/cufft/lib",
            "python/nvidia/curand/lib",
            "python/nvidia/nvjitlink/lib",
            "python/nvidia/cudnn/lib",
            "python/nvidia/cu13/lib",
            "lib",
            "lib64",
            "lib/x86_64",
            "lib/aarch64",
        ]
    };
    std::iter::once(root.to_path_buf())
        .chain(relative.into_iter().map(|relative| root.join(relative)))
        .filter(|path| path.is_dir())
        .collect()
}
#[cfg(feature = "onnx")]
static CUDA_PROVIDER_READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
#[cfg(feature = "onnx")]
fn preload_cuda_provider(path: &Path) -> std::result::Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryExW;
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                // CUDA12's provider DLL initializes successfully only with
                // the legacy loader mode after its absolute-path dependency
                // DLLs have been preloaded above. The provider path itself is
                // absolute, so this does not introduce cwd-based discovery.
                0,
            )
        };
        if handle.is_null() {
            return Err(format!(
                "LoadLibraryExW failed for {} (Win32 error {})",
                path.display(),
                unsafe { GetLastError() }
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        ort::util::preload_dylib(path).map_err(|error| error.to_string())
    }
}
#[cfg(feature = "onnx")]
fn preload_dll_directory(path: &Path) -> Option<String> {
    let mut pending = fs::read_dir(path)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_cuda_library(path))
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return None;
    }
    #[cfg(target_os = "windows")]
    {
        // Load CUDA DLLs by absolute path in dependency order. ORT's generic
        // preload helper can block while resolving the large CUDA12 bundle;
        // direct LoadLibraryExW also keeps discovery scoped to the configured
        // directories and avoids PATH mutation.
        pending.sort_by_key(|dll| {
            let name = dll
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            (!name.contains("cublaslt"), !name.contains("cudart"), name)
        });
        let mut last_error = None;
        for dll in pending {
            if let Err(error) = preload_cuda_provider(&dll) {
                last_error = Some(format!("{}: {error}", dll.display()));
            }
        }
        last_error
    }
    #[cfg(not(target_os = "windows"))]
    let mut last_error = None;
    #[cfg(not(target_os = "windows"))]
    for _ in 0..pending.len() {
        #[cfg(not(target_os = "windows"))]
        let previous_len = pending.len();
        #[cfg(not(target_os = "windows"))]
        let mut next = Vec::new();
        #[cfg(not(target_os = "windows"))]
        for dll in pending {
            match ort::util::preload_dylib(&dll) {
                Ok(()) => {}
                Err(error) => {
                    last_error = Some(format!("{}: {error}", dll.display()));
                    next.push(dll);
                }
            }
        }
        if next.len() == previous_len {
            break;
        }
        pending = next;
        if pending.is_empty() {
            return None;
        }
    }
    #[cfg(not(target_os = "windows"))]
    last_error
}

#[cfg(feature = "onnx")]
fn is_cuda_library(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    #[cfg(target_os = "windows")]
    {
        name.to_ascii_lowercase().ends_with(".dll")
    }
    #[cfg(target_os = "linux")]
    {
        name.contains(".so")
    }
    #[cfg(target_os = "macos")]
    {
        name.contains(".dylib") || name.contains(".so")
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        path.extension().is_some()
    }
}
#[cfg(feature = "onnx")]
fn session_builder(
    config: &Config,
    prefer_gpu: bool,
) -> Result<ort::session::builder::SessionBuilder> {
    let mut b = ort::session::Session::builder().map_err(ort_error)?;
    b = b
        .with_intra_threads(4)
        .map_err(ort_error)?
        .with_inter_threads(1)
        .map_err(ort_error)?
        .with_parallel_execution(false)
        .map_err(ort_error)?
        .with_memory_pattern(false)
        .map_err(ort_error)?;
    if prefer_gpu {
        #[cfg(feature = "cuda")]
        if *CUDA_PROVIDER_READY.get().unwrap_or(&false) {
            let cuda = ort::ep::CUDA::default()
                .with_memory_limit(config.gpu_memory_limit_bytes())
                .with_arena_extend_strategy(ort::ep::ArenaExtendStrategy::SameAsRequested)
                .with_conv_algorithm_search(ort::ep::cuda::ConvAlgorithmSearch::Heuristic)
                .with_conv_max_workspace(false);
            b = b
                .with_execution_providers([cuda.build().error_on_failure()])
                .map_err(ort_error)?;
        } else {
            b = b
                .with_auto_device(ort::session::builder::AutoDevicePolicy::PreferGPU)
                .map_err(ort_error)?;
        }
        #[cfg(not(feature = "cuda"))]
        {
            b = b
                .with_auto_device(ort::session::builder::AutoDevicePolicy::PreferGPU)
                .map_err(ort_error)?;
        }
    } else {
        b = b
            .with_auto_device(ort::session::builder::AutoDevicePolicy::PreferCPU)
            .map_err(ort_error)?;
    }
    Ok(b)
}
#[cfg(feature = "onnx")]
fn provider_failure(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "execution provider",
        "executionprovider",
        "provider",
        "cuda",
        "directml",
        "dml",
        "gpu",
        "device",
        "out of memory",
        "bfcarena",
        "failed to allocate memory",
        "available memory",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(feature = "onnx")]
fn should_retry_inpaint_on_cpu(
    policy: SessionPolicy,
    already_fell_back: bool,
    message: &str,
) -> bool {
    policy == SessionPolicy::Cuda && !already_fell_back && provider_failure(message)
}

#[cfg(feature = "onnx")]
fn provider_failure_summary(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if [
        "out of memory",
        "bfcarena",
        "failed to allocate memory",
        "available memory",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        "CUDA memory allocation failed"
    } else {
        "CUDA execution provider failed"
    }
}
#[cfg(feature = "onnx")]
fn session(config: &Config, path: &Path) -> Result<ort::session::Session> {
    let policy = SESSION_POLICY.with(Cell::get);
    if policy == SessionPolicy::Cpu {
        return session_builder(config, false)
            .and_then(|mut builder| builder.commit_from_file(path).map_err(ort_error));
    }
    if !*CUDA_PROVIDER_READY.get().unwrap_or(&false) {
        SESSION_FELL_BACK.with(|fallback| fallback.set(true));
    }
    let gpu_result = session_builder(config, true)
        .and_then(|mut builder| builder.commit_from_file(path).map_err(ort_error));
    match gpu_result {
        Ok(session) => {
            if *CUDA_PROVIDER_READY.get().unwrap_or(&false) {
                eprintln!(
                    "onnxruntime: explicit CUDAExecutionProvider session active: {}",
                    path.display()
                );
            }
            Ok(session)
        }
        Err(gpu_error) if provider_failure(&gpu_error.to_string()) => {
            SESSION_FELL_BACK.with(|fallback| fallback.set(true));
            tracing::warn!(
                path = %path.display(),
                error = %gpu_error,
                "GPU-preferred ONNX session creation failed; retrying with CPU policy"
            );
            session_builder(config, false)?
                .commit_from_file(path)
                .map_err(|cpu_error| {
                    FukidashiError::Inference(format!(
                        "GPU-preferred session failed ({gpu_error}); CPU fallback failed ({cpu_error})"
                    ))
                })
        }
        Err(error) => Err(ort_error(error)),
    }
}

#[cfg(feature = "onnx")]
impl Pipeline {
    fn initialize(&mut self, config: &Config) -> Result<()> {
        ensure_runtime(config)?;
        if self.detector.is_none() {
            config::ensure_file(&config.detector_model())?;
            self.detector = Some(session(config, &config.detector_model())?);
        }
        if self.db.is_none() {
            config::ensure_file(&config.db_model())?;
            self.db = Some(session(config, &config.db_model())?);
        }
        if self.osd.is_none() {
            let p = config.models_dir.join("detection/script_id/osd_lstm.onnx");
            config::ensure_file(&p)?;
            self.osd = Some(session(config, &p)?);
            let labels = config
                .models_dir
                .join("detection/script_id/osd_labels.json");
            config::ensure_file(&labels)?;
            self.osd_labels = serde_json::from_str(&fs::read_to_string(labels)?)
                .map_err(|e| FukidashiError::TensorContract(format!("invalid OSD labels: {e}")))?;
            if self.osd_labels.len() != 79 {
                return Err(FukidashiError::TensorContract(
                    "OSD labels must contain 79 classes".into(),
                ));
            }
        }
        Ok(())
    }
    fn recognizer(&mut self, config: &Config, kind: RecognizerKind) -> Result<&mut Recognizer> {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.recognizers.entry(kind) {
            let (model, dict, classes) = match kind {
                RecognizerKind::Han => (
                    config
                        .models_dir
                        .join("ocr/ppocr-v6-onnx/PP-OCRv6_small_rec.onnx"),
                    config
                        .models_dir
                        .join("ocr/ppocr-v6-onnx/PP-OCRv6_small_rec.txt"),
                    18710,
                ),
                RecognizerKind::Korean => (
                    config
                        .models_dir
                        .join("ocr/ppocr-v5-onnx/korean_PP-OCRv5_rec_mobile_infer.onnx"),
                    config
                        .models_dir
                        .join("ocr/ppocr-v5-onnx/ppocrv5_korean_dict.txt"),
                    11947,
                ),
                RecognizerKind::Latin => (
                    config
                        .models_dir
                        .join("ocr/ppocr-v5-onnx/latin_PP-OCRv5_rec_mobile_infer.onnx"),
                    config
                        .models_dir
                        .join("ocr/ppocr-v5-onnx/ppocrv5_latin_dict.txt"),
                    504,
                ),
                RecognizerKind::English => {
                    let model = config.english_model();
                    let dict = config.english_dictionary();
                    match (model.is_file(), dict.is_file()) {
                        // en_PP-OCRv5_rec_mobile_infer emits 438 classes:
                        // 436 dictionary entries plus CTC blank and space.
                        (true, true) => (model, dict, 438),
                        (false, false) => (
                            config
                                .models_dir
                                .join("ocr/ppocr-v5-onnx/latin_PP-OCRv5_rec_mobile_infer.onnx"),
                            config
                                .models_dir
                                .join("ocr/ppocr-v5-onnx/ppocrv5_latin_dict.txt"),
                            504,
                        ),
                        (false, true) => return Err(FukidashiError::MissingAsset { path: model }),
                        (true, false) => return Err(FukidashiError::MissingAsset { path: dict }),
                    }
                }
                RecognizerKind::Manga => {
                    return Err(FukidashiError::RuntimeUnavailable(
                        "Manga OCR uses its encoder/decoder route, not a CTC recognizer".into(),
                    ));
                }
                RecognizerKind::Baberu => {
                    return Err(FukidashiError::RuntimeUnavailable(
                        "Baberu OCR uses its vision/decoder route, not a CTC recognizer".into(),
                    ));
                }
            };
            config::ensure_file(&model)?;
            config::ensure_file(&dict)?;
            let chars = fs::read_to_string(&dict)?
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if classes != 0 && chars.len() + 2 != classes {
                return Err(FukidashiError::TensorContract(format!(
                    "{} dictionary does not match its graph class count",
                    kind.as_str()
                )));
            }
            entry.insert(Recognizer {
                session: session(config, &model)?,
                chars,
            });
        }
        Ok(self
            .recognizers
            .get_mut(&kind)
            .expect("inserted recognizer"))
    }
    fn manga_assets_available(config: &Config) -> bool {
        [
            config.ocr_encoder_model(),
            config.ocr_init_model(),
            config.ocr_step_model(),
            config.ocr_vocab(),
        ]
        .iter()
        .all(|path| path.is_file())
    }
    fn manga_recognizer(&mut self, config: &Config) -> Result<&mut MangaRecognizer> {
        if self.manga.is_none() {
            let encoder_path = config.ocr_encoder_model();
            let init_path = config.ocr_init_model();
            let step_path = config.ocr_step_model();
            let vocab_path = config.ocr_vocab();
            for path in [&encoder_path, &init_path, &step_path, &vocab_path] {
                config::ensure_file(path)?;
            }
            let vocab = fs::read_to_string(&vocab_path)?
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if vocab.len() != 9_415 {
                return Err(FukidashiError::TensorContract(format!(
                    "Manga OCR vocabulary must contain 9415 entries, got {}",
                    vocab.len()
                )));
            }
            self.manga = Some(MangaRecognizer {
                encoder: session(config, &encoder_path)?,
                decoder_init: session(config, &init_path)?,
                decoder_step: session(config, &step_path)?,
                vocab,
            });
        }
        Ok(self.manga.as_mut().expect("inserted Manga OCR recognizer"))
    }
    fn baberu_assets_available(config: &Config) -> bool {
        [
            config.baberu_vision_model(),
            config.baberu_prefill_model(),
            config.baberu_step_model(),
            config.baberu_vocab(),
            config.baberu_tokenizer_config(),
        ]
        .iter()
        .all(|path| path.is_file())
    }
    fn baberu_recognizer(&mut self, config: &Config) -> Result<&mut BaberuRecognizer> {
        if self.baberu.is_none() {
            let vision_path = config.baberu_vision_model();
            let prefill_path = config.baberu_prefill_model();
            let step_path = config.baberu_step_model();
            let vocab_path = config.baberu_vocab();
            let tokenizer_path = config.baberu_tokenizer_config();
            for path in [
                &vision_path,
                &prefill_path,
                &step_path,
                &vocab_path,
                &tokenizer_path,
            ] {
                config::ensure_file(path)?;
            }
            let vocab: Vec<String> = serde_json::from_str(&fs::read_to_string(&vocab_path)?)
                .map_err(|e| {
                    FukidashiError::TensorContract(format!("invalid Baberu vocab: {e}"))
                })?;
            if vocab.len() + 4 != 14_630 || vocab.iter().any(String::is_empty) {
                return Err(FukidashiError::TensorContract(format!(
                    "Baberu vocab must contain 14626 nonempty symbols, got {}",
                    vocab.len()
                )));
            }
            let tokenizer: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&tokenizer_path)?).map_err(|e| {
                    FukidashiError::TensorContract(format!("invalid Baberu tokenizer config: {e}"))
                })?;
            if tokenizer
                .get("bos_token")
                .and_then(serde_json::Value::as_str)
                != Some("<bos>")
                || tokenizer
                    .get("eos_token")
                    .and_then(serde_json::Value::as_str)
                    != Some("<eos>")
            {
                return Err(FukidashiError::TensorContract(
                    "Baberu tokenizer config must define <bos> and <eos>".into(),
                ));
            }
            let mut content_ids = vec![false; vocab.len() + 4];
            for (index, symbol) in vocab.iter().enumerate() {
                let mut chars = symbol.chars();
                if let (Some(ch), None) = (chars.next(), chars.next()) {
                    content_ids[index + 4] = ch.is_alphanumeric() && !"ーｰ〜~".contains(ch);
                }
            }
            self.baberu = Some(BaberuRecognizer {
                vision: session(config, &vision_path)?,
                decoder_prefill: session(config, &prefill_path)?,
                decoder_step: session(config, &step_path)?,
                vocab,
                content_ids,
            });
        }
        Ok(self.baberu.as_mut().expect("inserted Baberu recognizer"))
    }
    fn analyze(
        &mut self,
        config: &Config,
        image: &RgbImage,
        image_path: &Path,
        source: Option<&str>,
        target: String,
    ) -> Result<PageAnalysis> {
        self.initialize(config)?;
        let mut detections = self.detect_rtdetr(image)?;
        if detections.is_empty() {
            // Keep the DB detector as a conservative compatibility fallback when
            // an RT-DETR graph returns no usable objects. RT-DETR remains the
            // primary bubble/text detector whenever it has detections.
            detections = self
                .detect_text(image)?
                .into_iter()
                .map(|(bbox, score)| Detection {
                    label: 0,
                    bbox,
                    score,
                })
                .collect();
        }
        let mut bubble_detections = detections
            .iter()
            .copied()
            .filter(|d| d.label == 0)
            .collect::<Vec<_>>();
        let line_detections = detections
            .into_iter()
            .filter(|d| d.label == 1 || d.label == 2)
            .collect::<Vec<_>>();
        let page_prior = self.page_route_prior(config, image, &line_detections)?;
        bubble_detections.sort_by(|a, b| {
            a.bbox
                .y1
                .total_cmp(&b.bbox.y1)
                .then_with(|| a.bbox.x1.total_cmp(&b.bbox.x1))
                .then_with(|| b.score.total_cmp(&a.score))
        });
        let associations = associate_text_lines(&bubble_detections, &line_detections);
        let explicit_source = source.filter(|v| normalize_language(v) != "auto");
        let mut bubbles = Vec::with_capacity(bubble_detections.len());
        let mut text_lines = Vec::new();
        let mut unmatched_text = Vec::new();
        for (index, bubble_detection) in bubble_detections.iter().enumerate() {
            let line_indices = &associations[index];
            let bubble_ocr = if Self::baberu_assets_available(config) {
                Some(self.ocr_detection(
                    config,
                    image,
                    *bubble_detection,
                    explicit_source,
                    page_prior,
                    image_path,
                    format!("bubble-tmp-{}", index + 1),
                )?)
            } else {
                None
            };
            if line_indices.is_empty() {
                if let Some(region) = bubble_ocr {
                    bubbles.push(region);
                } else {
                    bubbles.push(self.ocr_detection(
                        config,
                        image,
                        *bubble_detection,
                        explicit_source,
                        page_prior,
                        image_path,
                        format!("bubble-tmp-{}", index + 1),
                    )?);
                }
                continue;
            }
            let mut line_regions = line_indices
                .iter()
                .map(|&line_index| {
                    self.ocr_detection(
                        config,
                        image,
                        line_detections[line_index],
                        explicit_source,
                        page_prior,
                        image_path,
                        format!("line-tmp-{}-{}", index + 1, line_index + 1),
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            text_lines.extend(line_regions.iter().cloned());
            sort_regions(&mut line_regions);
            let joiner = if line_regions.iter().all(|r| {
                r.recognizer == RecognizerKind::Han.as_str()
                    || r.recognizer == RecognizerKind::Manga.as_str()
            }) {
                ""
            } else {
                " "
            };
            let text = line_regions
                .iter()
                .map(|r| r.text.trim())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(joiner);
            let source_language = aggregate_source(&line_regions);
            let script = if line_regions
                .iter()
                .map(|r| r.script.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == 1
            {
                line_regions[0].script.clone()
            } else {
                "mixed".into()
            };
            let recognizer = if line_regions
                .iter()
                .map(|r| r.recognizer.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == 1
            {
                line_regions[0].recognizer.clone()
            } else {
                "mixed".into()
            };
            let rec_conf =
                line_regions.iter().map(|r| r.confidence).sum::<f32>() / line_regions.len() as f32;
            let uncertainty = line_regions
                .iter()
                .map(|r| r.uncertainty)
                .fold(0.0, f32::max);
            bubbles.push(if let Some(region) = bubble_ocr {
                if region.recognizer == RecognizerKind::Baberu.as_str() {
                    region
                } else {
                    OcrRegion {
                        id: format!("bubble-tmp-{}", index + 1),
                        bbox: bubble_detection.bbox,
                        text,
                        source_language,
                        script,
                        recognizer,
                        confidence: (bubble_detection.score * rec_conf).clamp(0.0, 1.0),
                        uncertainty,
                        detector_label: 0,
                        detector_confidence: bubble_detection.score,
                        reading_order: 0,
                        vision_correction: VisionCorrection {
                            image_path: image_path.display().to_string(),
                            bbox: bubble_detection.bbox,
                            contract: "source-image-bbox",
                        },
                    }
                }
            } else {
                OcrRegion {
                    id: format!("bubble-tmp-{}", index + 1),
                    bbox: bubble_detection.bbox,
                    text,
                    source_language,
                    script,
                    recognizer,
                    confidence: (bubble_detection.score * rec_conf).clamp(0.0, 1.0),
                    uncertainty,
                    detector_label: 0,
                    detector_confidence: bubble_detection.score,
                    reading_order: 0,
                    vision_correction: VisionCorrection {
                        image_path: image_path.display().to_string(),
                        bbox: bubble_detection.bbox,
                        contract: "source-image-bbox",
                    },
                }
            });
        }
        for (line_index, line) in line_detections.iter().enumerate() {
            if !associations.iter().any(|items| items.contains(&line_index)) {
                let region = self.ocr_detection(
                    config,
                    image,
                    *line,
                    explicit_source,
                    page_prior,
                    image_path,
                    format!("text-tmp-{}", line_index + 1),
                )?;
                text_lines.push(region.clone());
                unmatched_text.push(region);
            }
        }
        // A single page-wide direction keeps the comparator total for mixed
        // script pages (a comparator that chooses direction from only `a` can
        // violate antisymmetry for Han/Latin pairs).
        sort_regions(&mut bubbles);
        let mut bubble_occurrences = std::collections::HashMap::<[u32; 4], usize>::new();
        for (i, b) in bubbles.iter_mut().enumerate() {
            let key = [
                b.bbox.x1.to_bits(),
                b.bbox.y1.to_bits(),
                b.bbox.x2.to_bits(),
                b.bbox.y2.to_bits(),
            ];
            let occurrence = bubble_occurrences.entry(key).or_insert(0);
            b.id = stable_region_id("bubble", b.bbox, *occurrence);
            *occurrence = occurrence.saturating_add(1);
            b.reading_order = i;
        }
        sort_regions(&mut unmatched_text);
        let mut text_occurrences = std::collections::HashMap::<[u32; 4], usize>::new();
        for (i, region) in unmatched_text.iter_mut().enumerate() {
            let key = [
                region.bbox.x1.to_bits(),
                region.bbox.y1.to_bits(),
                region.bbox.x2.to_bits(),
                region.bbox.y2.to_bits(),
            ];
            let occurrence = text_occurrences.entry(key).or_insert(0);
            region.id = stable_region_id("text", region.bbox, *occurrence);
            *occurrence = occurrence.saturating_add(1);
            region.reading_order = i;
        }
        let mut line_occurrences = std::collections::HashMap::<[u32; 4], usize>::new();
        for line in &mut text_lines {
            let key = [
                line.bbox.x1.to_bits(),
                line.bbox.y1.to_bits(),
                line.bbox.x2.to_bits(),
                line.bbox.y2.to_bits(),
            ];
            let occurrence = line_occurrences.entry(key).or_insert(0);
            line.id = stable_region_id("line", line.bbox, *occurrence);
            *occurrence = occurrence.saturating_add(1);
        }
        let items = bubbles
            .iter()
            .chain(unmatched_text.iter())
            .map(TranslationItem::from_region)
            .collect();
        let source_language = source
            .filter(|v| normalize_language(v) != "auto")
            .map(normalize_language)
            .unwrap_or_else(|| {
                let mut all = bubbles.clone();
                all.extend(unmatched_text.clone());
                aggregate_source(&all)
            });
        Ok(PageAnalysis {
            source_language,
            target_language: target.clone(),
            bubbles,
            text_lines,
            unmatched_text,
            translation_handoff: TranslationHandoff {
                target_language: target,
                status: "pending",
                items,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn ocr_detection(
        &mut self,
        config: &Config,
        image: &RgbImage,
        detection: Detection,
        source: Option<&str>,
        page_prior: Option<RecognizerKind>,
        image_path: &Path,
        id: String,
    ) -> Result<OcrRegion> {
        let crop = crop_rgb(image, detection.bbox)?;
        let (script, script_confidence, script_uncertainty) = self.detect_script(&crop)?;
        let hinted = route_script(&script);
        let route = if is_japanese_script(&script) && Self::manga_assets_available(config) {
            RecognizerKind::Manga
        } else {
            hinted
        };
        let (text, rec_conf, route) = if let Some(explicit) = source {
            let requested = route_override(explicit)?;
            let route = preferred_explicit_route(config, requested);
            let (text, confidence, effective_route) = match route {
                RecognizerKind::Manga => {
                    let (text, confidence) = self.recognize_manga(config, &crop)?;
                    (text, confidence, route)
                }
                RecognizerKind::Baberu => match self.recognize_baberu(config, &crop) {
                    Ok((text, confidence)) if !text.trim().is_empty() => (text, confidence, route),
                    _ => {
                        let fallback = if requested == RecognizerKind::Manga
                            && !Self::manga_assets_available(config)
                        {
                            RecognizerKind::Han
                        } else {
                            requested
                        };
                        let (text, confidence) = self.recognize(config, fallback, &crop)?;
                        (text, confidence, fallback)
                    }
                },
                _ => {
                    let (text, confidence) = self.recognize(config, route, &crop)?;
                    (text, confidence, route)
                }
            };
            (text, confidence, effective_route)
        } else {
            let automatic = self.recognize_auto(
                config,
                route,
                &crop,
                page_prior,
                &script,
                script_confidence,
                true,
            );
            match automatic {
                Ok((text, _confidence, RecognizerKind::Baberu)) if text.trim().is_empty() => self
                    .recognize_auto(
                    config,
                    route,
                    &crop,
                    page_prior,
                    &script,
                    script_confidence,
                    false,
                )?,
                Ok(result) => result,
                Err(_) => self.recognize_auto(
                    config,
                    route,
                    &crop,
                    page_prior,
                    &script,
                    script_confidence,
                    false,
                )?,
            }
        };
        let source_language = source.map(normalize_language).unwrap_or_else(|| {
            if route == RecognizerKind::Baberu {
                match page_prior {
                    Some(RecognizerKind::English) => "en".into(),
                    Some(RecognizerKind::Latin) => "en|latin".into(),
                    _ => inferred_language(&script, route, &text),
                }
            } else {
                inferred_language(&script, route, &text)
            }
        });
        let confidence = (detection.score * rec_conf).clamp(0.0, 1.0);
        Ok(OcrRegion {
            id,
            bbox: detection.bbox,
            text,
            source_language,
            script,
            recognizer: route.as_str().into(),
            confidence,
            uncertainty: (1.0 - confidence).max(script_uncertainty).clamp(0.0, 1.0),
            detector_label: detection.label,
            detector_confidence: detection.score,
            reading_order: 0,
            vision_correction: VisionCorrection {
                image_path: image_path.display().to_string(),
                bbox: detection.bbox,
                contract: "source-image-bbox",
            },
        })
    }
    fn detect_text(&mut self, image: &RgbImage) -> Result<Vec<(Rect, f32)>> {
        let (data, (h, w)) = db_tensor(image)?;
        let input = ort::value::Tensor::from_array((vec![1_i64, 3, h as i64, w as i64], data))
            .map_err(ort_error)?;
        let db = self.db.as_mut().expect("initialized DB session");
        let outputs = db.run(ort::inputs![input]).map_err(ort_error)?;
        let arr = outputs[0].try_extract_array::<f32>().map_err(ort_error)?;
        let shape = arr.shape();
        if shape.len() != 4 || shape[0] != 1 || shape[1] != 1 {
            return Err(FukidashiError::TensorContract(format!(
                "DB output must be [1,1,H,W], got {shape:?}"
            )));
        }
        let (ph, pw) = (shape[2], shape[3]);
        let values = arr.iter().copied().collect::<Vec<_>>();
        let mut found = connected_components(&values, pw, ph, 0.3);
        let (sx, sy) = (
            image.width() as f32 / pw as f32,
            image.height() as f32 / ph as f32,
        );
        for (r, _) in &mut found {
            *r = Rect {
                x1: r.x1 * sx,
                y1: r.y1 * sy,
                x2: (r.x2 + 1.0) * sx,
                y2: (r.y2 + 1.0) * sy,
            };
        }
        found.retain(|(r, _)| r.x2 - r.x1 >= 4.0 && r.y2 - r.y1 >= 4.0);
        found.sort_by(|a, b| {
            a.0.y1
                .total_cmp(&b.0.y1)
                .then_with(|| a.0.x1.total_cmp(&b.0.x1))
        });
        Ok(found)
    }

    fn detect_rtdetr(&mut self, image: &RgbImage) -> Result<Vec<Detection>> {
        let resized = DynamicImage::ImageRgb8(image.clone())
            .resize_exact(640, 640, FilterType::CatmullRom)
            .to_rgb8();
        let mut data = vec![0.0f32; 3 * 640 * 640];
        for (y, row) in resized.rows().enumerate() {
            for (x, pixel) in row.enumerate() {
                let i = y * 640 + x;
                data[i] = pixel[0] as f32 / 255.0;
                data[640 * 640 + i] = pixel[1] as f32 / 255.0;
                data[2 * 640 * 640 + i] = pixel[2] as f32 / 255.0;
            }
        }
        let images =
            ort::value::Tensor::from_array((vec![1_i64, 3, 640, 640], data)).map_err(ort_error)?;
        let sizes = ort::value::Tensor::from_array((
            vec![1_i64, 2],
            vec![image.width() as i64, image.height() as i64],
        ))
        .map_err(ort_error)?;
        let session = self
            .detector
            .as_mut()
            .expect("initialized detector session");
        let outputs = session
            .run(ort::inputs![images, sizes])
            .map_err(ort_error)?;
        let labels = outputs[0].try_extract_array::<i64>().map_err(ort_error)?;
        let boxes = outputs[1].try_extract_array::<f32>().map_err(ort_error)?;
        let scores = outputs[2].try_extract_array::<f32>().map_err(ort_error)?;
        if labels.shape().len() != 2
            || boxes.shape().len() != 3
            || scores.shape().len() != 2
            || labels.shape()[0] != 1
            || boxes.shape()[0] != 1
            || scores.shape()[0] != 1
            || boxes.shape()[1] != labels.shape()[1]
            || scores.shape()[1] != labels.shape()[1]
            || boxes.shape()[2] != 4
        {
            return Err(FukidashiError::TensorContract(format!(
                "RT-DETR outputs must be [1,N], [1,N,4], [1,N], got {:?}, {:?}, {:?}",
                labels.shape(),
                boxes.shape(),
                scores.shape()
            )));
        }
        let mut detections = Vec::new();
        for i in 0..labels.shape()[1] {
            let score = scores[[0, i]];
            let label = labels[[0, i]];
            if !score.is_finite() || score < 0.30 || !matches!(label, 0..=2) {
                continue;
            }
            let Some(bbox) = (Rect {
                x1: boxes[[0, i, 0]],
                y1: boxes[[0, i, 1]],
                x2: boxes[[0, i, 2]],
                y2: boxes[[0, i, 3]],
            })
            .clip(image.width() as f32, image.height() as f32) else {
                continue;
            };
            if bbox.validate().is_ok() {
                detections.push(Detection { label, bbox, score });
            }
        }
        dedup_detections(detections)
    }
    fn detect_script(&mut self, crop: &RgbImage) -> Result<(String, f32, f32)> {
        let (data, w) = osd_tensor(crop)?;
        let input = ort::value::Tensor::from_array((vec![1_i64, 1, 48, w as i64], data))
            .map_err(ort_error)?;
        let osd = self.osd.as_mut().expect("initialized OSD session");
        let outputs = osd.run(ort::inputs![input]).map_err(ort_error)?;
        let arr = outputs[0].try_extract_array::<f32>().map_err(ort_error)?;
        dominant_script(
            arr.as_slice().ok_or_else(|| {
                FukidashiError::TensorContract("OSD output is not contiguous".into())
            })?,
            arr.shape(),
            &self.osd_labels,
        )
    }
    fn recognize(
        &mut self,
        config: &Config,
        kind: RecognizerKind,
        crop: &RgbImage,
    ) -> Result<(String, f32)> {
        let (data, h, w) = pp_tensor(crop)?;
        let input = ort::value::Tensor::from_array((vec![1_i64, 3, h as i64, w as i64], data))
            .map_err(ort_error)?;
        let rec = self.recognizer(config, kind)?;
        let outputs = rec.session.run(ort::inputs![input]).map_err(ort_error)?;
        let arr = outputs[0].try_extract_array::<f32>().map_err(ort_error)?;
        decode_ctc(
            arr.as_slice().ok_or_else(|| {
                FukidashiError::TensorContract("recognizer output is not contiguous".into())
            })?,
            arr.shape(),
            &rec.chars,
        )
    }
    #[allow(clippy::identity_op)]
    fn recognize_manga(&mut self, config: &Config, crop: &RgbImage) -> Result<(String, f32)> {
        macro_rules! logits {
            ($value:expr, $vocab_len:expr) => {{
                let logits = $value.try_extract_array::<f32>().map_err(ort_error)?;
                if logits.shape() != [1, $vocab_len]
                    || logits.iter().any(|value| !value.is_finite())
                {
                    return Err(FukidashiError::TensorContract(format!(
                        "Manga decoder logits must be [1,{}] with finite values, got {:?}",
                        $vocab_len,
                        logits.shape()
                    )));
                }
                let row = logits.as_slice().ok_or_else(|| {
                    FukidashiError::TensorContract("Manga logits are not contiguous".into())
                })?;
                let (index, &value) = row
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .expect("non-empty Manga vocabulary");
                let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let denom = row.iter().map(|value| (value - max).exp()).sum::<f32>();
                (index, ((value - max).exp() / denom).clamp(0.0, 1.0))
            }};
        }
        let data = manga_tensor(crop)?;
        let image =
            ort::value::Tensor::from_array((vec![1_i64, 3, 224, 224], data)).map_err(ort_error)?;
        let manga = self.manga_recognizer(config)?;
        let encoder_outputs = manga.encoder.run(ort::inputs![image]).map_err(ort_error)?;
        let hidden = encoder_outputs[0]
            .try_extract_array::<f32>()
            .map_err(ort_error)?;
        if hidden.shape() != [1, 196, 256] {
            return Err(FukidashiError::TensorContract(format!(
                "Manga encoder output must be [1,196,256], got {:?}",
                hidden.shape()
            )));
        }
        let hidden_data = hidden.iter().copied().collect::<Vec<_>>();
        let hidden_tensor =
            ort::value::Tensor::from_array((vec![1_i64, 196, 256], hidden_data.clone()))
                .map_err(ort_error)?;
        let token_tensor =
            ort::value::Tensor::from_array((vec![1_i64, 1], vec![2_i64])).map_err(ort_error)?;
        let init_outputs = manga
            .decoder_init
            .run(ort::inputs![hidden_tensor, token_tensor])
            .map_err(ort_error)?;
        let vocab_len = manga.vocab.len();
        let (mut next_token, mut confidence) = logits!(&init_outputs[0], vocab_len);
        let mut tokens = vec![2_i64];
        let mut confidences = Vec::new();
        if next_token != 3 {
            tokens.push(next_token as i64);
            confidences.push(confidence);
        }
        let mut self_k = init_outputs[1]
            .try_extract_array::<f32>()
            .map_err(ort_error)?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let mut self_v = init_outputs[2]
            .try_extract_array::<f32>()
            .map_err(ort_error)?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let cross_k = init_outputs[3]
            .try_extract_array::<f32>()
            .map_err(ort_error)?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let cross_v = init_outputs[4]
            .try_extract_array::<f32>()
            .map_err(ort_error)?
            .iter()
            .copied()
            .collect::<Vec<_>>();
        if self_k.len() != 4 * 1 * 4 * 1 * 64
            || self_v.len() != 4 * 1 * 4 * 1 * 64
            || cross_k.len() != 4 * 1 * 4 * 196 * 64
            || cross_v.len() != 4 * 1 * 4 * 196 * 64
        {
            return Err(FukidashiError::TensorContract(
                "Manga decoder cache output shape is incompatible".into(),
            ));
        }
        self_k.resize(4 * 1 * 4 * 256 * 64, 0.0);
        self_v.resize(4 * 1 * 4 * 256 * 64, 0.0);
        let mut cache_len = 1usize;
        while next_token != 3 && tokens.len() < 256 && cache_len < 256 {
            let token = ort::value::Tensor::from_array((vec![1_i64, 1], vec![next_token as i64]))
                .map_err(ort_error)?;
            // Matches upstream mobile Manga OCR: position IDs are offset by
            // one from the cache slot and capped at the exported model's
            // maximum position embedding.
            let position = ort::value::Tensor::from_array((
                vec![1_i64, 1],
                vec![manga_position_id(cache_len) as i64],
            ))
            .map_err(ort_error)?;
            let hidden =
                ort::value::Tensor::from_array((vec![1_i64, 196, 256], hidden_data.clone()))
                    .map_err(ort_error)?;
            let self_k_input =
                ort::value::Tensor::from_array((vec![4_i64, 1, 4, 256, 64], self_k.clone()))
                    .map_err(ort_error)?;
            let self_v_input =
                ort::value::Tensor::from_array((vec![4_i64, 1, 4, 256, 64], self_v.clone()))
                    .map_err(ort_error)?;
            let cross_k_input =
                ort::value::Tensor::from_array((vec![4_i64, 1, 4, 196, 64], cross_k.clone()))
                    .map_err(ort_error)?;
            let cross_v_input =
                ort::value::Tensor::from_array((vec![4_i64, 1, 4, 196, 64], cross_v.clone()))
                    .map_err(ort_error)?;
            let outputs = manga
                .decoder_step
                .run(ort::inputs![
                    hidden,
                    token,
                    position,
                    self_k_input,
                    self_v_input,
                    cross_k_input,
                    cross_v_input
                ])
                .map_err(ort_error)?;
            let step_k = outputs[1]
                .try_extract_array::<f32>()
                .map_err(ort_error)?
                .iter()
                .copied()
                .collect::<Vec<_>>();
            let step_v = outputs[2]
                .try_extract_array::<f32>()
                .map_err(ort_error)?
                .iter()
                .copied()
                .collect::<Vec<_>>();
            if step_k.len() != 4 * 1 * 4 * 1 * 64 || step_v.len() != 4 * 1 * 4 * 1 * 64 {
                return Err(FukidashiError::TensorContract(
                    "Manga decoder step cache output shape is incompatible".into(),
                ));
            }
            copy_cache_slice(&mut self_k, &step_k, cache_len)?;
            copy_cache_slice(&mut self_v, &step_v, cache_len)?;
            (next_token, confidence) = logits!(&outputs[0], vocab_len);
            if next_token == 3 {
                break;
            }
            tokens.push(next_token as i64);
            confidences.push(confidence);
            cache_len += 1;
        }
        let text = tokens
            .into_iter()
            .filter(|id| *id >= 5)
            .filter_map(|id| manga.vocab.get(id as usize).cloned())
            .collect::<String>();
        let text = text.split_whitespace().collect::<String>();
        let confidence = if confidences.is_empty() {
            0.0
        } else {
            confidences.iter().sum::<f32>() / confidences.len() as f32
        };
        Ok((text, confidence.clamp(0.0, 1.0)))
    }

    fn recognize_baberu(&mut self, config: &Config, crop: &RgbImage) -> Result<(String, f32)> {
        const VOCAB_SIZE: usize = 14_630;
        const MAX_NEW_TOKENS: usize = 128;
        const REPETITION_PENALTY: f64 = 1.2;
        const MAX_CONTENT_RUN: usize = 12;
        let data = baberu_tensor(crop)?;
        let pixel_values =
            ort::value::Tensor::from_array((vec![1_i64, 3, 224, 224], data)).map_err(ort_error)?;
        let baberu = self.baberu_recognizer(config)?;
        let vision_outputs = baberu
            .vision
            .run(ort::inputs! { "pixel_values" => pixel_values })
            .map_err(ort_error)?;
        let vision = vision_outputs[0]
            .try_extract_array::<f32>()
            .map_err(ort_error)?;
        if vision.shape() != [1, 256, 512] || vision.iter().any(|value| !value.is_finite()) {
            return Err(FukidashiError::TensorContract(format!(
                "Baberu vision output must be [1,256,512] and finite, got {:?}",
                vision.shape()
            )));
        }
        let vision_data = vision.iter().copied().collect::<Vec<_>>();
        let hidden = ort::value::Tensor::from_array((vec![1_i64, 256, 512], vision_data))
            .map_err(ort_error)?;
        let bos =
            ort::value::Tensor::from_array((vec![1_i64, 1], vec![1_i64])).map_err(ort_error)?;
        let prefill = baberu
            .decoder_prefill
            .run(ort::inputs! {
                "vision_embeds" => hidden,
                "input_ids" => bos,
            })
            .map_err(ort_error)?;
        if prefill.len() != 13 {
            return Err(FukidashiError::TensorContract(format!(
                "Baberu prefill must return 13 tensors, got {}",
                prefill.len()
            )));
        }
        let mut logits = baberu_logits(&prefill[0], &[1, 257, VOCAB_SIZE])?;
        let mut caches = Vec::with_capacity(12);
        for output in prefill.values().skip(1) {
            caches.push(baberu_cache(&output, 257)?);
        }
        let mut sequence = vec![1usize];
        let mut tokens = Vec::with_capacity(MAX_NEW_TOKENS);
        let mut confidences = Vec::with_capacity(MAX_NEW_TOKENS);
        for (position, cache_len) in (257_i64..).zip(257_usize..).take(MAX_NEW_TOKENS) {
            for token in sequence
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
            {
                let score = logits[token];
                logits[token] = if score < 0.0 {
                    score * REPETITION_PENALTY
                } else {
                    score / REPETITION_PENALTY
                };
            }
            if let Some(&last) = tokens.last()
                && baberu.content_ids.get(last).copied().unwrap_or(false)
            {
                let run = tokens
                    .iter()
                    .rev()
                    .take_while(|token| **token == last)
                    .count();
                if run >= MAX_CONTENT_RUN {
                    logits[last] = f64::NEG_INFINITY;
                }
            }
            let (next, confidence) = baberu_next_token(&logits)?;
            if next == 2 {
                break;
            }
            tokens.push(next);
            sequence.push(next);
            confidences.push(confidence);
            if tokens.len() >= MAX_NEW_TOKENS {
                break;
            }
            let token = ort::value::Tensor::from_array((vec![1_i64, 1], vec![next as i64]))
                .map_err(ort_error)?;
            let position_tensor = ort::value::Tensor::from_array((vec![1_i64, 1], vec![position]))
                .map_err(ort_error)?;
            let step = baberu
                .decoder_step
                .run(ort::inputs![
                    token,
                    position_tensor,
                    baberu_cache_tensor(&caches[0], cache_len)?,
                    baberu_cache_tensor(&caches[1], cache_len)?,
                    baberu_cache_tensor(&caches[2], cache_len)?,
                    baberu_cache_tensor(&caches[3], cache_len)?,
                    baberu_cache_tensor(&caches[4], cache_len)?,
                    baberu_cache_tensor(&caches[5], cache_len)?,
                    baberu_cache_tensor(&caches[6], cache_len)?,
                    baberu_cache_tensor(&caches[7], cache_len)?,
                    baberu_cache_tensor(&caches[8], cache_len)?,
                    baberu_cache_tensor(&caches[9], cache_len)?,
                    baberu_cache_tensor(&caches[10], cache_len)?,
                    baberu_cache_tensor(&caches[11], cache_len)?,
                ])
                .map_err(ort_error)?;
            if step.len() != 13 {
                return Err(FukidashiError::TensorContract(format!(
                    "Baberu decoder step must return 13 tensors, got {}",
                    step.len()
                )));
            }
            logits = baberu_logits(&step[0], &[1, 1, VOCAB_SIZE])?;
            caches.clear();
            for output in step.values().skip(1) {
                caches.push(baberu_cache(&output, cache_len + 1)?);
            }
        }
        let text = tokens
            .into_iter()
            .filter(|id| *id >= 4)
            .filter_map(|id| baberu.vocab.get(id - 4).cloned())
            .collect::<String>();
        let confidence = if confidences.is_empty() {
            0.0
        } else {
            confidences.iter().sum::<f32>() / confidences.len() as f32
        };
        Ok((text, confidence.clamp(0.0, 1.0)))
    }

    /// OSD is intentionally treated as routing evidence, not a guaranteed
    /// language classifier. When automatic routing is requested, run the
    /// bounded set of installed recognizers and select text whose characters
    /// agree with the candidate script before considering confidence.
    #[allow(clippy::too_many_arguments)]
    fn recognize_auto(
        &mut self,
        config: &Config,
        hinted: RecognizerKind,
        crop: &RgbImage,
        page_prior: Option<RecognizerKind>,
        script: &str,
        script_confidence: f32,
        prefer_baberu: bool,
    ) -> Result<(String, f32, RecognizerKind)> {
        let baberu_supported = matches!(
            hinted,
            RecognizerKind::English
                | RecognizerKind::Latin
                | RecognizerKind::Han
                | RecognizerKind::Manga
        ) || matches!(
            page_prior,
            Some(
                RecognizerKind::English
                    | RecognizerKind::Latin
                    | RecognizerKind::Han
                    | RecognizerKind::Manga
            )
        );
        if prefer_baberu && Self::baberu_assets_available(config) && baberu_supported {
            let (text, confidence) = self.recognize_baberu(config, crop)?;
            return Ok((text, confidence, RecognizerKind::Baberu));
        }
        let mut candidates = Vec::new();
        let japanese = strong_japanese_evidence(script, script_confidence);
        let han = strong_han_evidence(script, script_confidence);
        let mut kinds = Vec::new();
        // Uncalibrated recognizer scores are not comparable. Keep Han and
        // Manga out of Latin pages so mojibake cannot change page language.
        match page_prior.unwrap_or(hinted) {
            RecognizerKind::English | RecognizerKind::Latin => {
                kinds.extend([RecognizerKind::English, RecognizerKind::Latin]);
                if japanese {
                    kinds.clear();
                    kinds.extend([RecognizerKind::Manga, RecognizerKind::Han]);
                }
            }
            RecognizerKind::Manga => {
                if japanese {
                    kinds.extend([RecognizerKind::Manga, RecognizerKind::Han]);
                } else {
                    kinds.extend([RecognizerKind::English, RecognizerKind::Latin]);
                }
            }
            RecognizerKind::Han => {
                if han || japanese {
                    kinds.push(RecognizerKind::Han);
                    if japanese {
                        kinds.insert(0, RecognizerKind::Manga);
                    }
                } else {
                    kinds.extend([RecognizerKind::English, RecognizerKind::Latin]);
                }
            }
            RecognizerKind::Korean => kinds.push(RecognizerKind::Korean),
            RecognizerKind::Baberu => {
                kinds.extend([RecognizerKind::English, RecognizerKind::Latin])
            }
        }
        if hinted == RecognizerKind::Manga && japanese && !kinds.contains(&RecognizerKind::Manga) {
            kinds.insert(0, RecognizerKind::Manga);
        }
        if !Pipeline::manga_assets_available(config) {
            kinds.retain(|kind| *kind != RecognizerKind::Manga);
        }
        if kinds.is_empty() {
            kinds.push(RecognizerKind::Latin);
        }
        for kind in kinds {
            if candidates
                .iter()
                .any(|(k, _, _): &(RecognizerKind, String, f32)| *k == kind)
            {
                continue;
            }
            let (text, confidence) = match kind {
                RecognizerKind::Manga => self.recognize_manga(config, crop)?,
                RecognizerKind::Baberu => self.recognize_baberu(config, crop)?,
                _ => self.recognize(config, kind, crop)?,
            };
            candidates.push((kind, text, confidence));
        }
        if let Some(prior) = page_prior {
            let preferred = candidates.iter().find(|(kind, _, _)| *kind == prior);
            if prior == RecognizerKind::English
                && let Some((kind, text, confidence)) = preferred
            {
                return Ok((text.clone(), *confidence, *kind));
            }
            let alternate_native = candidates
                .iter()
                .any(|(kind, text, _)| *kind != prior && native_script_chars(*kind, text) >= 4);
            if let Some((kind, text, confidence)) = preferred
                && !alternate_native
            {
                return Ok((text.clone(), *confidence, *kind));
            }
        }
        candidates.sort_by(|a, b| {
            let prior = |kind| u8::from(page_prior == Some(kind));
            (b.2 + f32::from(prior(b.0)) * 0.20)
                .total_cmp(&(a.2 + f32::from(prior(a.0)) * 0.20))
                .then_with(|| prior(b.0).cmp(&prior(a.0)))
                .then_with(|| script_match(b.0, &b.1).cmp(&script_match(a.0, &a.1)))
        });
        let (kind, text, confidence) = candidates
            .into_iter()
            .next()
            .expect("candidate set is nonempty");
        Ok((text, confidence, kind))
    }

    fn page_route_prior(
        &mut self,
        config: &Config,
        image: &RgbImage,
        lines: &[Detection],
    ) -> Result<Option<RecognizerKind>> {
        if lines.is_empty() {
            return Ok(None);
        }
        let mut script_hits = HashMap::<RecognizerKind, usize>::new();
        for line in lines.iter().take(8) {
            let crop = crop_rgb(image, line.bbox)?;
            let (script, confidence, _) = self.detect_script(&crop)?;
            let lowered = script.to_ascii_lowercase();
            if strong_japanese_evidence(&script, confidence) {
                *script_hits.entry(RecognizerKind::Manga).or_default() += 1;
            } else if strong_han_evidence(&script, confidence) {
                *script_hits.entry(RecognizerKind::Han).or_default() += 1;
            } else if lowered.starts_with("hangul") {
                *script_hits.entry(RecognizerKind::Korean).or_default() += 1;
            }
        }
        if let Some((kind, hits)) = script_hits.iter().max_by_key(|(_, hits)| **hits) {
            // One isolated non-Latin OSD result is insufficient to overturn a
            // page with Latin evidence; repeated evidence is meaningful.
            if *hits >= 2 {
                return Ok(Some(
                    if *kind == RecognizerKind::Manga && !Self::manga_assets_available(config) {
                        RecognizerKind::Han
                    } else {
                        *kind
                    },
                ));
            }
        }
        let mut scores = HashMap::<RecognizerKind, f32>::new();
        for line in lines.iter().take(4) {
            let crop = crop_rgb(image, line.bbox)?;
            for kind in [RecognizerKind::English, RecognizerKind::Latin] {
                let (text, confidence) = self.recognize(config, kind, &crop)?;
                let native = native_script_chars(kind, &text);
                let alphanumeric = text.chars().filter(|c| c.is_alphanumeric()).count();
                let quality = if text.trim().is_empty() {
                    0.1
                } else if kind == RecognizerKind::English {
                    english_evidence(&text)
                } else {
                    // Latin is the honest fallback for scripts outside the
                    // dedicated English word evidence set.
                    let _ = (native, alphanumeric);
                    0.8
                };
                *scores.entry(kind).or_default() += confidence * quality;
            }
        }
        Ok(scores
            .into_iter()
            .max_by(|(ka, a), (kb, b)| a.total_cmp(b).then_with(|| kb.as_str().cmp(ka.as_str())))
            .map(|(kind, _)| kind))
    }
}

#[cfg(feature = "onnx")]
fn english_evidence(text: &str) -> f32 {
    let lowered = text.to_ascii_lowercase();
    let words = lowered
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.is_empty() {
        return 0.1;
    }
    let common = [
        "a", "an", "and", "are", "be", "been", "bro", "cock", "did", "for", "forgot", "growing",
        "hello", "i", "is", "it", "later", "my", "or", "so", "some", "stop", "the", "thing", "won",
        "world", "years",
    ];
    let matches = words.iter().filter(|word| common.contains(word)).count();
    let ascii_ratio = text.chars().filter(|c| c.is_ascii_alphabetic()).count() as f32
        / text.chars().filter(|c| c.is_alphanumeric()).count().max(1) as f32;
    if matches > 0 {
        (0.75 + 0.15 * (matches as f32 / words.len() as f32) + 0.1 * ascii_ratio).min(1.0)
    } else if ascii_ratio >= 0.8 && words.len() >= 2 {
        0.55
    } else {
        0.2
    }
}

#[cfg(feature = "onnx")]
fn db_tensor(image: &RgbImage) -> Result<(Vec<f32>, (usize, usize))> {
    let (h, w, _, _) =
        crate::vision::preprocess::db_dimensions(image.height(), image.width(), 960, "min")?;
    if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        return Err(FukidashiError::ResourceLimit(
            "detector tensor exceeds OCR pixel limit".into(),
        ));
    }
    let resized = DynamicImage::ImageRgb8(image.clone())
        .resize_exact(w, h, FilterType::CatmullRom)
        .to_rgb8();
    let (wu, hu) = (w as usize, h as usize);
    let mut out = vec![0.0; 3 * wu * hu];
    for (y, row) in resized.rows().enumerate() {
        for (x, p) in row.enumerate() {
            let i = y * wu + x;
            out[i] = (p[2] as f32 / 255.0 - 0.5) / 0.5;
            out[wu * hu + i] = (p[1] as f32 / 255.0 - 0.5) / 0.5;
            out[2 * wu * hu + i] = (p[0] as f32 / 255.0 - 0.5) / 0.5;
        }
    }
    Ok((out, (hu, wu)))
}
#[cfg(feature = "onnx")]
fn pp_tensor(image: &RgbImage) -> Result<(Vec<f32>, usize, usize)> {
    if image.width() == 0 || image.height() == 0 {
        return Err(FukidashiError::InvalidInput(
            "OCR crop dimensions must be nonzero".into(),
        ));
    }
    let h = 48usize;
    let w = ((image.width() as f32 / image.height() as f32 * h as f32).ceil() as usize).max(8);
    if w > MAX_RECOGNIZER_WIDTH || h * w > MAX_IMAGE_PIXELS as usize {
        return Err(FukidashiError::ResourceLimit(
            "OCR crop exceeds recognizer tensor limit".into(),
        ));
    }
    let resized = DynamicImage::ImageRgb8(image.clone())
        .resize_exact(w as u32, h as u32, FilterType::CatmullRom)
        .to_rgb8();
    let mut out = vec![0.0; 3 * h * w];
    for (y, row) in resized.rows().enumerate() {
        for (x, p) in row.enumerate() {
            let i = y * w + x;
            out[i] = (p[2] as f32 / 255.0 - 0.5) / 0.5;
            out[h * w + i] = (p[1] as f32 / 255.0 - 0.5) / 0.5;
            out[2 * h * w + i] = (p[0] as f32 / 255.0 - 0.5) / 0.5;
        }
    }
    Ok((out, h, w))
}
#[cfg(feature = "onnx")]
fn manga_tensor(image: &RgbImage) -> Result<Vec<f32>> {
    if image.width() == 0 || image.height() == 0 {
        return Err(FukidashiError::InvalidInput(
            "Manga OCR crop dimensions must be nonzero".into(),
        ));
    }
    let scale = (224.0 / image.width() as f32).min(224.0 / image.height() as f32);
    // The mobile export follows PIL's bilinear resize and floor dimensions.
    let width = (image.width() as f32 * scale).floor().clamp(1.0, 224.0) as u32;
    let height = (image.height() as f32 * scale).floor().clamp(1.0, 224.0) as u32;
    let resized = image::imageops::resize(image, width, height, FilterType::Triangle);
    let mut out = vec![1.0f32; 3 * 224 * 224];
    let x_offset = ((224 - width) / 2) as usize;
    let y_offset = ((224 - height) / 2) as usize;
    for (y, row) in resized.rows().enumerate() {
        for (x, pixel) in row.enumerate() {
            let index = (y + y_offset) * 224 + x + x_offset;
            out[index] = pixel[0] as f32 / 255.0;
            out[224 * 224 + index] = pixel[1] as f32 / 255.0;
            out[2 * 224 * 224 + index] = pixel[2] as f32 / 255.0;
        }
    }
    Ok(out)
}
#[cfg(feature = "onnx")]
fn manga_position_id(cache_len: usize) -> usize {
    (cache_len + 1).min(127)
}
#[cfg(feature = "onnx")]
#[allow(clippy::identity_op)]
fn copy_cache_slice(cache: &mut [f32], slice: &[f32], position: usize) -> Result<()> {
    let stride = 4 * 1 * 4 * 64;
    let offset = position * stride;
    if slice.len() != stride || offset + stride > cache.len() {
        return Err(FukidashiError::TensorContract(
            "Manga cache slice is out of bounds".into(),
        ));
    }
    for layer in 0..4 {
        for batch in 0..1 {
            for head in 0..4 {
                let source = (layer * 1 * 4 + batch * 4 + head) * 64;
                let target = ((layer * 1 * 4 + batch * 4 + head) * 256 + position) * 64;
                cache[target..target + 64].copy_from_slice(&slice[source..source + 64]);
            }
        }
    }
    Ok(())
}
#[cfg(feature = "onnx")]
fn osd_tensor(image: &RgbImage) -> Result<(Vec<f32>, usize)> {
    if image.width() == 0 || image.height() == 0 {
        return Err(FukidashiError::InvalidInput(
            "OSD crop dimensions must be nonzero".into(),
        ));
    }
    let h = 48usize;
    let w = ((image.width() as f32 / image.height() as f32 * h as f32).round() as usize).max(3);
    if w > MAX_RECOGNIZER_WIDTH || h * w > MAX_IMAGE_PIXELS as usize {
        return Err(FukidashiError::ResourceLimit(
            "OSD crop exceeds tensor limit".into(),
        ));
    }
    let resized = DynamicImage::ImageRgb8(image.clone())
        .resize_exact(w as u32, h as u32, FilterType::CatmullRom)
        .to_rgb8();
    let mut out = vec![0.0; h * w];
    for (y, row) in resized.rows().enumerate() {
        for (x, p) in row.enumerate() {
            out[y * w + x] =
                (0.3 * p[0] as f32 + 0.5 * p[1] as f32 + 0.2 * p[2] as f32 + 0.5).floor();
        }
    }
    let mut sorted = out.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2];
    if median < 64.0 {
        for value in &mut out {
            *value = 255.0 - *value;
        }
    }
    let mut mins = [0u32; 256];
    let mut maxes = [0u32; 256];
    if w >= 3 {
        let y = h / 2;
        for x in 1..w - 1 {
            let prev = out[y * w + x - 1].round().clamp(0.0, 255.0) as usize;
            let current = out[y * w + x].round().clamp(0.0, 255.0) as usize;
            let next = out[y * w + x + 1].round().clamp(0.0, 255.0) as usize;
            if (current < prev && current <= next) || (current <= prev && current < next) {
                mins[current] += 1;
            }
            if (current > prev && current >= next) || (current >= prev && current > next) {
                maxes[current] += 1;
            }
        }
    }
    if mins.iter().sum::<u32>() == 0 {
        mins[0] = 1;
    }
    if maxes.iter().sum::<u32>() == 0 {
        maxes[255] = 1;
    }
    let black = percentile(&mins, 0.25);
    let white = percentile(&maxes, 0.75);
    let contrast = if (white - black) / 2.0 <= 0.0 {
        1.0
    } else {
        (white - black) / 2.0
    };
    for value in &mut out {
        *value = *value / contrast - black / contrast - 1.0;
    }
    Ok((out, w))
}

#[cfg(feature = "onnx")]
fn percentile(buckets: &[u32; 256], frac: f32) -> f32 {
    let total: u32 = buckets.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let target = (frac * total as f32).clamp(1.0, total as f32);
    let mut sum = 0u32;
    for (index, count) in buckets.iter().enumerate() {
        sum += *count;
        if sum as f32 >= target {
            return index as f32 - (sum as f32 - target) / (*count).max(1) as f32;
        }
    }
    255.0
}
#[cfg(feature = "onnx")]
fn crop_rgb(image: &RgbImage, r: Rect) -> Result<RgbImage> {
    let (x1, y1) = (r.x1.floor().max(0.0) as u32, r.y1.floor().max(0.0) as u32);
    let (x2, y2) = (
        r.x2.ceil().min(image.width() as f32) as u32,
        r.y2.ceil().min(image.height() as f32) as u32,
    );
    if x2 <= x1 || y2 <= y1 {
        return Err(FukidashiError::InvalidInput(
            "detector returned an empty region".into(),
        ));
    }
    Ok(image::imageops::crop_imm(image, x1, y1, x2 - x1, y2 - y1).to_image())
}
#[cfg(feature = "onnx")]
fn connected_components(prob: &[f32], w: usize, h: usize, t: f32) -> Vec<(Rect, f32)> {
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    for sy in 0..h {
        for sx in 0..w {
            let start = sy * w + sx;
            if seen[start] || prob[start] <= t {
                continue;
            }
            let mut q = vec![start];
            seen[start] = true;
            let (mut n, mut x1, mut y1, mut x2, mut y2, mut sum) = (0usize, sx, sy, sx, sy, 0.0f32);
            while let Some(i) = q.pop() {
                let (x, y) = (i % w, i / w);
                n += 1;
                sum += prob[i];
                x1 = x1.min(x);
                x2 = x2.max(x);
                y1 = y1.min(y);
                y2 = y2.max(y);
                for (nx, ny) in [
                    (x.wrapping_sub(1), y),
                    (x + 1, y),
                    (x, y.wrapping_sub(1)),
                    (x, y + 1),
                ] {
                    if nx < w && ny < h {
                        let j = ny * w + nx;
                        if !seen[j] && prob[j] > t {
                            seen[j] = true;
                            q.push(j);
                        }
                    }
                }
            }
            if n >= 4 && sum / n as f32 >= 0.5 {
                out.push((
                    Rect {
                        x1: x1.saturating_sub(2) as f32,
                        y1: y1.saturating_sub(2) as f32,
                        x2: (x2 + 2).min(w.saturating_sub(1)) as f32,
                        y2: (y2 + 2).min(h.saturating_sub(1)) as f32,
                    },
                    (sum / n as f32).clamp(0.0, 1.0),
                ));
            }
        }
    }
    out
}
#[cfg(feature = "onnx")]
fn dominant_script(
    scores: &[f32],
    shape: &[usize],
    osd_labels: &[String],
) -> Result<(String, f32, f32)> {
    if shape.len() != 2 || shape[1] != 79 || scores.len() != shape[0] * shape[1] {
        return Err(FukidashiError::TensorContract(format!(
            "OSD output must be [T,79], got {shape:?}"
        )));
    }
    let mut counts = HashMap::<usize, (usize, f32)>::new();
    let mut prev = 2usize;
    for t in 0..shape[0] {
        let row = &scores[t * 79..(t + 1) * 79];
        let (idx, &v) = row
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        if idx != 2 && idx != prev {
            let e = counts.entry(idx).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += v;
        }
        prev = idx;
    }
    let (idx, (n, s)) = match counts.into_iter().max_by_key(|(_, v)| v.0) {
        Some(v) => v,
        None => return Ok((String::new(), 0.0, 1.0)),
    };
    let labels = [
        "NULL",
        "Joined",
        "Broken",
        "Arabic",
        "Arabic-dn",
        "Armenian",
        "Armenian-dn",
        "Bengali",
        "Bengali-dn",
        "Canadian_Aboriginal",
        "Canadian_Aboriginal-dn",
        "Cherokee",
        "Cherokee-dn",
        "Cyrillic",
        "Cyrillic-dn",
        "Devanagari",
        "Devanagari-dn",
        "Ethiopic",
        "Ethiopic-dn",
        "Fraktur",
        "Fraktur-dn",
        "Georgian",
        "Georgian-dn",
        "Greek",
        "Greek-dn",
        "Gujarati",
        "Gujarati-dn",
        "Gurmukhi",
        "Gurmukhi-dn",
        "Hangul",
        "Hangul-dn",
        "Hangul_vert",
        "Hangul_vert-dn",
        "HanS",
        "HanS-dn",
        "HanS_vert",
        "HanS_vert-dn",
        "HanT",
        "HanT-dn",
        "HanT_vert",
        "HanT_vert-dn",
        "Hebrew",
        "Hebrew-dn",
        "Japanese",
        "Japanese-dn",
        "Japanese_vert",
        "Japanese_vert-dn",
        "Kannada",
        "Kannada-dn",
        "Khmer",
        "Khmer-dn",
        "Lao",
        "Lao-dn",
        "Latin",
        "Latin-dn",
        "Malayalam",
        "Malayalam-dn",
        "Myanmar",
        "Myanmar-dn",
        "Oriya",
        "Oriya-dn",
        "Sinhala",
        "Sinhala-dn",
        "Syriac",
        "Syriac-dn",
        "Tamil",
        "Tamil-dn",
        "Telugu",
        "Telugu-dn",
        "Thai",
        "Thai-dn",
        "Tibetan",
        "Tibetan-dn",
        "Vietnamese",
        "Vietnamese-dn",
        "Common",
        "Common-dn",
    ];
    let c = (s / n as f32).clamp(0.0, 1.0);
    let label = osd_labels
        .get(idx)
        .map(String::as_str)
        .or_else(|| labels.get(idx).copied())
        .unwrap_or("Common");
    Ok((label.to_string(), c, 1.0 - c))
}
#[cfg(feature = "onnx")]
fn decode_ctc(v: &[f32], shape: &[usize], chars: &[String]) -> Result<(String, f32)> {
    if shape.len() != 3 || shape[0] != 1 {
        return Err(FukidashiError::TensorContract(format!(
            "recognizer output must be [1,T,C], got {shape:?}"
        )));
    }
    let (t, c) = (shape[1], shape[2]);
    if v.len() != t.saturating_mul(c) || v.iter().any(|x| !x.is_finite()) {
        return Err(FukidashiError::TensorContract(
            "recognizer output has invalid buffer or nonfinite logits".into(),
        ));
    }
    if c != chars.len() + 2 {
        return Err(FukidashiError::TensorContract(format!(
            "recognizer class count {c} does not match dictionary {}",
            chars.len()
        )));
    }
    let values_are_probabilities = v.iter().all(|x| (0.0..=1.0).contains(x));
    let (mut text, mut confs, mut prev) = (String::new(), Vec::new(), 0usize);
    for i in 0..t {
        let row = &v[i * c..(i + 1) * c];
        let (idx, &raw) = row
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        let p = if values_are_probabilities {
            let sum: f32 = row.iter().sum();
            if sum <= 0.0 {
                return Err(FukidashiError::TensorContract(
                    "recognizer probability row sums to zero".into(),
                ));
            }
            raw / sum
        } else {
            let m = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            (raw - m).exp() / row.iter().map(|x| (x - m).exp()).sum::<f32>()
        };
        if idx != 0 && idx != prev {
            if idx == c - 1 {
                text.push(' ');
                confs.push(p);
            } else if let Some(ch) = chars.get(idx - 1) {
                text.push_str(ch);
                confs.push(p);
            }
        }
        prev = idx;
    }
    Ok((
        text,
        if confs.is_empty() {
            0.0
        } else {
            (confs.iter().sum::<f32>() / confs.len() as f32).clamp(0.0, 1.0)
        },
    ))
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn normalize_language(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn route_override(s: &str) -> Result<RecognizerKind> {
    match normalize_language(s).as_str() {
        "ja" => Ok(RecognizerKind::Manga),
        "zh" => Ok(RecognizerKind::Han),
        "ko" => Ok(RecognizerKind::Korean),
        "en" => Ok(RecognizerKind::English),
        "latin" => Ok(RecognizerKind::Latin),
        "auto" => Err(FukidashiError::InvalidInput(
            "source_language=auto must be omitted for automatic routing".into(),
        )),
        x => Err(FukidashiError::InvalidInput(format!(
            "unsupported OCR source_language {x:?}; use auto, ja, zh, ko, en, or latin"
        ))),
    }
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn route_script(s: &str) -> RecognizerKind {
    let s = s.to_ascii_lowercase();
    if s.starts_with("hangul") {
        RecognizerKind::Korean
    } else if s.starts_with("japanese") || s == "kana" {
        RecognizerKind::Manga
    } else if s.starts_with("hans") || s.starts_with("hant") {
        RecognizerKind::Han
    } else if s.starts_with("latin") {
        RecognizerKind::Latin
    } else {
        // Common/unknown OSD labels do not justify Han or Manga. Latin is a
        // safe bounded fallback and is refined by the English candidate.
        RecognizerKind::Latin
    }
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn is_japanese_script(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.starts_with("japanese") || s == "kana"
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn strong_japanese_evidence(script: &str, confidence: f32) -> bool {
    is_japanese_script(script) && confidence >= 0.95
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn strong_han_evidence(script: &str, confidence: f32) -> bool {
    let lowered = script.to_ascii_lowercase();
    (lowered.starts_with("hans") || lowered.starts_with("hant")) && confidence >= 0.95
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn preferred_explicit_route(config: &Config, route: RecognizerKind) -> RecognizerKind {
    #[cfg(feature = "onnx")]
    if Pipeline::baberu_assets_available(config)
        && matches!(
            route,
            RecognizerKind::English | RecognizerKind::Han | RecognizerKind::Manga
        )
    {
        return RecognizerKind::Baberu;
    }
    #[cfg(feature = "onnx")]
    if route == RecognizerKind::Manga && !Pipeline::manga_assets_available(config) {
        return RecognizerKind::Han;
    }
    #[cfg(not(feature = "onnx"))]
    let _ = config;
    route
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn inferred_language(script: &str, route: RecognizerKind, text: &str) -> String {
    let s = script.to_ascii_lowercase();
    if route == RecognizerKind::Korean {
        "ko".into()
    } else if route == RecognizerKind::English {
        "en".into()
    } else if route == RecognizerKind::Latin {
        "en|latin".into()
    } else if route == RecognizerKind::Baberu {
        if s.starts_with("hans") {
            "zh-hans".into()
        } else if s.starts_with("hant") {
            "zh-hant".into()
        } else if is_japanese_script(script)
            || text
                .chars()
                .any(|c| ('ぁ'..='ゖ').contains(&c) || ('ァ'..='ヺ').contains(&c))
        {
            "ja".into()
        } else {
            "en".into()
        }
    } else if s.starts_with("japanese") {
        if text
            .chars()
            .any(|c| ('ぁ'..='ゖ').contains(&c) || ('ァ'..='ヺ').contains(&c))
        {
            "ja".into()
        } else {
            "ja|zh".into()
        }
    } else if s.starts_with("hans") {
        "ja|zh-hans".into()
    } else if s.starts_with("hant") {
        "ja|zh-hant".into()
    } else if text
        .chars()
        .any(|c| ('ぁ'..='ゖ').contains(&c) || ('ァ'..='ヺ').contains(&c))
    {
        "ja".into()
    } else {
        match route {
            RecognizerKind::Han => "ja|zh".into(),
            RecognizerKind::Korean => "ko".into(),
            RecognizerKind::Latin => "en|latin".into(),
            RecognizerKind::English => "en".into(),
            RecognizerKind::Manga => "ja".into(),
            RecognizerKind::Baberu => "en".into(),
        }
    }
}
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
fn aggregate_source(b: &[OcrRegion]) -> String {
    let mut set = std::collections::BTreeSet::new();
    for x in b {
        if !x.source_language.is_empty() {
            set.insert(x.source_language.clone());
        }
    }
    if set.is_empty() {
        "und".into()
    } else if set.len() == 1 {
        set.into_iter().next().unwrap()
    } else {
        "mixed".into()
    }
}

#[cfg(feature = "onnx")]
fn rect_iou(a: Rect, b: Rect) -> f32 {
    let x1 = a.x1.max(b.x1);
    let y1 = a.y1.max(b.y1);
    let x2 = a.x2.min(b.x2);
    let y2 = a.y2.min(b.y2);
    let intersection = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    if intersection <= 0.0 {
        return 0.0;
    }
    let area = (a.x2 - a.x1) * (a.y2 - a.y1) + (b.x2 - b.x1) * (b.y2 - b.y1) - intersection;
    if area <= 0.0 {
        0.0
    } else {
        intersection / area
    }
}

#[cfg(feature = "onnx")]
fn line_intersection_fraction(line: Rect, bubble: Rect) -> f32 {
    let x1 = line.x1.max(bubble.x1);
    let y1 = line.y1.max(bubble.y1);
    let x2 = line.x2.min(bubble.x2);
    let y2 = line.y2.min(bubble.y2);
    let intersection = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let area = (line.x2 - line.x1) * (line.y2 - line.y1);
    if area <= 0.0 {
        0.0
    } else {
        intersection / area
    }
}

#[cfg(feature = "onnx")]
fn dedup_detections(mut detections: Vec<Detection>) -> Result<Vec<Detection>> {
    detections.retain(|d| d.bbox.validate().is_ok() && d.score.is_finite());
    detections.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.bbox.y1.total_cmp(&b.bbox.y1))
            .then_with(|| a.bbox.x1.total_cmp(&b.bbox.x1))
    });
    let mut kept = Vec::with_capacity(detections.len());
    for detection in detections {
        if kept.iter().any(|previous: &Detection| {
            previous.label == detection.label && rect_iou(previous.bbox, detection.bbox) > 0.6
        }) {
            continue;
        }
        kept.push(detection);
    }
    kept.sort_by(|a, b| {
        a.bbox
            .y1
            .total_cmp(&b.bbox.y1)
            .then_with(|| a.bbox.x1.total_cmp(&b.bbox.x1))
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| b.score.total_cmp(&a.score))
    });
    Ok(kept)
}

#[cfg(feature = "onnx")]
fn associate_text_lines(bubbles: &[Detection], lines: &[Detection]) -> Vec<Vec<usize>> {
    let mut associations = vec![Vec::new(); bubbles.len()];
    for (line_index, line) in lines.iter().enumerate() {
        let center_x = (line.bbox.x1 + line.bbox.x2) / 2.0;
        let center_y = (line.bbox.y1 + line.bbox.y2) / 2.0;
        let containing = bubbles.iter().enumerate().filter(|(_, bubble)| {
            center_x >= bubble.bbox.x1
                && center_x <= bubble.bbox.x2
                && center_y >= bubble.bbox.y1
                && center_y <= bubble.bbox.y2
        });
        let best = containing
            .max_by(|(ai, a), (bi, b)| {
                line_intersection_fraction(line.bbox, a.bbox)
                    .total_cmp(&line_intersection_fraction(line.bbox, b.bbox))
                    .then_with(|| b.score.total_cmp(&a.score))
                    .then_with(|| bi.cmp(ai))
            })
            .map(|(index, _)| index)
            .or_else(|| {
                bubbles
                    .iter()
                    .enumerate()
                    .map(|(index, bubble)| {
                        (index, line_intersection_fraction(line.bbox, bubble.bbox))
                    })
                    .filter(|(_, fraction)| *fraction >= 0.05)
                    .max_by(|(ai, af), (bi, bf)| af.total_cmp(bf).then_with(|| bi.cmp(ai)))
                    .map(|(index, _)| index)
            });
        if let Some(index) = best {
            associations[index].push(line_index);
        }
    }
    associations
}

#[cfg(feature = "onnx")]
fn sort_regions(bubbles: &mut [OcrRegion]) {
    let han_order = bubbles.iter().all(|b| {
        b.recognizer == RecognizerKind::Han.as_str()
            || b.recognizer == RecognizerKind::Manga.as_str()
    });
    bubbles.sort_by(|a, b| {
        a.bbox
            .y1
            .total_cmp(&b.bbox.y1)
            .then_with(|| {
                if han_order {
                    b.bbox.x1.total_cmp(&a.bbox.x1)
                } else {
                    a.bbox.x1.total_cmp(&b.bbox.x1)
                }
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[cfg(feature = "onnx")]
fn script_match(kind: RecognizerKind, text: &str) -> u8 {
    let (matched, total) = text.chars().fold((0u8, 0u8), |(matched, total), c| {
        let is_match = match kind {
            RecognizerKind::Han => ('\u{3000}'..='\u{9fff}').contains(&c),
            RecognizerKind::Korean => {
                ('\u{1100}'..='\u{11ff}').contains(&c) || ('\u{ac00}'..='\u{d7af}').contains(&c)
            }
            RecognizerKind::Latin | RecognizerKind::English => c.is_ascii_alphabetic(),
            RecognizerKind::Manga => {
                ('\u{3040}'..='\u{30ff}').contains(&c) || ('\u{3400}'..='\u{9fff}').contains(&c)
            }
            RecognizerKind::Baberu => c.is_alphanumeric(),
        };
        (
            matched.saturating_add(u8::from(is_match)),
            total.saturating_add(u8::from(c.is_alphanumeric())),
        )
    });
    if total > 0 && matched * 2 >= total {
        2
    } else if matched > 0 {
        1
    } else {
        0
    }
}

#[cfg(feature = "onnx")]
fn native_script_chars(kind: RecognizerKind, text: &str) -> usize {
    text.chars()
        .filter(|c| match kind {
            RecognizerKind::Han => ('\u{3400}'..='\u{9fff}').contains(c),
            RecognizerKind::Korean => {
                ('\u{1100}'..='\u{11ff}').contains(c) || ('\u{ac00}'..='\u{d7af}').contains(c)
            }
            RecognizerKind::Latin | RecognizerKind::English => c.is_ascii_alphabetic(),
            RecognizerKind::Manga => {
                ('\u{3040}'..='\u{30ff}').contains(c) || ('\u{3400}'..='\u{9fff}').contains(c)
            }
            RecognizerKind::Baberu => c.is_alphanumeric(),
        })
        .count()
}

#[cfg(feature = "onnx")]
fn baberu_tensor(image: &RgbImage) -> Result<Vec<f32>> {
    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];
    let resized = DynamicImage::ImageRgb8(image.clone())
        .resize_exact(224, 224, FilterType::CatmullRom)
        .to_rgb8();
    let mut data = vec![0.0f32; 3 * 224 * 224];
    for (y, row) in resized.rows().enumerate() {
        for (x, pixel) in row.enumerate() {
            let offset = y * 224 + x;
            for channel in 0..3 {
                data[channel * 224 * 224 + offset] =
                    (pixel[channel] as f32 / 255.0 - MEAN[channel]) / STD[channel];
            }
        }
    }
    Ok(data)
}

#[cfg(feature = "onnx")]
fn baberu_logits(value: &ort::value::DynValue, expected: &[usize]) -> Result<Vec<f64>> {
    let logits = value.try_extract_array::<f32>().map_err(ort_error)?;
    if logits.shape() != expected || logits.iter().any(|score| !score.is_finite()) {
        return Err(FukidashiError::TensorContract(format!(
            "Baberu logits must be {:?} and finite, got {:?}",
            expected,
            logits.shape()
        )));
    }
    let width = *expected.last().expect("logits shape has a class dimension");
    let values = logits
        .iter()
        .skip(logits.len() - width)
        .map(|score| f64::from(*score))
        .collect::<Vec<_>>();
    Ok(values)
}

#[cfg(feature = "onnx")]
fn baberu_cache(value: &ort::value::DynValue, sequence_len: usize) -> Result<Vec<f32>> {
    const CACHE_WIDTH: usize = 64;
    let cache = value.try_extract_array::<f32>().map_err(ort_error)?;
    let expected = [1, 2, sequence_len, CACHE_WIDTH];
    if cache.shape() != expected || cache.iter().any(|item| !item.is_finite()) {
        return Err(FukidashiError::TensorContract(format!(
            "Baberu cache must be {:?} and finite, got {:?}",
            expected,
            cache.shape()
        )));
    }
    Ok(cache.iter().copied().collect())
}

#[cfg(feature = "onnx")]
fn baberu_cache_tensor(cache: &[f32], sequence_len: usize) -> Result<ort::value::Tensor<f32>> {
    if !baberu_cache_length_valid(cache.len(), sequence_len) {
        return Err(FukidashiError::TensorContract(
            "Baberu cache tensor length is incompatible".into(),
        ));
    }
    ort::value::Tensor::from_array((vec![1_i64, 2, sequence_len as i64, 64], cache.to_vec()))
        .map_err(ort_error)
}

#[cfg(feature = "onnx")]
fn baberu_cache_length_valid(cache_len: usize, sequence_len: usize) -> bool {
    sequence_len > 0 && cache_len == 2 * sequence_len * 64
}

#[cfg(feature = "onnx")]
fn baberu_next_token(logits: &[f64]) -> Result<(usize, f32)> {
    let (index, &selected) = logits
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .ok_or_else(|| FukidashiError::TensorContract("Baberu logits are empty".into()))?;
    if !selected.is_finite() {
        return Err(FukidashiError::TensorContract(
            "Baberu logits contain no finite candidate".into(),
        ));
    }
    let max = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let denominator = logits.iter().map(|score| (score - max).exp()).sum::<f64>();
    if !denominator.is_finite() || denominator <= 0.0 {
        return Err(FukidashiError::TensorContract(
            "Baberu logits have an invalid softmax denominator".into(),
        ));
    }
    Ok((
        index,
        ((selected - max).exp() / denominator).clamp(0.0, 1.0) as f32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heavy_call_recycling_obeys_boundary_and_resets_counter() {
        let mut engine = OcrEngine::default();
        assert!(!engine.finish_heavy_call(2));
        assert!(engine.finish_heavy_call(2));
        assert!(!engine.finish_heavy_call(2));
        engine.release_sessions();
        assert!(!engine.finish_heavy_call(2));
    }
    use std::collections::HashMap;
    #[test]
    fn target_precedence() {
        assert_eq!(resolve_target_language(Some("VI"), "en").unwrap(), "vi");
        assert_eq!(resolve_target_language(None, "fr").unwrap(), "fr");
        assert!(resolve_target_language(Some("bad tag"), "vi").is_err());
        assert_eq!(resolve_target_language(None, "vi").unwrap(), "vi");
    }
    #[test]
    fn routing_ambiguity() {
        assert_eq!(route_script("HanS"), RecognizerKind::Han);
        assert_eq!(route_script("Hangul_vert"), RecognizerKind::Korean);
        assert_eq!(route_script("Japanese_vert"), RecognizerKind::Manga);
        assert_eq!(route_script("Latin-dn"), RecognizerKind::Latin);
        assert_eq!(route_script("Common"), RecognizerKind::Latin);
        assert_eq!(
            inferred_language("Common", RecognizerKind::Han, "漢字"),
            "ja|zh"
        );
        assert_eq!(normalize_language("auto"), "auto");
    }
    #[test]
    fn explicit_english_and_latin_are_distinct_routes() {
        assert_eq!(route_override("en").unwrap(), RecognizerKind::English);
        assert_eq!(route_override("latin").unwrap(), RecognizerKind::Latin);
        assert_eq!(route_script("Latin"), RecognizerKind::Latin);
        assert_eq!(
            inferred_language("Latin", RecognizerKind::English, "Hello"),
            "en"
        );
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn auto_english_evidence_prefers_common_words() {
        assert!(english_evidence("Hello world") > english_evidence("Xqv zlm"));
        assert_eq!(script_match(RecognizerKind::English, "Hello"), 2);
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn baberu_tensor_uses_rgb_imagenet_normalization() {
        let image = RgbImage::from_pixel(1, 1, image::Rgb([255, 0, 127]));
        let tensor = baberu_tensor(&image).unwrap();
        assert_eq!(tensor.len(), 3 * 224 * 224);
        assert!((tensor[0] - (1.0 - 0.485) / 0.229).abs() < 1e-5);
        assert!((tensor[224 * 224] - (0.0 - 0.456) / 0.224).abs() < 1e-5);
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn baberu_decode_confidence_is_finite_and_cache_bounds_are_checked() {
        let mut logits = vec![f64::NEG_INFINITY; 14_630];
        logits[42] = 3.0;
        let (token, confidence) = baberu_next_token(&logits).unwrap();
        assert_eq!(token, 42);
        assert!(confidence.is_finite() && confidence > 0.99);
        assert!(baberu_cache_length_valid(2 * 257 * 64, 257));
        assert!(!baberu_cache_length_valid(2 * 256 * 64, 257));
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn baberu_route_is_gated_by_the_complete_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            storage_root: temp.path().to_path_buf(),
            models_dir: temp.path().to_path_buf(),
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
        assert_eq!(
            preferred_explicit_route(&config, RecognizerKind::English),
            RecognizerKind::English
        );
        for name in [
            "vision_int4.onnx",
            "decoder_prefill_int8.onnx",
            "decoder_step_int8.onnx",
            "vocab.json",
            "tokenizer_config.json",
        ] {
            std::fs::create_dir_all(config.baberu_dir()).unwrap();
            std::fs::write(config.baberu_dir().join(name), b"asset").unwrap();
        }
        assert!(Pipeline::baberu_assets_available(&config));
        assert_eq!(
            preferred_explicit_route(&config, RecognizerKind::English),
            RecognizerKind::Baberu
        );
    }
    #[test]
    fn corrected_source_precedes_ocr_and_preserves_audit() {
        assert_eq!(
            effective_source_text("gibberish", Some("corrected text")),
            ("corrected text".into(), true)
        );
        assert_eq!(
            effective_source_text("ocr text", None),
            ("ocr text".into(), false)
        );
    }
    #[test]
    fn correction_contract_updates_handoff_by_stable_id() {
        let region = OcrRegion {
            id: "line-1".into(),
            bbox: Rect {
                x1: 0.0,
                y1: 0.0,
                x2: 4.0,
                y2: 4.0,
            },
            text: "raw OCR".into(),
            source_language: "en".into(),
            script: "Latin".into(),
            recognizer: RecognizerKind::English.as_str().into(),
            confidence: 0.73,
            uncertainty: 0.27,
            detector_label: 1,
            detector_confidence: 0.9,
            reading_order: 0,
            vision_correction: VisionCorrection {
                image_path: "C:/page.png".into(),
                bbox: Rect {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 4.0,
                    y2: 4.0,
                },
                contract: "source-image-bbox",
            },
        };
        let mut analysis = PageAnalysis {
            source_language: "en".into(),
            target_language: "vi".into(),
            bubbles: vec![region.clone()],
            text_lines: vec![region.clone()],
            unmatched_text: Vec::new(),
            translation_handoff: TranslationHandoff {
                target_language: "vi".into(),
                status: "pending",
                items: vec![TranslationItem::from_region(&region)],
            },
        };
        let corrections = HashMap::from([(String::from("line-1"), String::from("fixed text"))]);
        apply_source_corrections(&mut analysis, &corrections);
        let item = &analysis.translation_handoff.items[0];
        assert_eq!(item.source_text, "fixed text");
        assert_eq!(item.ocr_text, "raw OCR");
        assert_eq!(item.confidence, 0.73);
        assert_eq!(item.corrected_source_text.as_deref(), Some("fixed text"));
        assert!(item.correction_applied);
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn missing_manga_assets_fall_back_to_shared_han() {
        let config = Config {
            storage_root: tempfile::tempdir().unwrap().path().to_path_buf(),
            models_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ort_dylib: None,
            config_file: tempfile::tempdir().unwrap().path().join("config.json"),
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
        assert!(!Pipeline::manga_assets_available(&config));
        assert_eq!(
            preferred_explicit_route(&config, RecognizerKind::Manga),
            RecognizerKind::Han
        );
    }
    #[test]
    fn translation_item_exposes_vision_handoff_contract() {
        let region = OcrRegion {
            id: "line-1".into(),
            bbox: Rect {
                x1: 1.0,
                y1: 2.0,
                x2: 10.0,
                y2: 12.0,
            },
            text: "OCR".into(),
            source_language: "en".into(),
            script: "Latin".into(),
            recognizer: RecognizerKind::English.as_str().into(),
            confidence: 0.8,
            uncertainty: 0.2,
            detector_label: 1,
            detector_confidence: 0.9,
            reading_order: 0,
            vision_correction: VisionCorrection {
                image_path: "C:/page.png".into(),
                bbox: Rect {
                    x1: 1.0,
                    y1: 2.0,
                    x2: 10.0,
                    y2: 12.0,
                },
                contract: "source-image-bbox",
            },
        };
        let item = TranslationItem::from_region_with_correction(&region, Some("fixed"));
        assert_eq!(item.ocr_text, "OCR");
        assert_eq!(item.source_text, "fixed");
        assert!(item.correction_applied);
        assert_eq!(item.vision_correction.contract, "source-image-bbox");
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn mixed_reading_sort_is_total_and_ltr() {
        let mut regions = vec![
            OcrRegion {
                id: "han".into(),
                bbox: Rect {
                    x1: 20.0,
                    y1: 1.0,
                    x2: 30.0,
                    y2: 10.0,
                },
                text: "漢".into(),
                source_language: "ja|zh".into(),
                script: "HanS".into(),
                recognizer: RecognizerKind::Han.as_str().into(),
                confidence: 1.0,
                uncertainty: 0.0,
                detector_label: 0,
                detector_confidence: 1.0,
                reading_order: 0,
                vision_correction: VisionCorrection {
                    image_path: "test.png".into(),
                    bbox: Rect {
                        x1: 20.0,
                        y1: 1.0,
                        x2: 30.0,
                        y2: 10.0,
                    },
                    contract: "source-image-bbox",
                },
            },
            OcrRegion {
                id: "latin".into(),
                bbox: Rect {
                    x1: 10.0,
                    y1: 1.0,
                    x2: 15.0,
                    y2: 10.0,
                },
                text: "A".into(),
                source_language: "en|latin".into(),
                script: "Latin".into(),
                recognizer: RecognizerKind::Latin.as_str().into(),
                confidence: 1.0,
                uncertainty: 0.0,
                detector_label: 0,
                detector_confidence: 1.0,
                reading_order: 0,
                vision_correction: VisionCorrection {
                    image_path: "test.png".into(),
                    bbox: Rect {
                        x1: 10.0,
                        y1: 1.0,
                        x2: 15.0,
                        y2: 10.0,
                    },
                    contract: "source-image-bbox",
                },
            },
        ];
        sort_regions(&mut regions);
        assert_eq!(
            regions.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["latin", "han"]
        );
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn detector_dedup_and_line_association_are_deterministic() {
        let bubble = Detection {
            label: 0,
            bbox: Rect {
                x1: 10.0,
                y1: 10.0,
                x2: 100.0,
                y2: 100.0,
            },
            score: 0.9,
        };
        let duplicate = Detection {
            label: 0,
            bbox: Rect {
                x1: 12.0,
                y1: 12.0,
                x2: 98.0,
                y2: 98.0,
            },
            score: 0.7,
        };
        let other = Detection {
            label: 0,
            bbox: Rect {
                x1: 120.0,
                y1: 10.0,
                x2: 200.0,
                y2: 100.0,
            },
            score: 0.8,
        };
        let line = Detection {
            label: 1,
            bbox: Rect {
                x1: 20.0,
                y1: 30.0,
                x2: 70.0,
                y2: 50.0,
            },
            score: 0.8,
        };
        let caption = Detection {
            label: 2,
            bbox: Rect {
                x1: 300.0,
                y1: 300.0,
                x2: 350.0,
                y2: 320.0,
            },
            score: 0.8,
        };
        let bubbles = dedup_detections(vec![duplicate, other, bubble]).unwrap();
        assert_eq!(bubbles.len(), 2);
        let association = associate_text_lines(&bubbles, &[line, caption]);
        assert_eq!(association.iter().map(Vec::len).sum::<usize>(), 1);
        assert_eq!(association[0], vec![0]);
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn ctc_blank_repeat() {
        let chars = vec!["A".into(), "B".into()];
        let mut v = vec![0.0; 20];
        for (i, c) in [(0, 1), (1, 1), (2, 0), (3, 2), (4, 0)] {
            v[i * 4 + c] = 1.0
        }
        assert_eq!(decode_ctc(&v, &[1, 5, 4], &chars).unwrap().0, "AB");
        v[4 * 4 + 3] = 1.0;
        assert_eq!(decode_ctc(&v, &[1, 5, 4], &chars).unwrap().0, "AB ")
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn manga_decoder_position_matches_upstream_cache_contract() {
        assert_eq!(manga_position_id(1), 2);
        assert_eq!(manga_position_id(126), 127);
        assert_eq!(manga_position_id(127), 127);
        assert_eq!(manga_position_id(255), 127);
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn manga_tensor_uses_white_rgb_letterbox() {
        let image = RgbImage::from_pixel(100, 50, image::Rgb([0, 128, 255]));
        let tensor = manga_tensor(&image).unwrap();
        assert_eq!(tensor.len(), 3 * 224 * 224);
        assert_eq!(tensor[0], 1.0);
        assert_eq!(tensor[(56 * 224) + 112], 0.0);
        assert!((tensor[224 * 224 + (56 * 224) + 112] - 128.0 / 255.0).abs() < 1e-6);
    }
    #[cfg(feature = "onnx")]
    #[test]
    fn provider_fallback_only_matches_provider_failures() {
        assert!(provider_failure(
            "CUDA execution provider failed to initialize"
        ));
        assert!(provider_failure("GPU device is out of memory"));
        let arena_oom = "BFCArena::AllocateRawInternal available memory 545118976 is smaller than requested 594027520";
        assert!(should_retry_inpaint_on_cpu(
            SessionPolicy::Cuda,
            false,
            arena_oom
        ));
        assert!(!should_retry_inpaint_on_cpu(
            SessionPolicy::Cpu,
            false,
            arena_oom
        ));
        assert_eq!(
            provider_failure_summary(arena_oom),
            "CUDA memory allocation failed"
        );
        assert!(!should_retry_inpaint_on_cpu(
            SessionPolicy::Cuda,
            true,
            arena_oom
        ));
        assert!(!provider_failure("invalid ONNX graph input shape"));
    }
}
