//! Pipeline processing module
//!
//! Provides a clean API for PDF processing pipeline, separating
//! business logic from CLI handling.
//!
//! ## Processing Steps
//!
//! 1. PDF読み込み・メタデータ抽出
//! 2. 画像抽出 (pdftoppm/ImageMagick)
//! 3. 傾き補正 (Deskew)
//! 4. マージントリミング
//! 5. AI超解像 (RealESRGAN)
//! 6. 内部解像度正規化
//! 7. 色統計分析・グローバル色補正
//! 8. Tukey fenceグループクロップ
//! 9. ページ番号オフセット計算
//! 10. 最終出力リサイズ
//! 11. 縦書き検出
//! 12. YomiToku OCR
//! 13. PDF生成

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;

use crate::cli::{ConvertArgs, GeometryAction};
use crate::deskew::{
    DeskewAlgorithm, DeskewMaxAngle, DeskewMinConfidence, DeskewMinFeatures, DeskewNoopAngle,
    DeskewOptions, DeskewPolicyOptions, DeskewReason, QualityMode, RotationAnalysisOptions,
    RotationConfidenceThreshold, RotationEvidence, RotationReason, SkewDetection,
    DEFAULT_MAX_ANGLE,
};
use crate::transform_manifest::{DeskewTransform, RotationTransform, TransformDecision};

// ============================================================
// Memory Management Utilities (Phase 3 optimization)
// ============================================================

/// Estimated memory per image at 300 DPI (A4 size, RGBA)
const ESTIMATED_IMAGE_MEMORY_MB: usize = 100;

/// Minimum chunk size for parallel processing
const MIN_CHUNK_SIZE: usize = 4;

/// Default memory limit if not specified (4GB)
const DEFAULT_MEMORY_LIMIT_MB: usize = 4096;

/// Calculate optimal chunk size based on memory constraints
///
/// # Arguments
/// * `total_items` - Total number of items to process
/// * `max_memory_mb` - Maximum memory to use (0 = use default limit)
/// * `threads` - Number of parallel threads
///
/// # Returns
/// Optimal chunk size that fits within memory constraints
pub fn calculate_optimal_chunk_size(
    total_items: usize,
    max_memory_mb: usize,
    threads: usize,
) -> usize {
    let memory_limit = if max_memory_mb == 0 {
        get_available_memory_mb().unwrap_or(DEFAULT_MEMORY_LIMIT_MB)
    } else {
        max_memory_mb
    };

    // Reserve 50% of available memory for OS and other processes
    let usable_memory = memory_limit / 2;

    // Calculate how many images can be processed concurrently
    // Consider thread count to avoid over-committing memory
    let max_concurrent = threads.max(num_cpus::get());
    let per_thread_capacity = usable_memory / ESTIMATED_IMAGE_MEMORY_MB;
    let concurrent_capacity = per_thread_capacity.min(max_concurrent);

    // Chunk size should be at least MIN_CHUNK_SIZE but not more than concurrent capacity
    let chunk_size = concurrent_capacity.max(MIN_CHUNK_SIZE);

    // Don't exceed total items
    chunk_size.min(total_items).max(1)
}

/// Get available system memory in MB
#[cfg(target_os = "linux")]
fn get_available_memory_mb() -> Option<usize> {
    use std::fs;

    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if line.starts_with("MemAvailable:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let kb: usize = parts[1].parse().ok()?;
                return Some(kb / 1024);
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn get_available_memory_mb() -> Option<usize> {
    // For non-Linux systems, use a conservative default
    Some(DEFAULT_MEMORY_LIMIT_MB)
}

/// Process items in chunks for memory-controlled parallel execution
///
/// This function processes items in batches to prevent memory exhaustion.
/// Each chunk is processed in parallel using rayon, but chunks are processed
/// sequentially to limit peak memory usage.
///
/// # Arguments
/// * `items` - Items to process
/// * `chunk_size` - Size of each processing chunk (0 = process all at once)
/// * `processor` - Function to apply to each item
/// * `progress` - Optional progress callback (current, total)
///
/// # Returns
/// Vector of results in the same order as input items
pub fn process_in_chunks<T, R, F, P>(
    items: &[T],
    chunk_size: usize,
    processor: F,
    progress: Option<&P>,
) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
    P: Fn(usize, usize) + Sync,
{
    let total = items.len();
    if total == 0 {
        return vec![];
    }

    let effective_chunk_size = if chunk_size == 0 { total } else { chunk_size };
    let completed = AtomicUsize::new(0);

    // Collect all results with their indices, then sort
    let mut indexed_results: Vec<(usize, R)> = Vec::with_capacity(total);

    for chunk_start in (0..total).step_by(effective_chunk_size) {
        let chunk_end = (chunk_start + effective_chunk_size).min(total);
        let chunk: Vec<(usize, &T)> = (chunk_start..chunk_end).map(|i| (i, &items[i])).collect();

        let chunk_results: Vec<(usize, R)> = chunk
            .par_iter()
            .map(|(idx, item)| {
                let result = processor(item);
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(cb) = progress {
                    cb(done, total);
                }
                (*idx, result)
            })
            .collect();

        indexed_results.extend(chunk_results);
    }

    // Sort by index to maintain original order
    indexed_results.sort_by_key(|(idx, _)| *idx);
    indexed_results.into_iter().map(|(_, r)| r).collect()
}

/// Progress callback for pipeline steps
pub trait ProgressCallback: Send + Sync {
    /// Called when a new step starts
    fn on_step_start(&self, step: &str);
    /// Called to report progress within a step
    fn on_step_progress(&self, current: usize, total: usize);
    /// Called when a step completes
    fn on_step_complete(&self, step: &str, message: &str);
    /// Called for debug/verbose messages
    fn on_debug(&self, message: &str);
    /// Called for warning messages (more visible than debug)
    fn on_warning(&self, message: &str) {
        // Default: print to stderr
        eprintln!("Warning: {}", message);
    }
}

/// No-op progress callback (silent mode)
pub struct SilentProgress;

impl ProgressCallback for SilentProgress {
    fn on_step_start(&self, _step: &str) {}
    fn on_step_progress(&self, _current: usize, _total: usize) {}
    fn on_step_complete(&self, _step: &str, _message: &str) {}
    fn on_debug(&self, _message: &str) {}
    fn on_warning(&self, _message: &str) {}
}

/// Pipeline processing error
#[derive(Error, Debug)]
pub enum PipelineError {
    #[error("Input file not found: {0}")]
    InputNotFound(PathBuf),

    #[error("Output directory not writable: {0}")]
    OutputNotWritable(PathBuf),

    #[error("PDF extraction failed: {0}")]
    ExtractionFailed(String),

    #[error("native PDF extraction failed: {0}")]
    NativeExtraction(#[from] crate::image_extract::NativeExtractError),

    #[error("Image processing failed: {0}")]
    ImageProcessingFailed(String),

    #[error("PDF generation failed: {0}")]
    PdfGenerationFailed(String),

    #[error(
        "OCR was requested (--ocr) but YomiToku is unavailable: {0}. \
         Set the SUPERBOOK_VENV environment variable to your venv path"
    )]
    OcrUnavailable(String),

    #[error("Unsupported mode: {0}")]
    UnsupportedMode(String),

    #[error("Transform manifest failed: {0}")]
    TransformManifest(#[from] crate::transform_manifest::TransformManifestError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Pipeline configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Use the preservation geometry-only pipeline
    #[serde(default)]
    pub geometry_only: bool,
    /// Action for 180-degree rotation analysis/application
    #[serde(default = "default_geometry_action_apply")]
    pub rotation_action: GeometryAction,
    /// Minimum confidence required to apply a proposed 180-degree rotation
    #[serde(default)]
    pub rotation_min_confidence: RotationConfidenceThreshold,
    /// Whether the rotation action was explicitly resolved or configured
    #[doc(hidden)]
    #[serde(default, skip_serializing_if = "is_false")]
    pub rotation_action_configured: bool,
    /// Action for deskew analysis/application
    #[serde(default = "default_geometry_action_apply")]
    pub deskew_action: GeometryAction,
    /// Maximum absolute angle eligible for automatic deskew
    #[serde(default)]
    pub deskew_max_angle: DeskewMaxAngle,
    /// Minimum confidence required for automatic deskew
    #[serde(default)]
    pub deskew_min_confidence: DeskewMinConfidence,
    /// Minimum detector feature count required for automatic deskew
    #[serde(default)]
    pub deskew_min_features: DeskewMinFeatures,
    /// Absolute angle at or below which deskew is a no-op
    #[serde(default)]
    pub deskew_noop_angle: DeskewNoopAngle,
    /// Whether the deskew action was explicitly resolved or configured
    #[doc(hidden)]
    #[serde(default, skip_serializing_if = "is_false")]
    pub deskew_action_configured: bool,
    /// Output DPI
    pub dpi: u32,
    /// Enable deskew
    pub deskew: bool,
    /// Margin trim percentage
    pub margin_trim: f64,
    /// Enable AI upscaling
    pub upscale: bool,
    /// Enable GPU
    pub gpu: bool,
    /// Enable internal resolution normalization
    pub internal_resolution: bool,
    /// Enable color correction
    pub color_correction: bool,
    /// Enable offset alignment
    pub offset_alignment: bool,
    /// Output height
    pub output_height: u32,
    /// Enable OCR
    pub ocr: bool,
    /// Max pages for debug
    pub max_pages: Option<usize>,
    /// Save debug images
    pub save_debug: bool,
    /// JPEG quality (0-100)
    pub jpeg_quality: u8,
    /// Thread count (None = auto)
    pub threads: Option<usize>,
    /// Maximum memory usage in MB (0 = unlimited)
    #[serde(default)]
    pub max_memory_mb: usize,
    /// Chunk size for batch processing (0 = auto based on memory)
    #[serde(default)]
    pub chunk_size: usize,
    /// Shadow removal mode: "none", "auto", "left", "right", "both"
    #[serde(default = "default_shadow_removal")]
    pub shadow_removal: String,
    /// Enable marker/highlighter removal
    #[serde(default)]
    pub remove_markers: bool,
    /// Marker colors to remove
    #[serde(default = "default_marker_colors")]
    pub marker_colors: Vec<String>,
    /// Enable deblur processing
    #[serde(default)]
    pub deblur: bool,
    /// Deblur algorithm: "unsharp_mask", "nafnet", "deblurgan_v2"
    #[serde(default = "default_deblur_algorithm")]
    pub deblur_algorithm: String,
    /// Assume Japanese book (enables R2L page progression even for horizontal text)
    #[serde(default = "default_true")]
    pub assume_japanese_book: bool,
}

fn default_true() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_geometry_action_apply() -> GeometryAction {
    GeometryAction::Apply
}

fn default_shadow_removal() -> String {
    "none".to_string()
}

fn default_marker_colors() -> Vec<String> {
    vec![
        "yellow".to_string(),
        "pink".to_string(),
        "green".to_string(),
        "blue".to_string(),
    ]
}

fn default_deblur_algorithm() -> String {
    "unsharp_mask".to_string()
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            geometry_only: false,
            rotation_action: GeometryAction::Apply,
            rotation_min_confidence: RotationConfidenceThreshold::default(),
            rotation_action_configured: false,
            deskew_action: GeometryAction::Apply,
            deskew_max_angle: DeskewMaxAngle::default(),
            deskew_min_confidence: DeskewMinConfidence::default(),
            deskew_min_features: DeskewMinFeatures::default(),
            deskew_noop_angle: DeskewNoopAngle::default(),
            deskew_action_configured: false,
            dpi: 300,
            deskew: true,
            margin_trim: 0.7,
            upscale: true,
            gpu: true,
            internal_resolution: false,
            color_correction: true,
            offset_alignment: false,
            output_height: 3508,
            ocr: false,
            max_pages: None,
            save_debug: false,
            jpeg_quality: 90,
            threads: None,
            max_memory_mb: 0, // 0 = unlimited
            chunk_size: 0,    // 0 = auto
            shadow_removal: default_shadow_removal(),
            remove_markers: false,
            marker_colors: default_marker_colors(),
            deblur: false,
            deblur_algorithm: default_deblur_algorithm(),
            assume_japanese_book: true,
        }
    }
}

impl PipelineConfig {
    /// Return the cross-field validated conservative deskew policy.
    pub fn deskew_policy(&self) -> Result<DeskewPolicyOptions, PipelineError> {
        DeskewPolicyOptions::new(
            self.deskew_max_angle,
            self.deskew_min_confidence,
            self.deskew_min_features,
            self.deskew_noop_angle,
        )
        .map_err(|error| PipelineError::ImageProcessingFailed(error.to_string()))
    }

    /// Create a safe geometry-only configuration with explicit actions.
    pub fn geometry_only(rotation_action: GeometryAction, deskew_action: GeometryAction) -> Self {
        let mut config = Self {
            geometry_only: true,
            rotation_action,
            rotation_action_configured: true,
            deskew_action,
            deskew_action_configured: true,
            ..Self::default()
        };
        config.enforce_geometry_only_constraints();
        config
    }

