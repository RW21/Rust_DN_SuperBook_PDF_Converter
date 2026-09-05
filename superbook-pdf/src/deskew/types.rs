//! Deskew module core types
//!
//! Contains basic data structures for skew detection and correction.

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

// ============================================================
// Constants
// ============================================================

/// Default maximum angle for deskew detection (degrees)
pub const DEFAULT_MAX_ANGLE: f64 = 15.0;

/// Default threshold angle - angles below this are not corrected (degrees)
pub const DEFAULT_THRESHOLD_ANGLE: f64 = 0.1;

/// Default background color (white) for filled areas after rotation
pub const DEFAULT_BACKGROUND_COLOR: [u8; 3] = [255, 255, 255];

/// Grayscale threshold for binarization in projection analysis
pub const GRAYSCALE_THRESHOLD: u8 = 128;

/// White pixel value for image processing
pub const WHITE_PIXEL: u8 = 255;

/// Fully opaque alpha value for RGBA images
pub const ALPHA_OPAQUE: u8 = 255;

/// Symmetric border excluded from rotation analysis.
pub const DEFAULT_ROTATION_BORDER_CROP_FRACTION: f64 = 0.03;
/// Minimum accepted width and height for rotation analysis.
pub const DEFAULT_ROTATION_MINIMUM_DIMENSION: u32 = 64;
/// Minimum robust grayscale contrast.
pub const DEFAULT_ROTATION_MINIMUM_CONTRAST: u8 = 24;
/// Ink ratios below this value are treated as blank.
pub const DEFAULT_ROTATION_MINIMUM_INK_RATIO: f64 = 0.0005;
/// Nonblank pages below this ink ratio are considered sparse.
pub const DEFAULT_ROTATION_SPARSE_INK_RATIO: f64 = 0.008;
/// Minimum number of glyph-like components.
pub const DEFAULT_ROTATION_MINIMUM_GLYPH_COMPONENTS: usize = 40;
/// Minimum number of horizontal text-like lines.
pub const DEFAULT_ROTATION_MINIMUM_TEXT_LINES: usize = 6;
/// Ink ratios at or above this value are illustration-like.
pub const DEFAULT_ROTATION_MAXIMUM_ILLUSTRATION_INK_RATIO: f64 = 0.20;
/// Maximum share of cleaned ink allowed in one component.
pub const DEFAULT_ROTATION_MAXIMUM_LARGEST_COMPONENT_SHARE: f64 = 0.25;
/// Maximum active-row fraction for pages without enough text lines.
pub const DEFAULT_ROTATION_MAXIMUM_DENSE_ROW_FRACTION: f64 = 0.75;
/// Minimum signed score required to approve a 180-degree proposal.
pub const DEFAULT_ROTATION_MINIMUM_APPLY_SCORE: f64 = 0.55;
/// Default minimum confidence required to approve a 180-degree proposal.
pub const DEFAULT_ROTATION_MINIMUM_APPLY_CONFIDENCE: f64 = 0.90;

/// Validated minimum confidence for automatic 180-degree rotation.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct RotationConfidenceThreshold(f64);

impl RotationConfidenceThreshold {
    pub fn new(value: f64) -> Result<Self> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(if value == 0.0 { 0.0 } else { value }))
        } else {
            Err(DeskewError::DetectionFailed(format!(
                "rotation minimum confidence must be finite and in 0.0..=1.0, got {value}"
            )))
        }
    }

    pub const fn get(self) -> f64 {
        self.0
    }
}

impl Default for RotationConfidenceThreshold {
    fn default() -> Self {
        Self(DEFAULT_ROTATION_MINIMUM_APPLY_CONFIDENCE)
    }
}

impl std::fmt::Display for RotationConfidenceThreshold {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for RotationConfidenceThreshold {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let parsed = value
            .parse::<f64>()
            .map_err(|error| format!("invalid rotation minimum confidence: {error}"))?;
        Self::new(parsed).map_err(|error| error.to_string())
    }
}

impl Serialize for RotationConfidenceThreshold {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for RotationConfidenceThreshold {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

// ============================================================
// Error Types
// ============================================================

/// Deskew error types
#[derive(Debug, Error)]
pub enum DeskewError {
    #[error("Image not found: {0}")]
    ImageNotFound(PathBuf),

    #[error("Invalid image format: {0}")]
    InvalidFormat(String),

    #[error("Detection failed: {0}")]
    DetectionFailed(String),

    #[error("Correction failed: {0}")]
    CorrectionFailed(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DeskewError>;

// ============================================================
// Options and Enums
// ============================================================

/// Stable reason for a rotation proposal or abstention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationReason {
    Disabled,
    ImageTooSmall,
    BlankPage,
    CoverLike,
    IllustrationLike,
    NonHorizontalLayout,
    SparsePage,
    InsufficientBandSupport,
    BandDisagreement,
    InsufficientOrientationEvidence,
    UprightEvidence,
    UpsideDownEvidence,
    BelowScoreThreshold,
    BelowConfidenceThreshold,
}

impl RotationReason {
    /// Return the stable snake-case identifier used in reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::ImageTooSmall => "image_too_small",
            Self::BlankPage => "blank_page",
            Self::CoverLike => "cover_like",
            Self::IllustrationLike => "illustration_like",
            Self::NonHorizontalLayout => "non_horizontal_layout",
            Self::SparsePage => "sparse_page",
            Self::InsufficientBandSupport => "insufficient_band_support",
            Self::BandDisagreement => "band_disagreement",
            Self::InsufficientOrientationEvidence => "insufficient_orientation_evidence",
            Self::UprightEvidence => "upright_evidence",
            Self::UpsideDownEvidence => "upside_down_evidence",
            Self::BelowScoreThreshold => "below_score_threshold",
            Self::BelowConfidenceThreshold => "below_confidence_threshold",
        }
    }
}

impl std::fmt::Display for RotationReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Conservative settings for deterministic 0/180-degree analysis.
#[derive(Debug, Clone, PartialEq)]
pub struct RotationAnalysisOptions {
    border_crop_fraction: f64,
    minimum_dimension: u32,
    minimum_contrast: u8,
    minimum_ink_ratio: f64,
    sparse_ink_ratio: f64,
    minimum_glyph_components: usize,
    minimum_text_lines: usize,
    maximum_illustration_ink_ratio: f64,
    maximum_largest_component_share: f64,
    maximum_dense_row_fraction: f64,
    minimum_apply_confidence: f64,
}

impl Default for RotationAnalysisOptions {
    fn default() -> Self {
        Self {
            border_crop_fraction: DEFAULT_ROTATION_BORDER_CROP_FRACTION,
            minimum_dimension: DEFAULT_ROTATION_MINIMUM_DIMENSION,
            minimum_contrast: DEFAULT_ROTATION_MINIMUM_CONTRAST,
            minimum_ink_ratio: DEFAULT_ROTATION_MINIMUM_INK_RATIO,
            sparse_ink_ratio: DEFAULT_ROTATION_SPARSE_INK_RATIO,
            minimum_glyph_components: DEFAULT_ROTATION_MINIMUM_GLYPH_COMPONENTS,
            minimum_text_lines: DEFAULT_ROTATION_MINIMUM_TEXT_LINES,
            maximum_illustration_ink_ratio: DEFAULT_ROTATION_MAXIMUM_ILLUSTRATION_INK_RATIO,
            maximum_largest_component_share: DEFAULT_ROTATION_MAXIMUM_LARGEST_COMPONENT_SHARE,
            maximum_dense_row_fraction: DEFAULT_ROTATION_MAXIMUM_DENSE_ROW_FRACTION,
            minimum_apply_confidence: DEFAULT_ROTATION_MINIMUM_APPLY_CONFIDENCE,
        }
    }
}

impl RotationAnalysisOptions {
    pub fn builder() -> RotationAnalysisOptionsBuilder {
        RotationAnalysisOptionsBuilder::default()
    }