    /// Create configuration from CLI convert arguments
    pub fn from_convert_args(args: &ConvertArgs) -> Self {
        let advanced = args.advanced;
        let mut config = Self {
            geometry_only: args.geometry_only,
            rotation_action: args.effective_rotation_action(),
            rotation_min_confidence: args.rotation_min_confidence.unwrap_or_default(),
            rotation_action_configured: true,
            deskew_action: args.effective_deskew_action(),
            deskew_max_angle: args.deskew_max_angle.unwrap_or_default(),
            deskew_min_confidence: args.deskew_min_confidence.unwrap_or_default(),
            deskew_min_features: args.deskew_min_features.unwrap_or_default(),
            deskew_noop_angle: args.deskew_noop_angle.unwrap_or_default(),
            deskew_action_configured: true,
            dpi: args.dpi,
            deskew: args.effective_deskew(),
            margin_trim: args.margin_trim as f64,
            upscale: args.effective_upscale(),
            gpu: args.effective_gpu(),
            internal_resolution: args.internal_resolution || advanced,
            color_correction: args.color_correction || advanced,
            offset_alignment: args.offset_alignment || advanced,
            output_height: args.output_height,
            ocr: args.ocr,
            max_pages: args.max_pages,
            save_debug: args.save_debug,
            jpeg_quality: args.jpeg_quality,
            threads: args.threads,
            max_memory_mb: 0,
            chunk_size: 0,
            shadow_removal: format!("{:?}", args.shadow_removal).to_lowercase(),
            remove_markers: args.remove_markers,
            marker_colors: args.marker_colors.clone(),
            deblur: args.deblur,
            deblur_algorithm: format!("{:?}", args.deblur_algorithm).to_lowercase(),
            assume_japanese_book: true,
        };
        config.enforce_geometry_only_constraints();
        config
    }

    /// Convert to JSON string for cache digest
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Builder pattern: set DPI
    pub fn with_dpi(mut self, dpi: u32) -> Self {
        self.dpi = dpi;
        self
    }

    /// Builder pattern: set deskew
    pub fn with_deskew(mut self, enabled: bool) -> Self {
        self.deskew = enabled;
        self.deskew_action = if enabled {
            GeometryAction::Apply
        } else {
            GeometryAction::Off
        };
        self.deskew_action_configured = true;
        self
    }

    /// Builder pattern: set margin trim
    pub fn with_margin_trim(mut self, percent: f64) -> Self {
        self.margin_trim = percent;
        self
    }

    /// Builder pattern: set upscale
    pub fn with_upscale(mut self, enabled: bool) -> Self {
        self.upscale = enabled;
        self
    }

    /// Builder pattern: set GPU
    pub fn with_gpu(mut self, enabled: bool) -> Self {
        self.gpu = enabled;
        self
    }

    /// Builder pattern: set OCR
    pub fn with_ocr(mut self, enabled: bool) -> Self {
        self.ocr = enabled;
        self
    }

    /// Builder pattern: set max pages
    pub fn with_max_pages(mut self, max: Option<usize>) -> Self {
        self.max_pages = max;
        self
    }

    /// Enable all advanced features
    pub fn with_advanced(mut self) -> Self {
        self.internal_resolution = true;
        self.color_correction = true;
        self.offset_alignment = true;
        self
    }

    /// Force the safe effective configuration required by geometry-only mode.
    pub fn enforce_geometry_only_constraints(&mut self) {
        if !self.geometry_only {
            return;
        }

        self.deskew = self.deskew_action != GeometryAction::Off;
        self.margin_trim = 0.0;
        self.upscale = false;
        self.gpu = false;
        self.internal_resolution = false;
        self.color_correction = false;
        self.offset_alignment = false;
        self.output_height = 0;
        self.ocr = false;
        self.shadow_removal = "none".to_string();
        self.remove_markers = false;
        self.deblur = false;
    }

    /// Resolve preservation-safe action defaults before pipeline execution.
    fn resolve_geometry_action_defaults(&mut self) {
        if !self.geometry_only {
            return;
        }
        if !self.rotation_action_configured {
            self.rotation_action = GeometryAction::Report;
        }
        if !self.deskew_action_configured {
            self.deskew_action = GeometryAction::Report;
        }
    }

    fn rotation_stage_enabled(&self) -> bool {
        self.rotation_action != GeometryAction::Off && (self.geometry_only || self.deskew)
    }

    fn deskew_stage_enabled(&self) -> bool {
        self.deskew_action != GeometryAction::Off && (self.geometry_only || self.deskew)
    }
}

/// Result of pipeline processing
#[derive(Debug, Clone)]
pub struct PipelineResult {
    /// Number of pages processed
    pub page_count: usize,
    /// Detected page number shift
    pub page_number_shift: Option<i32>,
    /// Whether vertical text was detected
    pub is_vertical: bool,
    /// Processing time in seconds
    pub elapsed_seconds: f64,
    /// Output file path
    pub output_path: PathBuf,
    /// Output file size in bytes
    pub output_size: u64,
    /// Transform manifest JSONL path, present only when the manifest was published.
    pub transform_manifest_path: Option<PathBuf>,
}

/// Auditable policy result derived from one rotation analysis.
#[derive(Debug, Clone, PartialEq)]
pub struct RotationPolicyOutcome {
    /// Zero-based physical page index supplied by the coordinator.
    pub page_index: usize,
    /// Manifest-ready transform evidence and decision.
    pub transform: RotationTransform,
    /// Whether decoded pixels should be permuted by 180 degrees.
    pub should_apply: bool,
    /// Whether a human should review this page.
    pub review_required: bool,
}

impl RotationPolicyOutcome {
    fn from_evidence(
        page_index: usize,
        evidence: &RotationEvidence,
        action: GeometryAction,
        threshold: RotationConfidenceThreshold,
    ) -> Result<Self, PipelineError> {
        let is_upright = evidence.proposed_degrees() == 0
            && evidence.reason() == RotationReason::UprightEvidence
            && !evidence.ambiguity_guard();
        let approved = evidence.is_auto_applicable(threshold.get());

        let (decision, should_apply, review_required) = match action {
            GeometryAction::Off => (TransformDecision::Unchanged, false, false),
            GeometryAction::Report if approved => (TransformDecision::Proposed, false, true),
            GeometryAction::Apply if approved => (TransformDecision::Proposed, true, false),
            GeometryAction::Report | GeometryAction::Apply if is_upright => {
                (TransformDecision::Unchanged, false, false)
            }
            GeometryAction::Report | GeometryAction::Apply => {
                (TransformDecision::Rejected, false, true)
            }
        };

        let transform = RotationTransform::new(
            evidence.proposed_degrees() as i16,
            Some(evidence.score()),
            evidence.confidence(),
            decision,
            evidence.reason().to_string(),
        )?;

        Ok(Self {
            page_index,
            transform,
            should_apply,
            review_required,
        })
    }

    fn mark_applied(&mut self) {
        debug_assert!(self.should_apply);
        self.transform.decision = TransformDecision::Applied;
    }
}

/// Auditable policy result derived from one deskew analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeskewInterpolation {
    Lanczos3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeskewCanvas {
    Expanded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeskewPixelMode {
    Rgba8,
}

/// Exact decoded-raster behavior used by the current deskew implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeskewApplicationMetadata {
    pub interpolation: DeskewInterpolation,
    pub canvas: DeskewCanvas,
    pub fill_rgba: [u8; 4],
    pub output_pixel_mode: DeskewPixelMode,
}