    /// Validate all thresholds before image analysis.
    pub fn validate(&self) -> Result<()> {
        fn unit(name: &str, value: f64) -> Result<()> {
            if value.is_finite() && (0.0..=1.0).contains(&value) {
                Ok(())
            } else {
                Err(DeskewError::DetectionFailed(format!(
                    "{name} must be finite and in 0.0..=1.0"
                )))
            }
        }

        unit("border_crop_fraction", self.border_crop_fraction)?;
        if self.border_crop_fraction >= 0.5 {
            return Err(DeskewError::DetectionFailed(
                "border_crop_fraction must be less than 0.5".to_string(),
            ));
        }
        unit("minimum_ink_ratio", self.minimum_ink_ratio)?;
        unit("sparse_ink_ratio", self.sparse_ink_ratio)?;
        unit(
            "maximum_illustration_ink_ratio",
            self.maximum_illustration_ink_ratio,
        )?;
        unit(
            "maximum_largest_component_share",
            self.maximum_largest_component_share,
        )?;
        unit(
            "maximum_dense_row_fraction",
            self.maximum_dense_row_fraction,
        )?;
        unit("minimum_apply_confidence", self.minimum_apply_confidence)?;
        if self.minimum_dimension == 0
            || self.minimum_contrast == 0
            || self.minimum_glyph_components == 0
            || self.minimum_text_lines == 0
        {
            return Err(DeskewError::DetectionFailed(
                "rotation count and contrast minimums must be nonzero".to_string(),
            ));
        }
        if !(self.minimum_ink_ratio <= self.sparse_ink_ratio
            && self.sparse_ink_ratio <= self.maximum_illustration_ink_ratio)
        {
            return Err(DeskewError::DetectionFailed(
                "rotation ink-ratio thresholds must be ordered".to_string(),
            ));
        }
        Ok(())
    }

    pub const fn border_crop_fraction(&self) -> f64 {
        self.border_crop_fraction
    }
    pub const fn minimum_dimension(&self) -> u32 {
        self.minimum_dimension
    }
    pub const fn minimum_contrast(&self) -> u8 {
        self.minimum_contrast
    }
    pub const fn minimum_ink_ratio(&self) -> f64 {
        self.minimum_ink_ratio
    }
    pub const fn sparse_ink_ratio(&self) -> f64 {
        self.sparse_ink_ratio
    }
    pub const fn minimum_glyph_components(&self) -> usize {
        self.minimum_glyph_components
    }
    pub const fn minimum_text_lines(&self) -> usize {
        self.minimum_text_lines
    }
    pub const fn maximum_illustration_ink_ratio(&self) -> f64 {
        self.maximum_illustration_ink_ratio
    }
    pub const fn maximum_largest_component_share(&self) -> f64 {
        self.maximum_largest_component_share
    }
    pub const fn maximum_dense_row_fraction(&self) -> f64 {
        self.maximum_dense_row_fraction
    }
    pub const fn minimum_apply_confidence(&self) -> f64 {
        self.minimum_apply_confidence
    }
}

/// Builder that validates rotation options at `build` time.
#[derive(Debug, Clone, Default)]
pub struct RotationAnalysisOptionsBuilder {
    options: RotationAnalysisOptions,
}

macro_rules! rotation_option_setter {
    ($name:ident, $field:ident, $type:ty) => {
        #[must_use]
        pub fn $name(mut self, value: $type) -> Self {
            self.options.$field = value;
            self
        }
    };
}

impl RotationAnalysisOptionsBuilder {
    rotation_option_setter!(border_crop_fraction, border_crop_fraction, f64);
    rotation_option_setter!(minimum_dimension, minimum_dimension, u32);
    rotation_option_setter!(minimum_contrast, minimum_contrast, u8);
    rotation_option_setter!(minimum_ink_ratio, minimum_ink_ratio, f64);
    rotation_option_setter!(sparse_ink_ratio, sparse_ink_ratio, f64);
    rotation_option_setter!(minimum_glyph_components, minimum_glyph_components, usize);
    rotation_option_setter!(minimum_text_lines, minimum_text_lines, usize);
    rotation_option_setter!(
        maximum_illustration_ink_ratio,
        maximum_illustration_ink_ratio,
        f64
    );
    rotation_option_setter!(
        maximum_largest_component_share,
        maximum_largest_component_share,
        f64
    );
    rotation_option_setter!(maximum_dense_row_fraction, maximum_dense_row_fraction, f64);
    rotation_option_setter!(minimum_apply_confidence, minimum_apply_confidence, f64);