impl DeskewApplicationMetadata {
    pub const fn current() -> Self {
        Self {
            interpolation: DeskewInterpolation::Lanczos3,
            canvas: DeskewCanvas::Expanded,
            fill_rgba: [255, 255, 255, 255],
            output_pixel_mode: DeskewPixelMode::Rgba8,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeskewPolicyOutcome {
    pub page_index: usize,
    pub detection: Option<SkewDetection>,
    pub transform: DeskewTransform,
    pub application: Option<DeskewApplicationMetadata>,
    pub should_apply: bool,
    pub review_required: bool,
}

impl DeskewPolicyOutcome {
    fn without_detection(page_index: usize, reason: DeskewReason) -> Result<Self, PipelineError> {
        Ok(Self {
            page_index,
            detection: None,
            transform: DeskewTransform::new(
                0.0,
                0.0,
                0,
                TransformDecision::Unchanged,
                reason.to_string(),
            )?,
            application: None,
            should_apply: false,
            review_required: false,
        })
    }

    fn from_detection(
        page_index: usize,
        detection: &SkewDetection,
        action: GeometryAction,
        policy: DeskewPolicyOptions,
    ) -> Result<Self, PipelineError> {
        detection
            .validate()
            .map_err(|error| PipelineError::ImageProcessingFailed(error.to_string()))?;
        let magnitude = detection.angle.abs();
        let (decision, reason, should_apply, review_required) =
            if magnitude > policy.maximum_angle().get() {
                (
                    TransformDecision::Rejected,
                    DeskewReason::AngleOutsideLimit,
                    false,
                    true,
                )
            } else if detection.confidence < policy.minimum_confidence().get() {
                (
                    TransformDecision::Rejected,
                    DeskewReason::InsufficientConfidence,
                    false,
                    true,
                )
            } else if detection.feature_count < policy.minimum_features().get() {
                (
                    TransformDecision::Rejected,
                    DeskewReason::InsufficientFeatures,
                    false,
                    true,
                )
            } else if magnitude <= policy.noop_angle().get() {
                (
                    TransformDecision::Unchanged,
                    DeskewReason::WithinNoopThreshold,
                    false,
                    false,
                )
            } else {
                match action {
                    GeometryAction::Report => (
                        TransformDecision::Proposed,
                        DeskewReason::CorrectionEvidence,
                        false,
                        true,
                    ),
                    GeometryAction::Apply => (
                        TransformDecision::Proposed,
                        DeskewReason::CorrectionEvidence,
                        true,
                        false,
                    ),
                    GeometryAction::Off => (
                        TransformDecision::Unchanged,
                        DeskewReason::Disabled,
                        false,
                        false,
                    ),
                }
            };

        Ok(Self {
            page_index,
            detection: Some(detection.clone()),
            transform: DeskewTransform::new(
                detection.angle,
                detection.confidence,
                detection.feature_count,
                decision,
                reason.to_string(),
            )?,
            application: None,
            should_apply,
            review_required,
        })
    }

    fn mark_applied(&mut self) {
        debug_assert!(self.should_apply);
        self.transform.decision = TransformDecision::Applied;
        self.application = Some(DeskewApplicationMetadata::current());
    }
}

impl PipelineResult {
    /// Create a new pipeline result
    pub fn new(
        page_count: usize,
        page_number_shift: Option<i32>,
        is_vertical: bool,
        elapsed_seconds: f64,
        output_path: PathBuf,
        output_size: u64,
        transform_manifest_path: Option<PathBuf>,
    ) -> Self {
        Self {
            page_count,
            page_number_shift,
            is_vertical,
            elapsed_seconds,
            output_path,
            output_size,
            transform_manifest_path,
        }
    }

    /// Convert to cache ProcessingResult
    pub fn to_cache_result(&self) -> crate::cache::ProcessingResult {
        crate::cache::ProcessingResult::new(
            self.page_count,
            self.page_number_shift,
            self.is_vertical,
            self.elapsed_seconds,
            self.output_size,
        )
    }
}

/// Processing context for intermediate data
#[derive(Debug)]
pub struct ProcessingContext {
    /// Working directory for this PDF
    pub work_dir: PathBuf,
    /// PDF document info
    pub pdf_info: crate::PdfDocument,
    /// Extracted page images
    pub current_images: Vec<PathBuf>,
    /// Detected vertical writing
    pub is_vertical: bool,
    /// Page number shift
    pub page_number_shift: Option<i32>,
}

/// PDF processing pipeline
pub struct PdfPipeline {
    config: PipelineConfig,
}

impl PdfPipeline {
    /// Create a new pipeline with the given configuration
    pub fn new(mut config: PipelineConfig) -> Self {
        config.resolve_geometry_action_defaults();
        config.enforce_geometry_only_constraints();
        Self { config }
    }

    /// Get the pipeline configuration
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// Inspect every physical source page without rendering or creating a workspace.
    pub fn inspect_native_source(
        &self,
        input: &Path,
    ) -> Result<crate::image_extract::NativePdfDocument, PipelineError> {
        crate::image_extract::NativePdfExtractor::extract_path(input).map_err(Into::into)
    }

    /// Analyze one decoded page and apply the configured report/apply policy.
    ///
    /// `None` means rotation analysis is disabled. The returned outcome is
    /// manifest-ready and does not itself modify the image.
    pub fn analyze_rotation(
        &self,
        page_index: usize,
        image_path: &Path,
    ) -> Result<Option<RotationPolicyOutcome>, PipelineError> {
        if self.config.rotation_action == GeometryAction::Off {
            return Ok(None);
        }

        let options = RotationAnalysisOptions::builder()
            .minimum_apply_confidence(self.config.rotation_min_confidence.get())
            .build()
            .map_err(|error| PipelineError::ImageProcessingFailed(error.to_string()))?;
        let evidence = crate::ImageProcDeskewer::analyze_rotation(image_path, &options)
            .map_err(|error| PipelineError::ImageProcessingFailed(error.to_string()))?;
        RotationPolicyOutcome::from_evidence(
            page_index,
            &evidence,
            self.config.rotation_action,
            self.config.rotation_min_confidence,
        )
        .map(Some)
    }

    /// Analyze one page exactly once and map its evidence through deskew policy.
    pub fn analyze_deskew(
        &self,
        page_index: usize,
        image_path: &Path,
        blank: bool,
    ) -> Result<DeskewPolicyOutcome, PipelineError> {
        if self.config.deskew_action == GeometryAction::Off {
            return DeskewPolicyOutcome::without_detection(page_index, DeskewReason::Disabled);
        }
        if blank {
            return DeskewPolicyOutcome::without_detection(page_index, DeskewReason::BlankPage);
        }

        let policy = self.config.deskew_policy()?;
        let analysis_options = DeskewOptions::builder()
            .algorithm(DeskewAlgorithm::PageEdge)
            .max_angle(DEFAULT_MAX_ANGLE)
            .threshold_angle(0.0)
            .build();
        let detection = crate::ImageProcDeskewer::detect_skew(image_path, &analysis_options)
            .map_err(|error| PipelineError::ImageProcessingFailed(error.to_string()))?;
        DeskewPolicyOutcome::from_detection(
            page_index,
            &detection,
            self.config.deskew_action,
            policy,
        )
    }

    /// Apply one previously analyzed, policy-approved deskew proposal.
    ///
    /// The outcome is re-derived from its evidence and the current pipeline
    /// policy before pixels change. Detection is never rerun.
    pub fn apply_deskew_outcome(
        &self,
        page_index: usize,
        input_path: &Path,
        output_path: &Path,
        mut outcome: DeskewPolicyOutcome,
    ) -> Result<DeskewPolicyOutcome, PipelineError> {
        let detection = outcome.detection.clone().ok_or_else(|| {
            PipelineError::ImageProcessingFailed(
                "deskew application requires measured evidence".to_string(),
            )
        })?;
        let expected = DeskewPolicyOutcome::from_detection(
            page_index,
            &detection,
            self.config.deskew_action,
            self.config.deskew_policy()?,
        )?;
        if outcome != expected || !outcome.should_apply {
            return Err(PipelineError::ImageProcessingFailed(
                "deskew outcome is not an untampered approved proposal".to_string(),
            ));
        }

        let policy = self.config.deskew_policy()?;
        let options = DeskewOptions::builder()
            .algorithm(DeskewAlgorithm::PageEdge)
            .max_angle(policy.maximum_angle().get())
            .threshold_angle(policy.noop_angle().get())
            .background_color([255, 255, 255])
            .quality_mode(QualityMode::HighQuality)
            .build();
        let result = crate::ImageProcDeskewer::correct_skew_with_detection(
            input_path,
            output_path,
            &detection,
            &options,
        )
        .map_err(|error| PipelineError::ImageProcessingFailed(error.to_string()))?;
        if !result.corrected {
            return Err(PipelineError::ImageProcessingFailed(
                "approved deskew did not publish a transformed output".to_string(),
            ));
        }
        outcome.mark_applied();
        Ok(outcome)
    }

    /// Reject pipeline modes whose safe writer path is not implemented yet.
    pub fn ensure_execution_supported(&self) -> Result<(), PipelineError> {
        if self.config.geometry_only {
            return Err(PipelineError::UnsupportedMode(
                "geometry-only conversion is not yet implemented; use --dry-run to inspect the contract"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Get the output PDF path for a given input PDF
    pub fn get_output_path(&self, input: &Path, output_dir: &Path) -> PathBuf {
        let pdf_name = input.file_stem().unwrap_or_default().to_string_lossy();
        output_dir.join(format!("{}_converted.pdf", pdf_name))
    }

    /// Get the transform manifest path for a given input PDF
    pub fn get_transform_manifest_path(&self, input: &Path, output_dir: &Path) -> PathBuf {
        let mut manifest_name = input.file_stem().unwrap_or_default().to_os_string();
        manifest_name.push(".transforms.jsonl");
        output_dir.join(manifest_name)
    }

    /// Get the working directory for a PDF
    pub fn get_work_dir(&self, input: &Path, output_dir: &Path) -> PathBuf {
        let pdf_name = input.file_stem().unwrap_or_default().to_string_lossy();
        output_dir.join(format!(".work_{}", pdf_name))
    }

    /// Process a single PDF file (silent mode)
    pub fn process(
        &self,
        input: &Path,
        output_dir: &Path,
    ) -> Result<PipelineResult, PipelineError> {
        self.process_with_progress(input, output_dir, &SilentProgress)
    }

    /// Process a single PDF file with progress callback
    ///
    /// This is the main entry point for PDF processing.
    pub fn process_with_progress<P: ProgressCallback>(
        &self,
        input: &Path,
        output_dir: &Path,
        progress: &P,
    ) -> Result<PipelineResult, PipelineError> {
        self.ensure_execution_supported()?;
        let start_time = Instant::now();

        // Validate input
        if !input.exists() {
            return Err(PipelineError::InputNotFound(input.to_path_buf()));
        }

        // Create output directory
        std::fs::create_dir_all(output_dir)?;

        let output_path = self.get_output_path(input, output_dir);
        let work_dir = self.get_work_dir(input, output_dir);
        std::fs::create_dir_all(&work_dir)?;

        // Step 1: Read PDF metadata
        progress.on_step_start("Reading PDF...");
        let reader = crate::LopdfReader::new(input)
            .map_err(|e| PipelineError::ExtractionFailed(e.to_string()))?;
        let total_pages = reader.info.page_count;
        progress.on_step_complete("Reading PDF", &format!("{} pages", total_pages));

        // Step 2: Extract images
        progress.on_step_start(&format!("Extracting images (DPI: {})...", self.config.dpi));
        let extract_options = crate::ExtractOptions::builder()
            .dpi(self.config.dpi)
            .build();
        let extracted_dir = work_dir.join("extracted");
        std::fs::create_dir_all(&extracted_dir)?;

        let mut extracted_pages =
            crate::LopdfExtractor::extract_auto(input, &extracted_dir, &extract_options)
                .map_err(|e| PipelineError::ExtractionFailed(e.to_string()))?;

        // Apply max_pages limit
        if let Some(max_pages) = self.config.max_pages {
            if extracted_pages.len() > max_pages {
                progress.on_debug(&format!("Limiting to {} pages (--max-pages)", max_pages));
                extracted_pages.truncate(max_pages);
            }
        }
        let page_count = extracted_pages.len();
        progress.on_step_complete("Extracting images", &format!("{} pages", page_count));

        // Convert to PathBuf list
        let mut current_images: Vec<PathBuf> =
            extracted_pages.iter().map(|p| p.path.clone()).collect();

        // ================================================================
        // C#版互換処理順序:
        // 1. PDF抽出 (上で完了)
        // 1.5 空白ページ検出
        // 2. マージントリム (0.5%)
        // 3. AI超解像 (RealESRGAN 2x)
        // 4. 内部解像度正規化 (4960x7016)
        // 5. 傾き補正 (Deskew) ← C#はここで実行
        // 6. 色補正
        // ================================================================

        // Step 1.5: Detect blank pages (used to skip processing steps)
        let blank_pages = Self::detect_blank_pages(&current_images, progress);

        // Step 2: Margin Trimming (C# does this first)
        // Note: margin_trim is a percentage, skip if 0
        if self.config.margin_trim > 0.0 {
            current_images =
                self.step_margin_trim(&work_dir, &current_images, &blank_pages, progress)?;
        }

        // Step 2.5: Shadow Removal (after margin trim, before upscale)
        if self.config.shadow_removal != "none" {
            current_images =
                self.step_shadow_removal(&work_dir, &current_images, &blank_pages, progress)?;
        }

        // Step 3: AI Upscaling (if enabled)
        if self.config.upscale {
            current_images = self.step_upscale(&work_dir, &current_images, progress)?;
        }

        // Step 3.5: Deblur (after upscale, before normalization)
        if self.config.deblur {
            current_images =
                self.step_deblur(&work_dir, &current_images, &blank_pages, progress)?;
        }

        // Step 4: Internal Resolution Normalization (if enabled)
        // C#: Fit to 4960x7016 with Lanczos3, padding with paper color
        // Skip blank pages to avoid checkerboard/transparency artifacts
        if self.config.internal_resolution {
            current_images =
                self.step_normalize(&work_dir, &current_images, &blank_pages, progress)?;
        }

        // Step 4.5: 180-degree rotation analysis/application (before deskew)
        if self.config.rotation_stage_enabled() {
            current_images = self.step_rotation_detect(&work_dir, &current_images, progress)?;
        }

        // Step 5: Deskew (if enabled) - C# does deskew AFTER normalization
        if self.config.deskew_stage_enabled() {
            current_images =
                self.step_deskew(&work_dir, &current_images, &blank_pages, progress)?;
        }

        // Step 6: Color Correction (if enabled)
        // Skip blank pages to avoid color artifacts
        if self.config.color_correction {
            current_images =
                self.step_color_correction(&work_dir, &current_images, &blank_pages, progress)?;
        }

        // Step 6.5: Marker Removal (after color correction, before group crop)
        if self.config.remove_markers {
            current_images =
                self.step_marker_removal(&work_dir, &current_images, &blank_pages, progress)?;
        }

        // Step 8: Tukey Fence Group Crop (if offset_alignment enabled)
        // Exclude blank pages from bounding box detection
        if self.config.offset_alignment {
            current_images =
                self.step_group_crop(&work_dir, &current_images, &blank_pages, progress)?;
        }

        // Step 9: Page Number Offset Calculation
        let page_number_shift = if self.config.offset_alignment {
            self.step_page_number_detection(&current_images, progress)?
        } else {
            None
        };

        // Step 10: Final Output (resize)
        if self.config.output_height != 0 && self.config.output_height != 7016 {
            current_images = self.step_finalize(&work_dir, &current_images, progress)?;
        }

        // Step 11: Vertical Text Detection
        let is_vertical = self.step_vertical_detection(&current_images, progress)?;

        // Step 12: OCR with YomiToku (if enabled)
        let ocr_results = if self.config.ocr {
            self.step_ocr(&current_images, progress)?
        } else {
            vec![]
        };

        // Step 13: Generate PDF
        progress.on_step_start("Generating output PDF...");
        self.step_generate_pdf(
            &current_images,
            &output_path,
            &reader.info,
            &ocr_results,
            page_number_shift,
            is_vertical,
            progress,
        )?;

        // Get output file size
        let output_size = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);
        progress.on_step_complete("Generating PDF", &format!("{} bytes", output_size));

        // Cleanup work directory (unless save_debug)
        if !self.config.save_debug {
            std::fs::remove_dir_all(&work_dir).ok();
        }

        let elapsed = start_time.elapsed().as_secs_f64();

        Ok(PipelineResult::new(
            page_count,
            page_number_shift,
            is_vertical,
            elapsed,
            output_path,
            output_size,
            None,
        ))
    }

    // ============ Processing Step Implementations ============

    /// Minimum non-background pixel ratio to consider a page as having content
    const BLANK_PAGE_THRESHOLD: f64 = 0.02; // 2% (was 1%, increased to catch near-blank pages)

    /// Detect blank pages (< 1% non-background pixels)
    ///
    /// Returns a boolean vec where `true` means the page is blank.
    fn detect_blank_pages<P: ProgressCallback>(images: &[PathBuf], progress: &P) -> Vec<bool> {
        progress.on_step_start("Detecting blank pages...");
        let results: Vec<bool> = images
            .par_iter()
            .map(|img_path| {
                if let Ok(img) = image::open(img_path) {
                    let gray = img.to_luma8();
                    let (width, height) = gray.dimensions();
                    let total_pixels = (width as u64) * (height as u64);
                    if total_pixels == 0 {
                        return true;
                    }

                    // Count non-background pixels (value < 240 = not near-white)
                    let non_bg_count: u64 = gray.pixels().filter(|p| p.0[0] < 240).count() as u64;

                    let ratio = non_bg_count as f64 / total_pixels as f64;
                    ratio < Self::BLANK_PAGE_THRESHOLD
                } else {
                    false // If we can't open it, assume not blank
                }
            })
            .collect();

        let blank_count = results.iter().filter(|&&b| b).count();
        if blank_count > 0 {
            progress.on_debug(&format!(
                "Detected {} blank page(s) (will skip deskew/margin trim)",
                blank_count
            ));
        }
        progress.on_step_complete(
            "Blank detection",
            &format!("{} blank / {} total", blank_count, results.len()),
        );
        results
    }

    /// Step 4.5: conservative 180-degree rotation analysis and policy application.
    fn step_rotation_detect<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start("Analyzing page rotation...");
        let rotated_dir = work_dir.join("rotation_corrected");
        std::fs::create_dir_all(&rotated_dir)?;

        let output_paths: Vec<PathBuf> = images
            .iter()
            .enumerate()
            .map(|(idx, image)| {
                let name = image.file_name().map_or_else(
                    || std::ffi::OsString::from(format!("page_{idx:04}.png")),
                    std::ffi::OsStr::to_os_string,
                );
                rotated_dir.join(name)
            })
            .collect();

        let results: Result<Vec<(PathBuf, RotationPolicyOutcome)>, PipelineError> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .enumerate()
            .map(|(page_index, (image_path, output_path))| {
                let mut outcome =
                    self.analyze_rotation(page_index, image_path)?
                        .ok_or_else(|| {
                            PipelineError::ImageProcessingFailed(
                                "rotation stage was invoked while rotation analysis was disabled"
                                    .to_string(),
                            )
                        })?;

                if outcome.should_apply {
                    crate::ImageProcDeskewer::correct_upside_down(image_path, output_path)
                        .map_err(|error| PipelineError::ImageProcessingFailed(error.to_string()))?;
                    outcome.mark_applied();
                } else {
                    std::fs::copy(image_path, output_path)?;
                }
                Ok((output_path.clone(), outcome))
            })
            .collect();

        let results = results?;
        let applied = results
            .iter()
            .filter(|(_, outcome)| outcome.should_apply)
            .count();
        let review_required = results
            .iter()
            .filter(|(_, outcome)| outcome.review_required)
            .count();
        progress.on_step_complete(
            "Rotation analysis",
            &format!("{applied} pages corrected (180°), {review_required} require review"),
        );
        Ok(results.into_iter().map(|(path, _)| path).collect())
    }

    /// Step 5: conservative deskew analysis and policy application.
    fn step_deskew<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        blank_pages: &[bool],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start("Analyzing page skew...");
        if blank_pages.len() != images.len() {
            return Err(PipelineError::ImageProcessingFailed(format!(
                "deskew blank-page cardinality mismatch: {} images, {} flags",
                images.len(),
                blank_pages.len()
            )));
        }
        let deskewed_dir = work_dir.join("deskewed");
        std::fs::create_dir_all(&deskewed_dir)?;
        self.config.deskew_policy()?;
        let output_paths: Vec<PathBuf> = images
            .iter()
            .enumerate()
            .map(|(index, image)| {
                let name = image.file_name().map_or_else(
                    || std::ffi::OsString::from(format!("page_{index:04}.png")),
                    std::ffi::OsStr::to_os_string,
                );
                deskewed_dir.join(name)
            })
            .collect();

        let results: Result<Vec<(PathBuf, DeskewPolicyOutcome)>, PipelineError> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .zip(blank_pages.par_iter())
            .enumerate()
            .map(|(page_index, ((input_path, output_path), blank))| {
                let mut outcome = self.analyze_deskew(page_index, input_path, *blank)?;
                if outcome.should_apply {
                    outcome =
                        self.apply_deskew_outcome(page_index, input_path, output_path, outcome)?;
                } else {
                    std::fs::copy(input_path, output_path)?;
                }
                Ok((output_path.clone(), outcome))
            })
            .collect();

        let results = results?;
        let applied = results
            .iter()
            .filter(|(_, outcome)| outcome.transform.decision == TransformDecision::Applied)
            .count();
        let proposed = results
            .iter()
            .filter(|(_, outcome)| outcome.transform.decision == TransformDecision::Proposed)
            .count();
        let review_required = results
            .iter()
            .filter(|(_, outcome)| outcome.review_required)
            .count();
        progress.on_step_complete(
            "Deskew",
            &format!("{applied} corrected, {proposed} proposed, {review_required} require review"),
        );
        Ok(results.into_iter().map(|(path, _)| path).collect())
    }

    /// Step 2.5: Shadow removal (after margin trim, before upscale)
    fn step_shadow_removal<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        blank_pages: &[bool],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start(&format!(
            "Removing binding shadows (mode: {})...",
            self.config.shadow_removal
        ));
        let shadow_dir = work_dir.join("shadow_removed");
        std::fs::create_dir_all(&shadow_dir)?;

        let shadow_options = match self.config.shadow_removal.as_str() {
            "left" => crate::margin::shadow::ShadowRemovalOptions::left_only(),
            "right" => crate::margin::shadow::ShadowRemovalOptions::right_only(),
            "both" => crate::margin::shadow::ShadowRemovalOptions::both_horizontal(),
            _ => crate::margin::shadow::ShadowRemovalOptions::default(), // "auto"
        };

        let output_paths: Vec<PathBuf> = images
            .iter()
            .enumerate()
            .map(|(idx, img)| {
                let name = img
                    .file_name()
                    .map(|n| n.to_os_string())
                    .unwrap_or_else(|| std::ffi::OsString::from(format!("page_{:04}.png", idx)));
                shadow_dir.join(name)
            })
            .collect();

        let removed_count = AtomicUsize::new(0);

        let results: Vec<PathBuf> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .enumerate()
            .map(|(idx, (img_path, output_path))| {
                if blank_pages.get(idx).copied().unwrap_or(false) {
                    std::fs::copy(img_path, output_path).ok();
                    return output_path.clone();
                }

                match crate::margin::shadow::ShadowDetector::remove_shadows(
                    img_path,
                    output_path,
                    &shadow_options,
                ) {
                    Ok(result) => {
                        if result.has_shadows() {
                            removed_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        std::fs::copy(img_path, output_path).ok();
                    }
                }
                output_path.clone()
            })
            .collect();

        let count = removed_count.load(Ordering::Relaxed);
        progress.on_step_complete(
            "Shadow removal",
            &format!("{} shadows found in {} images", count, results.len()),
        );
        Ok(results)
    }

    /// Step 3.5: Deblur (after upscale, before normalization)
    fn step_deblur<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        blank_pages: &[bool],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start(&format!(
            "Applying deblur ({})...",
            self.config.deblur_algorithm
        ));
        let deblur_dir = work_dir.join("deblurred");
        std::fs::create_dir_all(&deblur_dir)?;

        let algorithm = match self.config.deblur_algorithm.as_str() {
            "nafnet" => crate::cleanup::deblur::DeblurAlgorithm::NafNet,
            "deblurgan_v2" | "deblurganv2" => crate::cleanup::deblur::DeblurAlgorithm::DeblurGanV2,
            _ => crate::cleanup::deblur::DeblurAlgorithm::UnsharpMask,
        };

        let deblur_options = crate::cleanup::deblur::DeblurOptions::builder()
            .algorithm(algorithm)
            .build();

        let output_paths: Vec<PathBuf> = images
            .iter()
            .enumerate()
            .map(|(idx, img)| {
                let name = img
                    .file_name()
                    .map(|n| n.to_os_string())
                    .unwrap_or_else(|| std::ffi::OsString::from(format!("page_{:04}.png", idx)));
                deblur_dir.join(name)
            })
            .collect();

        let processed_count = AtomicUsize::new(0);

        let results: Vec<PathBuf> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .enumerate()
            .map(|(idx, (img_path, output_path))| {
                if blank_pages.get(idx).copied().unwrap_or(false) {
                    std::fs::copy(img_path, output_path).ok();
                    return output_path.clone();
                }

                match crate::cleanup::deblur::Deblurrer::process(
                    img_path,
                    output_path,
                    &deblur_options,
                ) {
                    Ok(result) => {
                        if result.processed {
                            processed_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        std::fs::copy(img_path, output_path).ok();
                    }
                }
                output_path.clone()
            })
            .collect();

        let count = processed_count.load(Ordering::Relaxed);
        progress.on_step_complete(
            "Deblur",
            &format!("{} deblurred / {} total", count, results.len()),
        );
        Ok(results)
    }

    /// Step 6.5: Marker removal (after color correction, before group crop)
    fn step_marker_removal<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        blank_pages: &[bool],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start("Removing highlighter markers...");
        let marker_dir = work_dir.join("marker_removed");
        std::fs::create_dir_all(&marker_dir)?;

        // Parse marker colors from config strings
        let colors: Vec<crate::cleanup::marker_removal::HighlighterColor> = self
            .config
            .marker_colors
            .iter()
            .filter_map(|c| match c.to_lowercase().as_str() {
                "yellow" => Some(crate::cleanup::marker_removal::HighlighterColor::Yellow),
                "pink" => Some(crate::cleanup::marker_removal::HighlighterColor::Pink),
                "green" => Some(crate::cleanup::marker_removal::HighlighterColor::Green),
                "blue" => Some(crate::cleanup::marker_removal::HighlighterColor::Blue),
                "orange" => Some(crate::cleanup::marker_removal::HighlighterColor::Orange),
                _ => None,
            })
            .collect();

        let marker_options = crate::cleanup::marker_removal::MarkerRemovalOptions::builder()
            .colors(if colors.is_empty() {
                crate::cleanup::marker_removal::HighlighterColor::all()
            } else {
                colors
            })
            .build();

        let output_paths: Vec<PathBuf> = images
            .iter()
            .enumerate()
            .map(|(idx, img)| {
                let name = img
                    .file_name()
                    .map(|n| n.to_os_string())
                    .unwrap_or_else(|| std::ffi::OsString::from(format!("page_{:04}.png", idx)));
                marker_dir.join(name)
            })
            .collect();

        let removed_count = AtomicUsize::new(0);

        let results: Vec<PathBuf> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .enumerate()
            .map(|(idx, (img_path, output_path))| {
                if blank_pages.get(idx).copied().unwrap_or(false) {
                    std::fs::copy(img_path, output_path).ok();
                    return output_path.clone();
                }

                match crate::cleanup::marker_removal::MarkerRemover::remove(
                    img_path,
                    output_path,
                    &marker_options,
                ) {
                    Ok(result) => {
                        if result.has_markers() {
                            removed_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        std::fs::copy(img_path, output_path).ok();
                    }
                }
                output_path.clone()
            })
            .collect();

        let count = removed_count.load(Ordering::Relaxed);
        progress.on_step_complete(
            "Marker removal",
            &format!("{} pages with markers / {} total", count, results.len()),
        );
        Ok(results)
    }

    /// Step 2: Margin trimming (C#互換: 単純な固定%カット)
    ///
    /// C#版と同様に、各辺から指定%を単純にカットする。
    /// 空白ページはスキップする。
    fn step_margin_trim<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        blank_pages: &[bool],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start(&format!(
            "Trimming margins ({}%)...",
            self.config.margin_trim
        ));
        let trimmed_dir = work_dir.join("trimmed");
        std::fs::create_dir_all(&trimmed_dir)?;

        let trim_percent = self.config.margin_trim / 100.0; // Convert to ratio

        let output_paths: Vec<PathBuf> = images
            .iter()
            .enumerate()
            .map(|(idx, img)| {
                let name = img
                    .file_name()
                    .map(|n| n.to_os_string())
                    .unwrap_or_else(|| std::ffi::OsString::from(format!("page_{:04}.png", idx)));
                trimmed_dir.join(name)
            })
            .collect();

        let results: Vec<PathBuf> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .enumerate()
            .map(|(idx, (img_path, output_path))| {
                // Skip blank pages
                if blank_pages.get(idx).copied().unwrap_or(false) {
                    std::fs::copy(img_path, output_path).ok();
                    return output_path.clone();
                }

                // C#互換: 単純な固定%カット
                // Force RGB conversion to prevent transparency/checkerboard artifacts
                if let Ok(img) = image::open(img_path) {
                    let (w, h) = (img.width(), img.height());
                    let trim_x = (w as f64 * trim_percent) as u32;
                    let trim_y = (h as f64 * trim_percent) as u32;

                    let new_x = trim_x;
                    let new_y = trim_y;
                    let new_w = w.saturating_sub(trim_x * 2);
                    let new_h = h.saturating_sub(trim_y * 2);

                    if new_w > 0 && new_h > 0 {
                        let cropped = img.crop_imm(new_x, new_y, new_w, new_h);
                        // Convert to RGB (no alpha) to avoid checkerboard artifacts
                        let rgb = cropped.to_rgb8();
                        rgb.save(output_path).ok();
                    } else {
                        let rgb = img.to_rgb8();
                        rgb.save(output_path).ok();
                    }
                } else {
                    std::fs::copy(img_path, output_path).ok();
                }
                output_path.clone()
            })
            .collect();

        progress.on_step_complete("Margin trim", &format!("{} images", results.len()));
        Ok(results)
    }

    /// Step 5: AI Upscaling
    fn step_upscale<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start("AI Upscaling (RealESRGAN)...");
        let upscaled_dir = work_dir.join("upscaled");
        std::fs::create_dir_all(&upscaled_dir)?;

        // Try to initialize RealESRGAN
        let venv_path = crate::resolve_venv_path();

        let bridge_config = crate::AiBridgeConfig::builder()
            .venv_path(venv_path)
            .build();

        let bridge = match crate::SubprocessBridge::new(bridge_config) {
            Ok(b) => b,
            Err(e) => {
                progress.on_warning(&format!("RealESRGAN not available: {}", e));
                return Ok(images.to_vec());
            }
        };

        let esrgan = crate::RealEsrgan::new(bridge);
        let mut options = crate::RealEsrganOptions::builder().scale(2);
        if self.config.gpu {
            options = options.gpu_id(0);
        }
        let options = options.build();

        match esrgan.upscale_batch(images, &upscaled_dir, &options, None) {
            Ok(result) => {
                progress
                    .on_step_complete("Upscaling", &format!("{} images", result.successful.len()));
                Ok(result
                    .successful
                    .iter()
                    .map(|r| r.output_path.clone())
                    .collect())
            }
            Err(e) => {
                progress.on_warning(&format!("Upscaling failed: {}", e));
                Ok(images.to_vec())
            }
        }
    }

    /// Step 6: Internal resolution normalization (skip blank pages)
    fn step_normalize<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        blank_pages: &[bool],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start("Normalizing to internal resolution (4960x7016)...");
        let normalized_dir = work_dir.join("normalized");
        std::fs::create_dir_all(&normalized_dir)?;

        let normalize_options = crate::NormalizeOptions::builder()
            .target_width(4960)
            .target_height(7016)
            .build();

        let output_paths: Vec<PathBuf> = (0..images.len())
            .map(|i| normalized_dir.join(format!("page_{:04}.png", i)))
            .collect();

        let completed = Arc::new(AtomicUsize::new(0));

        let results: Vec<PathBuf> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .enumerate()
            .map(|(idx, (img_path, output_path))| {
                // Skip blank pages to avoid transparency/checkerboard artifacts
                if blank_pages.get(idx).copied().unwrap_or(false) {
                    std::fs::copy(img_path, output_path).ok();
                } else {
                    match crate::ImageNormalizer::normalize(
                        img_path,
                        output_path,
                        &normalize_options,
                    ) {
                        Ok(_) => {}
                        Err(_) => {
                            std::fs::copy(img_path, output_path).ok();
                        }
                    }
                }
                completed.fetch_add(1, Ordering::Relaxed);
                output_path.clone()
            })
            .collect();

        progress.on_step_complete("Normalization", &format!("{} images", results.len()));
        Ok(results)
    }

    /// Step 7: Color correction (skip blank pages in stats collection)
    fn step_color_correction<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        blank_pages: &[bool],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start("Analyzing color statistics...");
        let color_corrected_dir = work_dir.join("color_corrected");
        std::fs::create_dir_all(&color_corrected_dir)?;

        // Collect color statistics (exclude blank pages from global stats)
        let stats_results: Vec<_> = images
            .par_iter()
            .enumerate()
            .map(|(idx, img_path)| {
                if blank_pages.get(idx).copied().unwrap_or(false) {
                    Err(std::io::Error::other("blank page").into())
                } else {
                    crate::ColorAnalyzer::calculate_stats(img_path)
                }
            })
            .collect();

        let all_stats: Vec<_> = stats_results.into_iter().filter_map(|r| r.ok()).collect();

        if all_stats.is_empty() {
            progress.on_debug("Color analysis failed, skipping correction");
            return Ok(images.to_vec());
        }

        let global_param = crate::ColorAnalyzer::decide_global_adjustment(&all_stats);

        // Debug: 実際のカラー補正パラメータを出力
        progress.on_debug(&format!(
            "Color params: scale=({:.3},{:.3},{:.3}), offset=({:.1},{:.1},{:.1}), ghost_thr={}, paper=({},{},{})",
            global_param.scale_r, global_param.scale_g, global_param.scale_b,
            global_param.offset_r, global_param.offset_g, global_param.offset_b,
            global_param.ghost_suppress_threshold,
            global_param.paper_r, global_param.paper_g, global_param.paper_b
        ));

        let output_paths: Vec<PathBuf> = (0..images.len())
            .map(|i| color_corrected_dir.join(format!("page_{:04}.png", i)))
            .collect();

        let results: Vec<PathBuf> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .enumerate()
            .map(|(idx, (img_path, output_path))| {
                // Skip blank pages
                if blank_pages.get(idx).copied().unwrap_or(false) {
                    std::fs::copy(img_path, output_path).ok();
                } else if let Ok(img) = image::open(img_path) {
                    let mut rgb_img = img.to_rgb8();
                    crate::ColorAnalyzer::apply_adjustment(&mut rgb_img, &global_param);
                    rgb_img.save(output_path).ok();
                } else {
                    std::fs::copy(img_path, output_path).ok();
                }
                output_path.clone()
            })
            .collect();

        progress.on_step_complete("Color correction", &format!("{} images", results.len()));
        Ok(results)
    }