    pub fn build(self) -> Result<RotationAnalysisOptions> {
        self.options.validate()?;
        Ok(self.options)
    }
}

/// Deterministic diagnostic measurements supporting rotation evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct RotationMetrics {
    otsu_threshold: u8,
    robust_contrast: u8,
    cleaned_ink_ratio: f64,
    glyph_component_count: usize,
    text_line_count: usize,
    largest_component_share: f64,
    active_row_fraction: f64,
    outer_frame_ink_density: f64,
    broad_band_difference: Option<f64>,
    outer_band_difference: Option<f64>,
}

impl Default for RotationMetrics {
    fn default() -> Self {
        Self {
            otsu_threshold: 0,
            robust_contrast: 0,
            cleaned_ink_ratio: 0.0,
            glyph_component_count: 0,
            text_line_count: 0,
            largest_component_share: 0.0,
            active_row_fraction: 0.0,
            outer_frame_ink_density: 0.0,
            broad_band_difference: None,
            outer_band_difference: None,
        }
    }
}

impl RotationMetrics {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        otsu_threshold: u8,
        robust_contrast: u8,
        cleaned_ink_ratio: f64,
        glyph_component_count: usize,
        text_line_count: usize,
        largest_component_share: f64,
        active_row_fraction: f64,
        outer_frame_ink_density: f64,
        broad_band_difference: Option<f64>,
        outer_band_difference: Option<f64>,
    ) -> Result<Self> {
        let metrics = Self {
            otsu_threshold,
            robust_contrast,
            cleaned_ink_ratio,
            glyph_component_count,
            text_line_count,
            largest_component_share,
            active_row_fraction,
            outer_frame_ink_density,
            broad_band_difference,
            outer_band_difference,
        };
        metrics.validate()?;
        Ok(metrics)
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("cleaned_ink_ratio", self.cleaned_ink_ratio),
            ("largest_component_share", self.largest_component_share),
            ("active_row_fraction", self.active_row_fraction),
            ("outer_frame_ink_density", self.outer_frame_ink_density),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(DeskewError::DetectionFailed(format!(
                    "rotation metric {name} must be finite and in 0.0..=1.0"
                )));
            }
        }
        for value in [self.broad_band_difference, self.outer_band_difference]
            .into_iter()
            .flatten()
        {
            if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
                return Err(DeskewError::DetectionFailed(
                    "rotation band differences must be finite and in -1.0..=1.0".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub const fn otsu_threshold(&self) -> u8 {
        self.otsu_threshold
    }
    pub const fn robust_contrast(&self) -> u8 {
        self.robust_contrast
    }
    pub const fn cleaned_ink_ratio(&self) -> f64 {
        self.cleaned_ink_ratio
    }
    pub const fn glyph_component_count(&self) -> usize {
        self.glyph_component_count
    }
    pub const fn text_line_count(&self) -> usize {
        self.text_line_count
    }
    pub const fn largest_component_share(&self) -> f64 {
        self.largest_component_share
    }
    pub const fn active_row_fraction(&self) -> f64 {
        self.active_row_fraction
    }
    pub const fn outer_frame_ink_density(&self) -> f64 {
        self.outer_frame_ink_density
    }
    pub const fn broad_band_difference(&self) -> Option<f64> {
        self.broad_band_difference
    }
    pub const fn outer_band_difference(&self) -> Option<f64> {
        self.outer_band_difference
    }
}

/// Validated rotation proposal, confidence, reason, and supporting diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct RotationEvidence {
    proposed_degrees: u16,
    score: f64,
    confidence: f64,
    reason: RotationReason,
    ambiguity_guard: bool,
    metrics: RotationMetrics,
}

impl RotationEvidence {
    pub fn try_new(
        proposed_degrees: u16,
        score: f64,
        confidence: f64,
        reason: RotationReason,
        ambiguity_guard: bool,
        metrics: RotationMetrics,
    ) -> Result<Self> {
        if !matches!(proposed_degrees, 0 | 180) {
            return Err(DeskewError::DetectionFailed(
                "rotation proposal must be 0 or 180 degrees".to_string(),
            ));
        }
        if !score.is_finite() || !(-1.0..=1.0).contains(&score) {
            return Err(DeskewError::DetectionFailed(
                "rotation score must be finite and in -1.0..=1.0".to_string(),
            ));
        }
        if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
            return Err(DeskewError::DetectionFailed(
                "rotation confidence must be finite and in 0.0..=1.0".to_string(),
            ));
        }
        metrics.validate()?;
        Ok(Self {
            proposed_degrees,
            score: if score == 0.0 { 0.0 } else { score },
            confidence: if confidence == 0.0 { 0.0 } else { confidence },
            reason,
            ambiguity_guard,
            metrics,
        })
    }

    pub const fn proposed_degrees(&self) -> u16 {
        self.proposed_degrees
    }
    pub const fn score(&self) -> f64 {
        self.score
    }
    pub const fn confidence(&self) -> f64 {
        self.confidence
    }
    pub const fn reason(&self) -> RotationReason {
        self.reason
    }
    pub const fn ambiguity_guard(&self) -> bool {
        self.ambiguity_guard
    }
    pub const fn metrics(&self) -> &RotationMetrics {
        &self.metrics
    }

    /// Return true only when the immutable score safety floor and caller's
    /// confidence threshold both pass for validated upside-down evidence.
    pub fn is_auto_applicable(&self, minimum_confidence: f64) -> bool {
        minimum_confidence.is_finite()
            && (0.0..=1.0).contains(&minimum_confidence)
            && self.proposed_degrees == 180
            && self.reason == RotationReason::UpsideDownEvidence
            && !self.ambiguity_guard
            && self.score >= DEFAULT_ROTATION_MINIMUM_APPLY_SCORE
            && self.confidence >= minimum_confidence
    }
}

/// Deskew detection algorithms
#[derive(Debug, Clone, Copy, Default)]
pub enum DeskewAlgorithm {
    /// Hough line transform
    #[default]
    HoughLines,
    /// Projection profile method
    ProjectionProfile,
    /// Text line detection
    TextLineDetection,
    /// Combined (average of multiple methods)
    Combined,
    /// Page edge detection (for scanned book pages)
    PageEdge,
}

/// Quality modes for rotation
#[derive(Debug, Clone, Copy, Default)]
pub enum QualityMode {
    /// Fast (bilinear interpolation)
    Fast,
    /// Standard (bicubic interpolation)
    #[default]
    Standard,
    /// High quality (Lanczos interpolation)
    HighQuality,
}

/// Deskew detection options
#[derive(Debug, Clone)]
pub struct DeskewOptions {
    /// Detection algorithm
    pub algorithm: DeskewAlgorithm,
    /// Maximum detection angle (degrees)
    pub max_angle: f64,
    /// Correction threshold (angles below this are ignored)
    pub threshold_angle: f64,
    /// Background color for filled areas after rotation
    pub background_color: [u8; 3],
    /// Quality mode for interpolation
    pub quality_mode: QualityMode,
}

impl Default for DeskewOptions {
    fn default() -> Self {
        Self {
            algorithm: DeskewAlgorithm::HoughLines,
            max_angle: DEFAULT_MAX_ANGLE,
            threshold_angle: DEFAULT_THRESHOLD_ANGLE,
            background_color: DEFAULT_BACKGROUND_COLOR,
            quality_mode: QualityMode::Standard,
        }
    }
}