    /// Step 8: Tukey fence group crop (skip blank pages in detection)
    fn step_group_crop<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        blank_pages: &[bool],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start("Detecting text bounding boxes...");
        let cropped_dir = work_dir.join("cropped");
        std::fs::create_dir_all(&cropped_dir)?;

        // Only detect bounding boxes on non-blank pages
        let non_blank_images: Vec<PathBuf> = images
            .iter()
            .enumerate()
            .filter(|(idx, _)| !blank_pages.get(*idx).copied().unwrap_or(false))
            .map(|(_, p)| p.clone())
            .collect();

        let bounding_boxes =
            crate::GroupCropAnalyzer::detect_all_bounding_boxes(&non_blank_images, 240);

        if bounding_boxes.is_empty() {
            progress.on_debug("No bounding boxes detected, skipping crop");
            return Ok(images.to_vec());
        }

        let unified = crate::GroupCropAnalyzer::unify_and_expand_regions(
            &bounding_boxes,
            10,   // 10% margin expansion (was 8%, increased to prevent edge clipping)
            4960, // internal width limit
            7016, // internal height limit
        );

        let output_paths: Vec<PathBuf> = (0..images.len())
            .map(|i| cropped_dir.join(format!("page_{:04}.png", i)))
            .collect();

        let results: Vec<PathBuf> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .enumerate()
            .map(|(i, (img_path, output_path))| {
                // Skip blank pages
                if blank_pages.get(i).copied().unwrap_or(false) {
                    std::fs::copy(img_path, output_path).ok();
                    return output_path.clone();
                }

                let region = if i % 2 == 0 {
                    &unified.odd_region
                } else {
                    &unified.even_region
                };

                if let Ok(img) = image::open(img_path) {
                    let cropped = img.crop_imm(
                        region.left,
                        region.top,
                        region.width.min(img.width().saturating_sub(region.left)),
                        region.height.min(img.height().saturating_sub(region.top)),
                    );
                    // Force RGB to prevent transparency artifacts
                    let rgb = cropped.to_rgb8();
                    rgb.save(output_path).ok();
                } else {
                    std::fs::copy(img_path, output_path).ok();
                }
                output_path.clone()
            })
            .collect();