impl DeskewOptions {
    /// Create a new options builder
    pub fn builder() -> DeskewOptionsBuilder {
        DeskewOptionsBuilder::default()
    }

    /// Create options optimized for high quality output
    pub fn high_quality() -> Self {
        Self {
            algorithm: DeskewAlgorithm::Combined,
            quality_mode: QualityMode::HighQuality,
            ..Default::default()
        }
    }

    /// Create options optimized for fast processing
    pub fn fast() -> Self {
        Self {
            algorithm: DeskewAlgorithm::ProjectionProfile,
            quality_mode: QualityMode::Fast,
            threshold_angle: 0.5, // Skip small corrections
            ..Default::default()
        }
    }
}

/// Builder for DeskewOptions
#[derive(Debug, Default)]
pub struct DeskewOptionsBuilder {
    options: DeskewOptions,
}

impl DeskewOptionsBuilder {
    /// Set the detection algorithm
    #[must_use]
    pub fn algorithm(mut self, algorithm: DeskewAlgorithm) -> Self {
        self.options.algorithm = algorithm;
        self
    }

    /// Set the maximum detection angle
    #[must_use]
    pub fn max_angle(mut self, angle: f64) -> Self {
        self.options.max_angle = angle.abs();
        self
    }

    /// Set the correction threshold angle
    #[must_use]
    pub fn threshold_angle(mut self, angle: f64) -> Self {
        self.options.threshold_angle = angle.abs();
        self
    }

    /// Set the background color for rotated areas
    #[must_use]
    pub fn background_color(mut self, color: [u8; 3]) -> Self {
        self.options.background_color = color;
        self
    }

    /// Set the quality mode
    #[must_use]
    pub fn quality_mode(mut self, mode: QualityMode) -> Self {
        self.options.quality_mode = mode;
        self
    }

    /// Build the options
    #[must_use]
    pub fn build(self) -> DeskewOptions {
        self.options
    }
}

// ============================================================
// Result Types
// ============================================================

/// Skew detection result
#[derive(Debug, Clone)]
pub struct SkewDetection {
    /// Detected angle in degrees (positive = clockwise)
    pub angle: f64,
    /// Detection confidence (0.0 - 1.0)
    pub confidence: f64,
    /// Number of features used for detection
    pub feature_count: usize,
}

/// Deskew operation result
#[derive(Debug)]
pub struct DeskewResult {
    /// Original detection result
    pub detection: SkewDetection,
    /// Whether correction was applied
    pub corrected: bool,
    /// Output image path
    pub output_path: PathBuf,
    /// Original image size
    pub original_size: (u32, u32),
    /// Corrected image size
    pub corrected_size: (u32, u32),
}

// ============================================================
// Deskewer Trait
// ============================================================

/// Deskewer trait
pub trait Deskewer {
    /// Detect skew angle
    fn detect_skew(image_path: &Path, options: &DeskewOptions) -> Result<SkewDetection>;

    /// Correct skew
    fn correct_skew(
        input_path: &Path,
        output_path: &Path,
        options: &DeskewOptions,
    ) -> Result<DeskewResult>;

    /// Detect and correct in one operation
    fn deskew(
        input_path: &Path,
        output_path: &Path,
        options: &DeskewOptions,
    ) -> Result<DeskewResult>;

    /// Batch processing
    fn deskew_batch(
        images: &[(PathBuf, PathBuf)],
        options: &DeskewOptions,
    ) -> Vec<Result<DeskewResult>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deskew_options_default() {
        let opts = DeskewOptions::default();
        assert_eq!(opts.max_angle, 15.0);
        assert_eq!(opts.threshold_angle, 0.1);
        assert_eq!(opts.background_color, [255, 255, 255]);
        assert!(matches!(opts.algorithm, DeskewAlgorithm::HoughLines));
        assert!(matches!(opts.quality_mode, QualityMode::Standard));
    }

    #[test]
    fn test_deskew_options_high_quality() {
        let opts = DeskewOptions::high_quality();
        assert!(matches!(opts.algorithm, DeskewAlgorithm::Combined));
        assert!(matches!(opts.quality_mode, QualityMode::HighQuality));
    }

    #[test]
    fn test_deskew_options_fast() {
        let opts = DeskewOptions::fast();
        assert!(matches!(opts.algorithm, DeskewAlgorithm::ProjectionProfile));
        assert!(matches!(opts.quality_mode, QualityMode::Fast));
        assert_eq!(opts.threshold_angle, 0.5);
    }

    #[test]
    fn test_deskew_options_builder() {
        let opts = DeskewOptions::builder()
            .algorithm(DeskewAlgorithm::TextLineDetection)
            .max_angle(20.0)
            .threshold_angle(0.3)
            .background_color([0, 0, 0])
            .quality_mode(QualityMode::HighQuality)
            .build();

        assert!(matches!(opts.algorithm, DeskewAlgorithm::TextLineDetection));
        assert_eq!(opts.max_angle, 20.0);
        assert_eq!(opts.threshold_angle, 0.3);
        assert_eq!(opts.background_color, [0, 0, 0]);
        assert!(matches!(opts.quality_mode, QualityMode::HighQuality));
    }

    #[test]
    fn test_builder_abs_angle() {
        let opts = DeskewOptions::builder().max_angle(-10.0).build();
        assert_eq!(opts.max_angle, 10.0);

        let opts = DeskewOptions::builder().threshold_angle(-0.5).build();
        assert_eq!(opts.threshold_angle, 0.5);
    }

    #[test]
    fn test_skew_detection() {
        let detection = SkewDetection {
            angle: 2.5,
            confidence: 0.95,
            feature_count: 150,
        };
        assert_eq!(detection.angle, 2.5);
        assert_eq!(detection.confidence, 0.95);
        assert_eq!(detection.feature_count, 150);
    }

    #[test]
    fn test_algorithm_variants() {
        let algorithms = [
            DeskewAlgorithm::HoughLines,
            DeskewAlgorithm::ProjectionProfile,
            DeskewAlgorithm::TextLineDetection,
            DeskewAlgorithm::Combined,
        ];
        for alg in algorithms {
            let _copy = alg;
        }
    }

    #[test]
    fn test_quality_mode_variants() {
        let modes = [
            QualityMode::Fast,
            QualityMode::Standard,
            QualityMode::HighQuality,
        ];
        for mode in modes {
            let _copy = mode;
        }
    }

    #[test]
    fn test_error_types() {
        let _err1 = DeskewError::ImageNotFound(PathBuf::from("/test"));
        let _err2 = DeskewError::InvalidFormat("bad".to_string());
        let _err3 = DeskewError::DetectionFailed("fail".to_string());
        let _err4 = DeskewError::CorrectionFailed("fail".to_string());
        let _err5: DeskewError = std::io::Error::other("test").into();
    }

    #[test]
    fn rotation_options_defaults_are_conservative_and_valid() {
        let options = RotationAnalysisOptions::default();
        options.validate().unwrap();
        assert_eq!(options.border_crop_fraction(), 0.03);
        assert_eq!(options.minimum_dimension(), 64);
        assert_eq!(options.minimum_apply_confidence(), 0.90);
    }