        progress.on_step_complete("Group crop", &format!("{} images", results.len()));
        Ok(results)
    }

    /// Step 9: Page number detection
    fn step_page_number_detection<P: ProgressCallback>(
        &self,
        images: &[PathBuf],
        progress: &P,
    ) -> Result<Option<i32>, PipelineError> {
        progress.on_step_start("Detecting page numbers...");

        let page_options = crate::PageNumberOptions::default();
        let mut page_detections = Vec::new();

        for (i, img_path) in images.iter().enumerate() {
            if let Ok(detection) =
                crate::TesseractPageDetector::detect_single(img_path, i, &page_options)
            {
                page_detections.push(detection);
            }
        }

        if page_detections.is_empty() {
            progress.on_step_complete("Page number detection", "no pages detected");
            return Ok(None);
        }

        let first_img = image::open(&images[0]).ok();
        let img_height = first_img.as_ref().map(|img| img.height()).unwrap_or(7016);

        let analysis = crate::PageOffsetAnalyzer::analyze_offsets(&page_detections, img_height);
        let shift_msg = if analysis.page_number_shift == 0 {
            "aligned".to_string()
        } else {
            format!("offset: {}px", analysis.page_number_shift)
        };
        progress.on_step_complete("Page number detection", &shift_msg);

        Ok(Some(analysis.page_number_shift))
    }

    /// Step 10: Finalize output
    fn step_finalize<P: ProgressCallback>(
        &self,
        work_dir: &Path,
        images: &[PathBuf],
        progress: &P,
    ) -> Result<Vec<PathBuf>, PipelineError> {
        progress.on_step_start(&format!(
            "Finalizing output (height: {})...",
            self.config.output_height
        ));
        let finalized_dir = work_dir.join("finalized");
        std::fs::create_dir_all(&finalized_dir)?;

        let finalize_options = crate::FinalizeOptions::builder()
            .target_height(self.config.output_height)
            .build();

        let output_paths: Vec<PathBuf> = (0..images.len())
            .map(|i| finalized_dir.join(format!("page_{:04}.png", i)))
            .collect();

        let results: Vec<PathBuf> = images
            .par_iter()
            .zip(output_paths.par_iter())
            .map(|(img_path, output_path)| {
                match crate::PageFinalizer::finalize(
                    img_path,
                    output_path,
                    &finalize_options,
                    None,
                    0,
                    0,
                ) {
                    Ok(_) => {}
                    Err(_) => {
                        std::fs::copy(img_path, output_path).ok();
                    }
                }
                output_path.clone()
            })
            .collect();

        progress.on_step_complete("Output finalized", &format!("{} pages", results.len()));
        Ok(results)
    }

    /// Step 11: Vertical text detection
    fn step_vertical_detection<P: ProgressCallback>(
        &self,
        images: &[PathBuf],
        progress: &P,
    ) -> Result<bool, PipelineError> {
        if images.is_empty() {
            return Ok(false);
        }

        progress.on_step_start("Detecting text direction...");

        let mut gray_images = Vec::new();
        for img_path in images.iter().take(20) {
            if let Ok(img) = image::open(img_path) {
                gray_images.push(img.to_luma8());
            }
        }

        if gray_images.is_empty() {
            progress.on_step_complete("Text direction", "no images to analyze");
            return Ok(false);
        }

        let vd_options = crate::VerticalDetectOptions::default();
        match crate::detect_book_vertical_writing(&gray_images, &vd_options) {
            Ok(result) => {
                let direction = if result.is_vertical {
                    "vertical"
                } else {
                    "horizontal"
                };
                progress.on_step_complete("Text direction", direction);
                Ok(result.is_vertical)
            }
            Err(_) => {
                progress.on_step_complete("Text direction", "detection failed");
                Ok(false)
            }
        }
    }

    /// Step 12: OCR with YomiToku
    fn step_ocr<P: ProgressCallback>(
        &self,
        images: &[PathBuf],
        progress: &P,
    ) -> Result<Vec<Option<crate::OcrResult>>, PipelineError> {
        progress.on_step_start("Running OCR (YomiToku)...");

        let venv_path = crate::resolve_venv_path();

        let bridge_config = crate::AiBridgeConfig::builder()
            .venv_path(venv_path)
            .build();

        // OCR only runs when explicitly requested (--ocr), so a missing venv
        // is a hard error rather than a silent no-OCR fallback (issue #55).
        let bridge = crate::SubprocessBridge::new(bridge_config)
            .map_err(|e| PipelineError::OcrUnavailable(e.to_string()))?;

        let yomitoku = crate::YomiToku::new(bridge);
        if !yomitoku.is_available() {
            return Err(PipelineError::OcrUnavailable(
                "the venv exists but `import yomitoku` failed".to_string(),
            ));
        }
        let mut ocr_opts = crate::YomiTokuOptions::builder();
        if self.config.gpu {
            ocr_opts = ocr_opts.use_gpu(true).gpu_id(0);
        }
        let ocr_opts = ocr_opts.build();

        let mut results = Vec::new();
        for img_path in images {
            match yomitoku.ocr(img_path, &ocr_opts) {
                Ok(result) => results.push(Some(result)),
                Err(_) => results.push(None),
            }
        }

        let success_count = results.iter().filter(|r| r.is_some()).count();
        progress.on_step_complete("OCR", &format!("{}/{} pages", success_count, results.len()));
        Ok(results)
    }

    /// Step 13: Generate PDF
    #[allow(clippy::too_many_arguments)]
    fn step_generate_pdf<P: ProgressCallback>(
        &self,
        images: &[PathBuf],
        output_path: &Path,
        pdf_info: &crate::PdfDocument,
        ocr_results: &[Option<crate::OcrResult>],
        page_number_shift: Option<i32>,
        is_vertical: bool,
        _progress: &P,
    ) -> Result<(), PipelineError> {
        use crate::pdf_writer::{OcrLayer, OcrPageText, PdfViewerHints, TextBlock};

        // Convert OCR results to OcrLayer
        let ocr_layer = if !ocr_results.is_empty() {
            let pages: Vec<OcrPageText> = ocr_results
                .iter()
                .enumerate()
                .filter_map(|(idx, result): (usize, &Option<crate::OcrResult>)| {
                    result.as_ref().map(|r| OcrPageText {
                        page_index: idx,
                        blocks: r
                            .text_blocks
                            .iter()
                            .map(|b| TextBlock {
                                x: b.bbox.0 as f64,
                                y: b.bbox.1 as f64,
                                width: (b.bbox.2 - b.bbox.0) as f64,
                                height: (b.bbox.3 - b.bbox.1) as f64,
                                text: b.text.clone(),
                                font_size: b.font_size.unwrap_or(12.0) as f64,
                                vertical: matches!(b.direction, crate::TextDirection::Vertical),
                            })
                            .collect(),
                    })
                })
                .collect();

            if pages.is_empty() {
                None
            } else {
                Some(OcrLayer { pages })
            }
        } else {
            None
        };

        // Determine RTL reading direction:
        // Japanese books use R2L page progression regardless of text orientation.
        // Also enable for vertical text detection (covers CJK vertical writing).
        let is_rtl = is_vertical || self.config.assume_japanese_book;

        // Build viewer hints from pipeline results
        let viewer_hints = if page_number_shift.is_some() || is_rtl {
            Some(PdfViewerHints {
                page_number_shift,
                is_rtl,
                first_logical_page: 1,
            })
        } else {
            None
        };

        let mut pdf_builder = crate::PdfWriterOptions::builder()
            .dpi(self.config.dpi)
            .jpeg_quality(self.config.jpeg_quality)
            .metadata(pdf_info.metadata.clone());

        if let Some(layer) = ocr_layer {
            pdf_builder = pdf_builder.ocr_layer(layer);
        }

        if let Some(hints) = viewer_hints {
            pdf_builder = pdf_builder.viewer_hints(hints);
        }

        let pdf_options = pdf_builder.build();

        crate::PrintPdfWriter::create_from_images(images, output_path, &pdf_options)
            .map_err(|e| PipelineError::PdfGenerationFailed(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, GenericImageView, GrayImage, Luma};
    use tempfile::tempdir;

    // ============ PipelineConfig Tests ============

    #[test]
    fn test_pipeline_config_default() {
        // TC: PIPE-003
        let config = PipelineConfig::default();

        assert_eq!(config.dpi, 300);
        assert!(config.deskew);
        assert_eq!(config.rotation_min_confidence.get(), 0.90);
        assert_eq!(config.margin_trim, 0.7);
        assert!(config.upscale);
        assert!(config.gpu);
        assert!(!config.internal_resolution);
        assert!(config.color_correction);
        assert!(!config.offset_alignment);
        assert_eq!(config.output_height, 3508);
        assert!(!config.ocr);
        assert!(config.max_pages.is_none());
        assert!(!config.save_debug);
        assert_eq!(config.jpeg_quality, 90);
        assert!(config.threads.is_none());
        // Phase 3: Memory management fields
        assert_eq!(config.max_memory_mb, 0);
        assert_eq!(config.chunk_size, 0);
        assert!(!config.geometry_only);
        assert_eq!(config.rotation_action, crate::cli::GeometryAction::Apply);
        assert_eq!(config.deskew_action, crate::cli::GeometryAction::Apply);
    }

    #[test]
    fn test_geometry_only_config_forces_unrelated_stages_off() {
        use clap::Parser;

        let cli = crate::cli::Cli::try_parse_from([
            "superbook-pdf",
            "convert",
            "input.pdf",
            "--geometry-only",
            "--geometry-action",
            "apply",
            "--rotation-action",
            "report",
        ])
        .unwrap();
        let crate::cli::Commands::Convert(args) = cli.command else {
            panic!("expected convert command");
        };

        let config = PipelineConfig::from_convert_args(&args);
        assert!(config.geometry_only);
        assert_eq!(config.rotation_action, crate::cli::GeometryAction::Report);
        assert_eq!(config.deskew_action, crate::cli::GeometryAction::Apply);
        assert!(!config.upscale);
        assert!(!config.gpu);
        assert!(!config.ocr);
        assert_eq!(config.margin_trim, 0.0);
        assert_eq!(config.shadow_removal, "none");
        assert!(!config.deblur);
        assert!(!config.internal_resolution);
        assert!(!config.color_correction);
        assert!(!config.remove_markers);
        assert!(!config.offset_alignment);
        assert_eq!(config.output_height, 0);
    }

    #[test]
    fn test_geometry_only_constructor_preserves_explicit_apply_actions() {
        let config = PipelineConfig::geometry_only(GeometryAction::Apply, GeometryAction::Apply);
        let serialized = serde_json::to_string(&config).unwrap();
        let round_trip: PipelineConfig = serde_json::from_str(&serialized).unwrap();
        let pipeline = PdfPipeline::new(round_trip);

        assert_eq!(pipeline.config().rotation_action, GeometryAction::Apply);
        assert_eq!(pipeline.config().deskew_action, GeometryAction::Apply);
    }

    fn rotation_evidence(
        proposed_degrees: u16,
        score: f64,
        confidence: f64,
        reason: RotationReason,
        ambiguity_guard: bool,
    ) -> RotationEvidence {
        RotationEvidence::try_new(
            proposed_degrees,
            score,
            confidence,
            reason,
            ambiguity_guard,
            crate::RotationMetrics::default(),
        )
        .unwrap()
    }

    fn synthetic_rotation_page() -> DynamicImage {
        let mut image = GrayImage::from_pixel(600, 800, Luma([255]));
        let rows = [
            45, 56, 67, 78, 89, 100, 111, 122, 133, 144, 190, 220, 250, 270, 650, 700,
        ];
        for (line, y) in rows.into_iter().enumerate() {
            for glyph in 0..20 {
                let x = 50 + glyph * 25;
                let width = 6 + ((line + glyph as usize) % 3) as u32;
                for yy in y..y + 6 {
                    for xx in x..x + width {
                        image.put_pixel(xx, yy, Luma([0]));
                    }
                }
            }
        }
        DynamicImage::ImageLuma8(image)
    }

    fn rotation_pipeline(action: GeometryAction) -> PdfPipeline {
        PdfPipeline::new(PipelineConfig {
            rotation_action: action,
            rotation_action_configured: true,
            rotation_min_confidence: RotationConfidenceThreshold::default(),
            ..PipelineConfig::default()
        })
    }

    fn deskew_policy() -> DeskewPolicyOptions {
        DeskewPolicyOptions::default()
    }

    fn deskew_detection(angle: f64, confidence: f64, features: usize) -> SkewDetection {
        SkewDetection::try_new(angle, confidence, features).unwrap()
    }

    fn synthetic_page_edge(angle_degrees: f64) -> DynamicImage {
        let width = 320_u32;
        let height = 480_u32;
        let slope = angle_degrees.to_radians().tan();
        let mut image = GrayImage::from_pixel(width, height, Luma([255]));
        for y in 0..height {
            let edge = (25.0 + slope * f64::from(y)).round().clamp(6.0, 60.0) as u32;
            for x in edge..width {
                image.put_pixel(x, y, Luma([32]));
            }
        }
        DynamicImage::ImageLuma8(image)
    }

    fn write_one_bit_pbm(path: &Path) {
        let width = 64;
        let height = 96;
        let bytes_per_row = width / 8;
        let mut packed = vec![0x00_u8; (bytes_per_row * height) as usize];
        for y in 8..88 {
            if (y / 6) % 2 == 0 {
                packed[(y * bytes_per_row + 2) as usize] = 0xff;
            }
        }
        let mut encoded = format!("P4\n{width} {height}\n").into_bytes();
        encoded.extend_from_slice(&packed);
        std::fs::write(path, encoded).expect("encode one-bit PBM");
    }

    #[test]
    fn deskew_policy_maps_boundaries_and_review_states() {
        let policy = deskew_policy();
        let cases = [
            (
                deskew_detection(0.10, 0.90, 100),
                GeometryAction::Apply,
                TransformDecision::Unchanged,
                DeskewReason::WithinNoopThreshold,
                false,
                false,
            ),
            (
                deskew_detection(5.0, 0.90, 100),
                GeometryAction::Apply,
                TransformDecision::Proposed,
                DeskewReason::CorrectionEvidence,
                true,
                false,
            ),
            (
                deskew_detection(5.001, 1.0, 1000),
                GeometryAction::Apply,
                TransformDecision::Rejected,
                DeskewReason::AngleOutsideLimit,
                false,
                true,
            ),
            (
                deskew_detection(0.05, 0.89, 1000),
                GeometryAction::Apply,
                TransformDecision::Rejected,
                DeskewReason::InsufficientConfidence,
                false,
                true,
            ),
            (
                deskew_detection(1.0, 1.0, 99),
                GeometryAction::Apply,
                TransformDecision::Rejected,
                DeskewReason::InsufficientFeatures,
                false,
                true,
            ),
            (
                deskew_detection(-2.0, 0.95, 150),
                GeometryAction::Report,
                TransformDecision::Proposed,
                DeskewReason::CorrectionEvidence,
                false,
                true,
            ),
        ];

        for (detection, action, decision, reason, should_apply, review_required) in cases {
            let outcome =
                DeskewPolicyOutcome::from_detection(9, &detection, action, policy).unwrap();
            assert_eq!(outcome.page_index, 9);
            assert_eq!(outcome.detection.as_ref(), Some(&detection));
            assert_eq!(outcome.transform.decision, decision);
            assert_eq!(outcome.transform.reason, reason.as_str());
            assert_eq!(outcome.should_apply, should_apply);
            assert_eq!(outcome.review_required, review_required);
            assert_eq!(outcome.transform.proposed_degrees.get(), detection.angle);
            assert!(outcome.application.is_none());
        }
    }

    #[test]
    fn deskew_outcome_becomes_applied_only_with_publication_metadata() {
        let mut outcome = DeskewPolicyOutcome::from_detection(
            9,
            &deskew_detection(-1.25, 0.97, 420),
            GeometryAction::Apply,
            deskew_policy(),
        )
        .unwrap();
        assert_eq!(outcome.page_index, 9);
        assert_eq!(outcome.detection.as_ref().unwrap().angle, -1.25);
        assert_eq!(outcome.transform.proposed_degrees.get(), -1.25);
        assert_eq!(outcome.transform.decision, TransformDecision::Proposed);
        assert!(outcome.application.is_none());

        outcome.mark_applied();
        assert_eq!(outcome.transform.decision, TransformDecision::Applied);
        assert_eq!(
            outcome.application,
            Some(DeskewApplicationMetadata::current())
        );
        let encoded = serde_json::to_string(&outcome.application.unwrap()).unwrap();
        assert!(encoded.contains("\"interpolation\":\"lanczos3\""));
        assert!(encoded.contains("\"output_pixel_mode\":\"rgba8\""));
        assert!(serde_json::from_str::<DeskewApplicationMetadata>(
            r#"{"interpolation":"lanczos3","canvas":"expanded","fill_rgba":[255,255,255,255],"output_pixel_mode":"rgba8","extra":true}"#,
        )
        .is_err());
    }

    #[test]
    fn public_deskew_apply_consumes_only_an_untampered_proposal() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("page.png");
        let output = temp.path().join("applied.png");
        synthetic_page_edge(2.0).save(&input).unwrap();
        let pipeline = PdfPipeline::new(PipelineConfig {
            deskew_action: GeometryAction::Apply,
            deskew_action_configured: true,
            ..PipelineConfig::default()
        });

        let proposal = pipeline.analyze_deskew(12, &input, false).unwrap();
        let applied = pipeline
            .apply_deskew_outcome(12, &input, &output, proposal)
            .unwrap();
        assert_eq!(applied.page_index, 12);
        assert_eq!(applied.transform.decision, TransformDecision::Applied);
        assert_eq!(
            applied.application,
            Some(DeskewApplicationMetadata::current())
        );

        let mut tampered = pipeline.analyze_deskew(13, &input, false).unwrap();
        tampered.transform.reason = "tampered".to_string();
        let rejected_output = temp.path().join("rejected.png");
        assert!(pipeline
            .apply_deskew_outcome(13, &input, &rejected_output, tampered)
            .is_err());
        assert!(!rejected_output.exists());

        let mut wrong_page = pipeline.analyze_deskew(14, &input, false).unwrap();
        wrong_page.page_index = 99;
        let wrong_page_output = temp.path().join("wrong-page.png");
        assert!(pipeline
            .apply_deskew_outcome(14, &input, &wrong_page_output, wrong_page)
            .is_err());
        assert!(!wrong_page_output.exists());
    }

    #[test]
    fn deskew_disabled_and_blank_pages_do_not_read_images() {
        let missing = Path::new("/path/that/does/not/exist.png");
        let disabled = PdfPipeline::new(PipelineConfig {
            deskew_action: GeometryAction::Off,
            deskew_action_configured: true,
            ..PipelineConfig::default()
        })
        .analyze_deskew(3, missing, false)
        .unwrap();
        assert_eq!(disabled.transform.reason, DeskewReason::Disabled.as_str());
        assert!(disabled.detection.is_none());

        let blank = PdfPipeline::new(PipelineConfig {
            deskew_action: GeometryAction::Report,
            deskew_action_configured: true,
            ..PipelineConfig::default()
        })
        .analyze_deskew(4, missing, true)
        .unwrap();
        assert_eq!(blank.transform.reason, DeskewReason::BlankPage.as_str());
        assert!(blank.detection.is_none());
    }

    #[test]
    fn deskew_report_stage_preserves_bytes_and_input_order() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("z-page.png");
        let second = temp.path().join("a-page.png");
        synthetic_page_edge(2.0).save(&first).unwrap();
        synthetic_page_edge(-2.0).save(&second).unwrap();
        let source_bytes = [
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap(),
        ];
        let pipeline = PdfPipeline::new(PipelineConfig {
            deskew_action: GeometryAction::Report,
            deskew_action_configured: true,
            ..PipelineConfig::default()
        });
        let outputs = pipeline
            .step_deskew(
                temp.path(),
                &[first.clone(), second.clone()],
                &[false, false],
                &SilentProgress,
            )
            .unwrap();
        assert_eq!(outputs[0].file_name(), first.file_name());
        assert_eq!(outputs[1].file_name(), second.file_name());
        assert_eq!(std::fs::read(&outputs[0]).unwrap(), source_bytes[0]);
        assert_eq!(std::fs::read(&outputs[1]).unwrap(), source_bytes[1]);
    }

    #[test]
    fn deskew_report_preserves_bilevel_and_vertical_layout_pages() {
        let temp = tempdir().unwrap();
        let bilevel = temp.path().join("bilevel.pbm");
        write_one_bit_pbm(&bilevel);
        let mut vertical_image = GrayImage::from_pixel(600, 800, Luma([255]));
        for column in 0..8 {
            let x0 = 80 + column * 60;
            for y in (50..750).step_by(18) {
                for yy in y..(y + 9) {
                    for xx in x0..(x0 + 7) {
                        vertical_image.put_pixel(xx, yy, Luma([0]));
                    }
                }
            }
        }
        let vertical = temp.path().join("vertical.png");
        vertical_image.save(&vertical).unwrap();
        let source_bytes = [
            std::fs::read(&bilevel).unwrap(),
            std::fs::read(&vertical).unwrap(),
        ];
        assert!(
            source_bytes[0].starts_with(b"P4\n"),
            "fixture must be binary PBM"
        );

        let pipeline = PdfPipeline::new(PipelineConfig {
            deskew_action: GeometryAction::Report,
            deskew_action_configured: true,
            ..PipelineConfig::default()
        });
        let outputs = pipeline
            .step_deskew(
                temp.path(),
                &[bilevel, vertical],
                &[false, false],
                &SilentProgress,
            )
            .unwrap();
        assert_eq!(std::fs::read(&outputs[0]).unwrap(), source_bytes[0]);
        assert_eq!(std::fs::read(&outputs[1]).unwrap(), source_bytes[1]);
    }

    #[test]
    fn deskew_apply_stage_uses_measured_evidence_and_publishes_rgba8() {
        for (index, angle) in [-2.0_f64, 2.0].into_iter().enumerate() {
            let temp = tempdir().unwrap();
            let input = temp.path().join("page.png");
            synthetic_page_edge(angle).save(&input).unwrap();
            let pipeline = PdfPipeline::new(PipelineConfig {
                deskew_action: GeometryAction::Apply,
                deskew_action_configured: true,
                ..PipelineConfig::default()
            });
            let proposal = pipeline.analyze_deskew(index, &input, false).unwrap();
            assert!(proposal.should_apply);
            assert!((proposal.detection.as_ref().unwrap().angle - angle).abs() < 0.25);
            assert_eq!(proposal.transform.decision, TransformDecision::Proposed);
            let outputs = pipeline
                .step_deskew(temp.path(), &[input], &[false], &SilentProgress)
                .unwrap();
            let output = image::open(&outputs[0]).unwrap();
            assert_eq!(output.color(), image::ColorType::Rgba8);
            assert!(output.width() > 320);
            assert!(output.height() > 480);
            let corrected = pipeline.analyze_deskew(index, &outputs[0], false).unwrap();
            let corrected_angle = corrected.detection.as_ref().unwrap().angle.abs();
            assert!(
                corrected_angle < angle.abs(),
                "deskew must reduce the measured angle: input={angle}, output={corrected_angle}"
            );
        }
    }

    #[test]
    fn deskew_stage_propagates_cardinality_analysis_copy_and_transform_errors() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("page.png");
        synthetic_page_edge(2.0).save(&input).unwrap();
        let report = PdfPipeline::new(PipelineConfig {
            deskew_action: GeometryAction::Report,
            deskew_action_configured: true,
            ..PipelineConfig::default()
        });
        assert!(report
            .step_deskew(
                temp.path(),
                std::slice::from_ref(&input),
                &[],
                &SilentProgress
            )
            .is_err());

        let invalid_temp = tempdir().unwrap();
        let invalid = invalid_temp.path().join("invalid.png");
        std::fs::write(&invalid, b"not an image").unwrap();
        assert!(report
            .step_deskew(invalid_temp.path(), &[invalid], &[false], &SilentProgress)
            .is_err());

        let copy_temp = tempdir().unwrap();
        let copy_input = copy_temp.path().join("page.png");
        synthetic_page_edge(2.0).save(&copy_input).unwrap();
        std::fs::create_dir_all(copy_temp.path().join("deskewed/page.png")).unwrap();
        assert!(report
            .step_deskew(copy_temp.path(), &[copy_input], &[false], &SilentProgress)
            .is_err());

        let apply_temp = tempdir().unwrap();
        let apply_input = apply_temp.path().join("page.png");
        synthetic_page_edge(2.0).save(&apply_input).unwrap();
        std::fs::create_dir_all(apply_temp.path().join("deskewed/page.png")).unwrap();
        let apply = PdfPipeline::new(PipelineConfig {
            deskew_action: GeometryAction::Apply,
            deskew_action_configured: true,
            ..PipelineConfig::default()
        });
        assert!(apply
            .step_deskew(apply_temp.path(), &[apply_input], &[false], &SilentProgress)
            .is_err());
    }

    #[test]
    fn rotation_policy_report_never_applies_an_approved_proposal() {
        let evidence = rotation_evidence(180, 0.8, 0.95, RotationReason::UpsideDownEvidence, false);
        let outcome = RotationPolicyOutcome::from_evidence(
            7,
            &evidence,
            GeometryAction::Report,
            RotationConfidenceThreshold::default(),
        )
        .unwrap();

        assert_eq!(outcome.page_index, 7);

        assert_eq!(outcome.transform.decision, TransformDecision::Proposed);
        assert!(!outcome.should_apply);
        assert!(outcome.review_required);
    }

    #[test]
    fn rotation_policy_apply_requires_confidence_and_no_ambiguity() {
        let threshold = RotationConfidenceThreshold::new(0.90).unwrap();
        let approved = rotation_evidence(180, 0.8, 0.95, RotationReason::UpsideDownEvidence, false);
        let low_confidence =
            rotation_evidence(180, 0.8, 0.89, RotationReason::UpsideDownEvidence, false);
        let ambiguous = rotation_evidence(180, 0.8, 0.99, RotationReason::BandDisagreement, true);

        let mut applied =
            RotationPolicyOutcome::from_evidence(3, &approved, GeometryAction::Apply, threshold)
                .unwrap();
        assert_eq!(applied.transform.decision, TransformDecision::Proposed);
        assert!(applied.should_apply);
        assert!(!applied.review_required);
        applied.mark_applied();
        assert_eq!(applied.transform.decision, TransformDecision::Applied);

        for evidence in [&low_confidence, &ambiguous] {
            let rejected =
                RotationPolicyOutcome::from_evidence(3, evidence, GeometryAction::Apply, threshold)
                    .unwrap();
            assert_eq!(rejected.transform.decision, TransformDecision::Rejected);
            assert!(!rejected.should_apply);
            assert!(rejected.review_required);
        }
    }

    #[test]
    fn disabled_rotation_does_not_read_the_image() {
        let pipeline = PdfPipeline::new(PipelineConfig {
            rotation_action: GeometryAction::Off,
            rotation_action_configured: true,
            ..PipelineConfig::default()
        });

        assert!(pipeline
            .analyze_rotation(0, Path::new("/path/that/does/not/exist.png"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn geometry_stage_gates_keep_rotation_and_deskew_independent() {
        let rotation_only = PdfPipeline::new(PipelineConfig::geometry_only(
            GeometryAction::Report,
            GeometryAction::Off,
        ));
        assert!(rotation_only.config().rotation_stage_enabled());
        assert!(!rotation_only.config().deskew_stage_enabled());

        let deskew_only = PdfPipeline::new(PipelineConfig::geometry_only(
            GeometryAction::Off,
            GeometryAction::Report,
        ));
        assert!(!deskew_only.config().rotation_stage_enabled());
        assert!(deskew_only.config().deskew_stage_enabled());
    }

    #[test]
    fn rotation_report_stage_preserves_encoded_bytes_and_input_order() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        std::fs::create_dir(&source_dir).unwrap();
        let first = source_dir.join("z-page.png");
        let second = source_dir.join("a-page.png");
        let upright = synthetic_rotation_page();
        upright.save(&first).unwrap();
        crate::ImageProcDeskewer::rotate_180_exact(&upright)
            .save(&second)
            .unwrap();
        let source_bytes = [
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap(),
        ];

        let outputs = rotation_pipeline(GeometryAction::Report)
            .step_rotation_detect(
                temp.path(),
                &[first.clone(), second.clone()],
                &SilentProgress,
            )
            .unwrap();

        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].file_name(), first.file_name());
        assert_eq!(outputs[1].file_name(), second.file_name());
        assert_eq!(std::fs::read(&outputs[0]).unwrap(), source_bytes[0]);
        assert_eq!(std::fs::read(&outputs[1]).unwrap(), source_bytes[1]);
    }

    #[test]
    fn rotation_apply_stage_publishes_exact_half_turn() {
        let temp = tempdir().unwrap();
        let input = temp.path().join("upside-down.png");
        let upright = synthetic_rotation_page();
        crate::ImageProcDeskewer::rotate_180_exact(&upright)
            .save(&input)
            .unwrap();
        let pipeline = rotation_pipeline(GeometryAction::Apply);
        let proposal = pipeline.analyze_rotation(0, &input).unwrap().unwrap();
        assert!(proposal.should_apply);
        assert_eq!(proposal.transform.decision, TransformDecision::Proposed);

        let outputs = pipeline
            .step_rotation_detect(temp.path(), &[input], &SilentProgress)
            .unwrap();
        let output = image::open(&outputs[0]).unwrap();

        assert_eq!(output.color(), upright.color());
        assert_eq!(output.dimensions(), upright.dimensions());
        assert_eq!(output.as_bytes(), upright.as_bytes());
    }

    #[test]
    fn rotation_stage_propagates_analysis_copy_and_transform_errors() {
        let analysis_temp = tempdir().unwrap();
        let invalid = analysis_temp.path().join("invalid.png");
        std::fs::write(&invalid, b"not an image").unwrap();
        assert!(rotation_pipeline(GeometryAction::Report)
            .step_rotation_detect(analysis_temp.path(), &[invalid], &SilentProgress)
            .is_err());

        let copy_temp = tempdir().unwrap();
        let copy_input = copy_temp.path().join("upright.png");
        synthetic_rotation_page().save(&copy_input).unwrap();
        let copy_output = copy_temp.path().join("rotation_corrected/upright.png");
        std::fs::create_dir_all(&copy_output).unwrap();
        assert!(rotation_pipeline(GeometryAction::Report)
            .step_rotation_detect(copy_temp.path(), &[copy_input], &SilentProgress)
            .is_err());

        let transform_temp = tempdir().unwrap();
        let transform_input = transform_temp.path().join("upside-down.png");
        crate::ImageProcDeskewer::rotate_180_exact(&synthetic_rotation_page())
            .save(&transform_input)
            .unwrap();
        let transform_pipeline = rotation_pipeline(GeometryAction::Apply);
        assert!(
            transform_pipeline
                .analyze_rotation(0, &transform_input)
                .unwrap()
                .unwrap()
                .should_apply
        );
        let transform_output = transform_temp
            .path()
            .join("rotation_corrected/upside-down.png");
        std::fs::create_dir_all(&transform_output).unwrap();
        assert!(transform_pipeline
            .step_rotation_detect(transform_temp.path(), &[transform_input], &SilentProgress)
            .is_err());
    }

    #[test]
    fn test_geometry_only_pipeline_execution_is_unsupported_before_input_validation() {
        let config = PipelineConfig {
            geometry_only: true,
            ..PipelineConfig::default()
        };
        let pipeline = PdfPipeline::new(config);

        assert!(!pipeline.config().upscale);
        assert!(!pipeline.config().gpu);
        assert_eq!(pipeline.config().margin_trim, 0.0);
        assert!(!pipeline.config().color_correction);
        assert_eq!(pipeline.config().output_height, 0);
        assert_eq!(pipeline.config().rotation_action, GeometryAction::Report);
        assert_eq!(pipeline.config().deskew_action, GeometryAction::Report);

        let result = pipeline.process(Path::new("/nonexistent/file.pdf"), Path::new("/output"));

        assert!(matches!(result, Err(PipelineError::UnsupportedMode(_))));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not yet implemented"));
    }

    #[test]
    fn test_deserialized_geometry_only_config_without_actions_defaults_to_report() {
        let mut value = serde_json::to_value(PipelineConfig::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("geometry_only".to_string(), serde_json::Value::Bool(true));
        object.remove("rotation_action");
        object.remove("deskew_action");

        let config: PipelineConfig = serde_json::from_value(value).unwrap();
        let pipeline = PdfPipeline::new(config);

        assert_eq!(pipeline.config().rotation_action, GeometryAction::Report);
        assert_eq!(pipeline.config().deskew_action, GeometryAction::Report);
    }

    #[test]
    fn test_pipeline_config_to_json() {
        // TC: PIPE-002
        let config = PipelineConfig::default();
        let json = config.to_json();

        assert!(json.contains("\"dpi\":300"));
        assert!(json.contains("\"deskew\":true"));
        assert!(json.contains("\"upscale\":true"));
        assert!(json.contains("\"geometry_only\":false"));
        assert!(json.contains("\"rotation_action\":\"apply\""));
        assert!(json.contains("\"deskew_action\":\"apply\""));
    }

    #[test]
    fn test_pipeline_config_builder() {
        let config = PipelineConfig::default()
            .with_dpi(600)
            .with_deskew(false)
            .with_upscale(false)
            .with_ocr(true);

        assert_eq!(config.dpi, 600);
        assert!(!config.deskew);
        assert!(!config.upscale);
        assert!(config.ocr);
    }

    #[test]
    fn test_pipeline_config_with_advanced() {
        let config = PipelineConfig::default().with_advanced();

        assert!(config.internal_resolution);
        assert!(config.color_correction);
        assert!(config.offset_alignment);
    }

    #[test]
    fn test_pipeline_config_with_max_pages() {
        let config = PipelineConfig::default().with_max_pages(Some(10));

        assert_eq!(config.max_pages, Some(10));
    }

    #[test]
    fn test_pipeline_config_with_margin_trim() {
        let config = PipelineConfig::default().with_margin_trim(1.0);

        assert_eq!(config.margin_trim, 1.0);
    }

    #[test]
    fn test_pipeline_config_with_gpu() {
        let config = PipelineConfig::default().with_gpu(false);

        assert!(!config.gpu);
    }

    // ============ PipelineResult Tests ============

    #[test]
    fn test_pipeline_result_new() {
        // TC: PIPE-004
        let result = PipelineResult::new(
            100,
            Some(2),
            true,
            45.5,
            PathBuf::from("/output/file.pdf"),
            12345678,
            Some(PathBuf::from("/output/file.transforms.jsonl")),
        );

        assert_eq!(result.page_count, 100);
        assert_eq!(result.page_number_shift, Some(2));
        assert!(result.is_vertical);
        assert_eq!(result.elapsed_seconds, 45.5);
        assert_eq!(result.output_path, PathBuf::from("/output/file.pdf"));
        assert_eq!(result.output_size, 12345678);
        assert_eq!(
            result.transform_manifest_path,
            Some(PathBuf::from("/output/file.transforms.jsonl"))
        );
    }

    #[test]
    fn test_pipeline_result_no_shift() {
        let result = PipelineResult::new(
            50,
            None,
            false,
            10.0,
            PathBuf::from("/output/test.pdf"),
            5000000,
            None,
        );

        assert_eq!(result.page_count, 50);
        assert!(result.page_number_shift.is_none());
        assert!(!result.is_vertical);
        assert!(result.transform_manifest_path.is_none());
    }

    #[test]
    fn test_pipeline_result_to_cache() {
        let result = PipelineResult::new(
            100,
            Some(2),
            true,
            45.5,
            PathBuf::from("/out.pdf"),
            1000,
            None,
        );

        let cache_result = result.to_cache_result();

        assert_eq!(cache_result.page_count, 100);
        assert_eq!(cache_result.page_number_shift, Some(2));
        assert!(cache_result.is_vertical);
        assert_eq!(cache_result.elapsed_seconds, 45.5);
        assert_eq!(cache_result.output_size, 1000);
    }

    // ============ PdfPipeline Tests ============

    #[test]
    fn test_pdf_pipeline_new() {
        // TC: PIPE-005
        let config = PipelineConfig::default();
        let pipeline = PdfPipeline::new(config);

        assert_eq!(pipeline.config().dpi, 300);
    }

    #[test]
    fn test_pdf_pipeline_get_output_path() {
        let config = PipelineConfig::default();
        let pipeline = PdfPipeline::new(config);

        let input = Path::new("/input/document.pdf");
        let output_dir = Path::new("/output");

        let output_path = pipeline.get_output_path(input, output_dir);

        assert_eq!(output_path, PathBuf::from("/output/document_converted.pdf"));
    }

    #[test]
    fn test_pdf_pipeline_get_transform_manifest_path() {
        let pipeline = PdfPipeline::new(PipelineConfig::default());
        let input = Path::new("/input/document.pdf");
        let output_dir = Path::new("/output");

        let manifest_path = pipeline.get_transform_manifest_path(input, output_dir);

        assert_eq!(
            manifest_path,
            PathBuf::from("/output/document.transforms.jsonl")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_pdf_pipeline_get_transform_manifest_path_preserves_non_utf8_stem() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let pipeline = PdfPipeline::new(PipelineConfig::default());
        let input = PathBuf::from(std::ffi::OsString::from_vec(b"book\xff.pdf".to_vec()));
        let manifest_path = pipeline.get_transform_manifest_path(&input, Path::new("output"));

        let mut expected = b"book\xff".to_vec();
        expected.extend_from_slice(b".transforms.jsonl");
        assert_eq!(manifest_path.file_name().unwrap().as_bytes(), expected);
    }

    #[test]
    fn test_pdf_pipeline_get_output_path_no_extension() {
        let config = PipelineConfig::default();
        let pipeline = PdfPipeline::new(config);

        let input = Path::new("/input/document");
        let output_dir = Path::new("/output");

        let output_path = pipeline.get_output_path(input, output_dir);

        assert_eq!(output_path, PathBuf::from("/output/document_converted.pdf"));
    }

    #[test]
    fn test_pdf_pipeline_process_input_not_found() {
        let config = PipelineConfig::default();
        let pipeline = PdfPipeline::new(config);

        let result = pipeline.process(Path::new("/nonexistent/file.pdf"), Path::new("/output"));

        assert!(matches!(result, Err(PipelineError::InputNotFound(_))));
    }

    // ============ PipelineError Tests ============

    #[test]
    fn test_pipeline_error_display() {
        let err = PipelineError::InputNotFound(PathBuf::from("/test.pdf"));
        assert!(err.to_string().contains("/test.pdf"));

        let err = PipelineError::ExtractionFailed("test error".to_string());
        assert!(err.to_string().contains("test error"));
    }

    #[test]
    fn test_pipeline_error_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: PipelineError = io_err.into();

        assert!(matches!(err, PipelineError::Io(_)));
    }

    #[test]
    fn test_pipeline_error_variants() {
        let errors = vec![
            PipelineError::InputNotFound(PathBuf::from("/test.pdf")),
            PipelineError::OutputNotWritable(PathBuf::from("/out")),
            PipelineError::ExtractionFailed("test".to_string()),
            PipelineError::ImageProcessingFailed("test".to_string()),
            PipelineError::PdfGenerationFailed("test".to_string()),
        ];

        for err in errors {
            assert!(!err.to_string().is_empty());
        }
    }

    // ============ Memory Management Tests (Phase 3) ============

    #[test]
    fn test_calculate_optimal_chunk_size_basic() {
        // With 4GB memory limit and 100MB per image, should allow ~20 concurrent
        let chunk = calculate_optimal_chunk_size(100, 4096, 8);
        assert!(chunk >= MIN_CHUNK_SIZE);
        assert!(chunk <= 100);
    }

    #[test]
    fn test_calculate_optimal_chunk_size_small_batch() {
        // When total items is small, chunk size should not exceed it
        let chunk = calculate_optimal_chunk_size(3, 4096, 8);
        assert!(chunk >= 1);
        assert!(chunk <= 3);
    }

    #[test]
    fn test_calculate_optimal_chunk_size_zero_items() {
        let chunk = calculate_optimal_chunk_size(0, 4096, 8);
        assert_eq!(chunk, 1); // At least 1 to prevent division by zero
    }

    #[test]
    fn test_calculate_optimal_chunk_size_limited_memory() {
        // With very limited memory, chunk size should be small
        let chunk = calculate_optimal_chunk_size(100, 200, 8);
        assert!(chunk >= MIN_CHUNK_SIZE);
        // With only 200MB, usable = 100MB, capacity = 1, so MIN_CHUNK_SIZE
    }

    #[test]
    fn test_calculate_optimal_chunk_size_auto_memory() {
        // 0 memory means auto-detect
        let chunk = calculate_optimal_chunk_size(100, 0, 8);
        assert!(chunk >= 1);
    }

    #[test]
    fn test_process_in_chunks_empty() {
        let items: Vec<i32> = vec![];
        let results: Vec<i32> = process_in_chunks(&items, 4, |x| *x * 2, None::<&fn(usize, usize)>);
        assert!(results.is_empty());
    }

    #[test]
    fn test_process_in_chunks_single_item() {
        let items = vec![5];
        let results: Vec<i32> = process_in_chunks(&items, 4, |x| *x * 2, None::<&fn(usize, usize)>);
        assert_eq!(results, vec![10]);
    }

    #[test]
    fn test_process_in_chunks_maintains_order() {
        let items: Vec<i32> = (0..20).collect();
        let results: Vec<i32> = process_in_chunks(&items, 4, |x| *x * 2, None::<&fn(usize, usize)>);

        let expected: Vec<i32> = (0..20).map(|x| x * 2).collect();
        assert_eq!(results, expected);
    }

    #[test]
    fn test_process_in_chunks_with_progress() {
        use std::sync::atomic::AtomicUsize;

        let items: Vec<i32> = (0..10).collect();
        let progress_count = Arc::new(AtomicUsize::new(0));
        let progress_count_clone = progress_count.clone();

        let progress_fn = move |_current: usize, _total: usize| {
            progress_count_clone.fetch_add(1, Ordering::Relaxed);
        };

        let results: Vec<i32> = process_in_chunks(&items, 4, |x| *x * 2, Some(&progress_fn));

        assert_eq!(results.len(), 10);
        assert_eq!(progress_count.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_process_in_chunks_chunk_size_zero() {
        // chunk_size 0 means process all at once
        let items: Vec<i32> = (0..10).collect();
        let results: Vec<i32> = process_in_chunks(&items, 0, |x| *x + 1, None::<&fn(usize, usize)>);

        let expected: Vec<i32> = (1..11).collect();
        assert_eq!(results, expected);
    }

    #[test]
    fn test_pipeline_config_memory_fields() {
        let config = PipelineConfig::default();
        assert_eq!(config.max_memory_mb, 0);
        assert_eq!(config.chunk_size, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_get_available_memory_linux() {
        let mem = get_available_memory_mb();
        // On Linux, this should return Some value
        assert!(mem.is_some());
        assert!(mem.unwrap() > 0);
    }

    #[test]
    fn native_source_inspection_preserves_physical_page_count_without_workspace() {
        use lopdf::{dictionary, Document, Object};

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("native-inspection.pdf");
        let mut document = Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let first = document.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
        });
        let second = document.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 20.into(), 20.into()]),
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![first.into(), second.into()]),
                "Count" => Object::Integer(2),
            }),
        );
        let catalog = document.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        document.trailer.set("Root", catalog);
        document.save(&source).unwrap();

        let pipeline = PdfPipeline::new(PipelineConfig::default());
        let native = pipeline.inspect_native_source(&source).unwrap();
        assert_eq!(native.pages().len(), 2);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