    #[test]
    fn rotation_confidence_threshold_is_validated_for_parse_and_serde() {
        let default = RotationConfidenceThreshold::default();
        assert_eq!(default.get(), 0.90);
        assert_eq!(
            "0".parse::<RotationConfidenceThreshold>().unwrap().get(),
            0.0
        );
        assert_eq!(
            "1".parse::<RotationConfidenceThreshold>().unwrap().get(),
            1.0
        );

        for value in ["NaN", "inf", "-0.1", "1.1"] {
            assert!(
                value.parse::<RotationConfidenceThreshold>().is_err(),
                "{value}"
            );
        }
        for json in ["-0.1", "1.1", "null", "\"0.9\""] {
            assert!(serde_json::from_str::<RotationConfidenceThreshold>(json).is_err());
        }
        let round_trip: RotationConfidenceThreshold =
            serde_json::from_str(&serde_json::to_string(&default).unwrap()).unwrap();
        assert_eq!(round_trip, default);
    }

    #[test]
    fn rotation_options_reject_invalid_values() {
        for value in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            assert!(RotationAnalysisOptions::builder()
                .minimum_apply_confidence(value)
                .build()
                .is_err());
        }
        assert!(RotationAnalysisOptions::builder()
            .border_crop_fraction(0.5)
            .build()
            .is_err());
        assert!(RotationAnalysisOptions::builder()
            .minimum_dimension(0)
            .build()
            .is_err());
        assert!(RotationAnalysisOptions::builder()
            .minimum_glyph_components(0)
            .build()
            .is_err());
    }

    #[test]
    fn rotation_evidence_rejects_invalid_numbers_and_angles() {
        let metrics = RotationMetrics::default();
        assert!(RotationEvidence::try_new(
            90,
            0.0,
            0.0,
            RotationReason::InsufficientOrientationEvidence,
            false,
            metrics.clone(),
        )
        .is_err());
        assert!(RotationEvidence::try_new(
            0,
            f64::NAN,
            0.0,
            RotationReason::BlankPage,
            false,
            metrics.clone(),
        )
        .is_err());
        assert!(RotationEvidence::try_new(
            180,
            0.5,
            1.01,
            RotationReason::UpsideDownEvidence,
            false,
            metrics,
        )
        .is_err());
    }

    #[test]
    fn rotation_policy_has_inclusive_boundaries_and_hard_veto() {
        let evidence = |score, confidence, guard| {
            RotationEvidence::try_new(
                180,
                score,
                confidence,
                RotationReason::UpsideDownEvidence,
                guard,
                RotationMetrics::default(),
            )
            .unwrap()
        };

        assert!(!evidence(0.55, 0.899_999, false).is_auto_applicable(0.90));
        assert!(evidence(0.55, 0.90, false).is_auto_applicable(0.90));
        assert!(!evidence(0.549_999, 1.0, false).is_auto_applicable(0.90));
        assert!(evidence(0.55, 1.0, false).is_auto_applicable(0.90));
        assert!(!evidence(1.0, 1.0, true).is_auto_applicable(0.90));
        assert!(!evidence(1.0, 1.0, false).is_auto_applicable(f64::NAN));

        let contradictory = RotationEvidence::try_new(
            180,
            1.0,
            1.0,
            RotationReason::BlankPage,
            false,
            RotationMetrics::default(),
        )
        .unwrap();
        assert!(!contradictory.is_auto_applicable(0.0));
    }

    #[test]
    fn rotation_reasons_have_stable_snake_case_names() {
        assert_eq!(RotationReason::ImageTooSmall.as_str(), "image_too_small");
        assert_eq!(
            RotationReason::NonHorizontalLayout.as_str(),
            "non_horizontal_layout"
        );
        assert_eq!(
            RotationReason::UpsideDownEvidence.as_str(),
            "upside_down_evidence"
        );
        assert_eq!(
            RotationReason::BelowConfidenceThreshold.to_string(),
            "below_confidence_threshold"
        );
    }
}
