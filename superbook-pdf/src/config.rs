//! Configuration file support for superbook-pdf
//!
//! Supports TOML configuration files with the following search order:
//! 1. `--config <path>` - explicitly specified path
//! 2. `./superbook.toml` - current directory
//! 3. `~/.config/superbook-pdf/config.toml` - user config
//! 4. Default values
//!
//! # Example Configuration
//!
//! ```toml
//! [general]
//! dpi = 300
//! threads = 4
//!
//! [processing]
//! deskew = true
//! margin_trim = 0.5
//!
//! [advanced]
//! internal_resolution = true
//! color_correction = true
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::cli::{GeometryAction, GeometryModeAction};
use crate::{
    DeskewMaxAngle, DeskewMinConfidence, DeskewMinFeatures, DeskewNoopAngle, PipelineConfig,
    RotationConfidenceThreshold,
};

/// Configuration file errors
#[derive(Debug, Error)]
pub enum ConfigError {
    /// IO error reading config file
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// TOML parse error
    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    /// File not found
    #[error("Config file not found: {0}")]
    NotFound(PathBuf),
}

/// General configuration options
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GeneralConfig {
    /// Output DPI
    #[serde(default)]
    pub dpi: Option<u32>,

    /// Number of threads for parallel processing
    #[serde(default)]
    pub threads: Option<usize>,

    /// Verbosity level (0-2)
    #[serde(default)]
    pub verbose: Option<u8>,
}

/// Processing configuration options
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcessingConfig {
    /// Enable deskew correction
    #[serde(default)]
    pub deskew: Option<bool>,

    /// Use the preservation geometry-only pipeline
    #[serde(default)]
    pub geometry_only: Option<bool>,

    /// Common action for geometry operations
    #[serde(default)]
    pub geometry_action: Option<GeometryModeAction>,

    /// Override the common action for 180-degree rotation
    #[serde(default)]
    pub rotation_action: Option<GeometryAction>,

    /// Minimum confidence required to apply a proposed 180-degree rotation
    #[serde(default)]
    pub rotation_min_confidence: Option<RotationConfidenceThreshold>,

    /// Override the common action for deskew correction
    #[serde(default)]
    pub deskew_action: Option<GeometryAction>,

    /// Maximum absolute angle eligible for automatic deskew
    #[serde(default)]
    pub deskew_max_angle: Option<DeskewMaxAngle>,

    /// Minimum confidence required for automatic deskew
    #[serde(default)]
    pub deskew_min_confidence: Option<DeskewMinConfidence>,

    /// Minimum detector feature count required for automatic deskew
    #[serde(default)]
    pub deskew_min_features: Option<DeskewMinFeatures>,

    /// Absolute angle at or below which deskew is a no-op
    #[serde(default)]
    pub deskew_noop_angle: Option<DeskewNoopAngle>,

    /// Margin trim percentage
    #[serde(default)]
    pub margin_trim: Option<f64>,

    /// Enable AI upscaling
    #[serde(default)]
    pub upscale: Option<bool>,

    /// Enable GPU processing
    #[serde(default)]
    pub gpu: Option<bool>,

    // Issue #32: Content-aware margins
    /// Enable content-aware margin detection
    #[serde(default)]
    pub content_aware_margins: Option<bool>,

    /// Safety buffer percentage for margins (0.0-5.0)
    #[serde(default)]
    pub margin_safety: Option<f32>,

    /// Enable aggressive trimming
    #[serde(default)]
    pub aggressive_trim: Option<bool>,

    // Issue #33: Shadow removal
    /// Shadow removal mode (none, auto, left, right, both)
    #[serde(default)]
    pub shadow_removal: Option<String>,
}

/// Cleanup configuration (Issue #34-35)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CleanupConfig {
    /// Enable marker/highlighter removal
    #[serde(default)]
    pub marker_removal: Option<bool>,

    /// Highlighter colors to remove
    #[serde(default)]
    pub highlighter_colors: Option<Vec<String>>,

    /// Enable deblur processing
    #[serde(default)]
    pub deblur: Option<bool>,

    /// Deblur algorithm (unsharp_mask, nafnet, deblurgan_v2)
    #[serde(default)]
    pub deblur_algorithm: Option<String>,
}

/// Markdown conversion configuration (Issue #36)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MarkdownConfig {
    /// Extract images from PDF
    #[serde(default)]
    pub extract_images: Option<bool>,

    /// Detect and convert tables
    #[serde(default)]
    pub detect_tables: Option<bool>,

    /// Text direction (auto, horizontal, vertical)
    #[serde(default)]
    pub text_direction: Option<String>,

    /// Include page numbers in output
    #[serde(default)]
    pub include_page_numbers: Option<bool>,

    /// Generate metadata JSON
    #[serde(default)]
    pub generate_metadata: Option<bool>,

    /// Validation settings
    #[serde(default)]
    pub validation: Option<MarkdownValidationConfig>,
}

/// Markdown validation configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MarkdownValidationConfig {
    /// Enable validation
    #[serde(default)]
    pub enabled: Option<bool>,

    /// API provider (claude, openai, local)
    #[serde(default)]
    pub provider: Option<String>,
}

/// Advanced processing configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AdvancedConfig {
    /// Enable internal resolution normalization (4960x7016)
    #[serde(default)]
    pub internal_resolution: Option<bool>,

    /// Enable global color correction
    #[serde(default)]
    pub color_correction: Option<bool>,

    /// Enable page number offset alignment
    #[serde(default)]
    pub offset_alignment: Option<bool>,

    /// Output height in pixels
    #[serde(default)]
    pub output_height: Option<u32>,
}

/// OCR configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OcrConfig {
    /// Enable OCR
    #[serde(default)]
    pub enabled: Option<bool>,

    /// OCR language
    #[serde(default)]
    pub language: Option<String>,
}

/// Output configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutputConfig {
    /// JPEG quality (1-100)
    #[serde(default)]
    pub jpeg_quality: Option<u8>,

    /// Skip existing files
    #[serde(default)]
    pub skip_existing: Option<bool>,
}

/// Main configuration structure
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// General settings
    #[serde(default)]
    pub general: GeneralConfig,

    /// Processing settings
    #[serde(default)]
    pub processing: ProcessingConfig,

    /// Advanced settings
    #[serde(default)]
    pub advanced: AdvancedConfig,

    /// OCR settings
    #[serde(default)]
    pub ocr: OcrConfig,

    /// Output settings
    #[serde(default)]
    pub output: OutputConfig,

    /// Cleanup settings (Issue #34-35)
    #[serde(default)]
    pub cleanup: CleanupConfig,

    /// Markdown conversion settings (Issue #36)
    #[serde(default)]
    pub markdown: MarkdownConfig,
}

impl Config {
    /// Create a new default configuration
    pub fn new() -> Self {
        Self::default()
    }

    /// Load configuration from the default search path
    ///
    /// Search order:
    /// 1. `./superbook.toml`
    /// 2. `~/.config/superbook-pdf/config.toml`
    /// 3. Default values (if no file found)
    pub fn load() -> Result<Self, ConfigError> {
        // Try current directory first
        let current_dir_config = PathBuf::from("superbook.toml");
        if current_dir_config.exists() {
            return Self::load_from_path(&current_dir_config);
        }

        // Try user config directory
        if let Some(config_dir) = dirs::config_dir() {
            let user_config = config_dir.join("superbook-pdf").join("config.toml");
            if user_config.exists() {
                return Self::load_from_path(&user_config);
            }
        }

        // Return default config if no file found
        Ok(Self::default())
    }

    /// Load configuration from a specific file path
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Err(ConfigError::NotFound(path.to_path_buf()));
        }

        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Parse configuration from a TOML string
    pub fn from_toml(content: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(content)?;
        Ok(config)
    }

    /// Serialize configuration to TOML string
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Convert to PipelineConfig
    pub fn to_pipeline_config(&self) -> PipelineConfig {
        let mut config = PipelineConfig::default();

        // Apply general settings
        if let Some(dpi) = self.general.dpi {
            config = config.with_dpi(dpi);
        }
        if let Some(threads) = self.general.threads {
            config.threads = Some(threads);
        }

        // Apply processing settings
        if let Some(deskew) = self.processing.deskew {
            config = config.with_deskew(deskew);
        }
        if let Some(margin_trim) = self.processing.margin_trim {
            config = config.with_margin_trim(margin_trim);
        }
        if let Some(upscale) = self.processing.upscale {
            config = config.with_upscale(upscale);
        }
        if let Some(gpu) = self.processing.gpu {
            config = config.with_gpu(gpu);
        }

        // Apply advanced settings
        if let Some(internal) = self.advanced.internal_resolution {
            config.internal_resolution = internal;
        }
        if let Some(color) = self.advanced.color_correction {
            config.color_correction = color;
        }
        if let Some(offset) = self.advanced.offset_alignment {
            config.offset_alignment = offset;
        }
        if let Some(height) = self.advanced.output_height {
            config.output_height = height;
        }

        // Apply OCR settings
        if let Some(ocr) = self.ocr.enabled {
            config = config.with_ocr(ocr);
        }

        // Apply output settings
        if let Some(quality) = self.output.jpeg_quality {
            config.jpeg_quality = quality;
        }

        // Apply processing shadow removal setting
        if let Some(ref shadow) = self.processing.shadow_removal {
            config.shadow_removal = shadow.clone();
        }

        // Apply cleanup settings
        if let Some(marker) = self.cleanup.marker_removal {
            config.remove_markers = marker;
        }
        if let Some(ref colors) = self.cleanup.highlighter_colors {
            config.marker_colors = colors.clone();
        }
        if let Some(deblur) = self.cleanup.deblur {
            config.deblur = deblur;
        }
        if let Some(ref algo) = self.cleanup.deblur_algorithm {
            config.deblur_algorithm = algo.clone();
        }

        if self.processing.geometry_only.unwrap_or(false) {
            let common_action = self
                .processing
                .geometry_action
                .map(GeometryAction::from)
                .unwrap_or(GeometryAction::Report);
            config.geometry_only = true;
            config.rotation_action = self.processing.rotation_action.unwrap_or(common_action);
            config.rotation_action_configured = true;
            config.deskew_action = self.processing.deskew_action.unwrap_or_else(|| {
                if self.processing.deskew == Some(false) {
                    GeometryAction::Off
                } else {
                    common_action
                }
            });
            config.deskew_action_configured = true;
        } else {
            if let Some(action) = self.processing.rotation_action {
                config.rotation_action = action;
                config.rotation_action_configured = true;
            }
            if let Some(action) = self.processing.deskew_action {
                config.deskew_action = action;
                config.deskew_action_configured = true;
            }
        }

        if let Some(threshold) = self.processing.rotation_min_confidence {
            config.rotation_min_confidence = threshold;
        }
        if let Some(value) = self.processing.deskew_max_angle {
            config.deskew_max_angle = value;
        }
        if let Some(value) = self.processing.deskew_min_confidence {
            config.deskew_min_confidence = value;
        }
        if let Some(value) = self.processing.deskew_min_features {
            config.deskew_min_features = value;
        }
        if let Some(value) = self.processing.deskew_noop_angle {
            config.deskew_noop_angle = value;
        }

        config.enforce_geometry_only_constraints();

        config
    }

    /// Merge with CLI arguments (CLI takes precedence)
    pub fn merge_with_cli(&self, cli: &CliOverrides) -> PipelineConfig {
        let mut config = self.to_pipeline_config();
        let was_geometry_only = config.geometry_only;

        // CLI overrides take precedence
        if let Some(dpi) = cli.dpi {
            config = config.with_dpi(dpi);
        }
        if let Some(deskew) = cli.deskew {
            config = config.with_deskew(deskew);
        }
        if let Some(margin_trim) = cli.margin_trim {
            config = config.with_margin_trim(margin_trim);
        }
        if let Some(upscale) = cli.upscale {
            config = config.with_upscale(upscale);
        }
        if let Some(gpu) = cli.gpu {
            config = config.with_gpu(gpu);
        }
        if let Some(ocr) = cli.ocr {
            config = config.with_ocr(ocr);
        }
        if let Some(threads) = cli.threads {
            config.threads = Some(threads);
        }
        if let Some(internal) = cli.internal_resolution {
            config.internal_resolution = internal;
        }
        if let Some(color) = cli.color_correction {
            config.color_correction = color;
        }
        if let Some(offset) = cli.offset_alignment {
            config.offset_alignment = offset;
        }
        if let Some(height) = cli.output_height {
            config.output_height = height;
        }
        if let Some(quality) = cli.jpeg_quality {
            config.jpeg_quality = quality;
        }
        if let Some(max_pages) = cli.max_pages {
            config = config.with_max_pages(Some(max_pages));
        }
        if let Some(save_debug) = cli.save_debug {
            config.save_debug = save_debug;
        }
        if let Some(ref shadow) = cli.shadow_removal {
            config.shadow_removal = shadow.clone();
        }
        if let Some(markers) = cli.remove_markers {
            config.remove_markers = markers;
        }
        if let Some(ref colors) = cli.marker_colors {
            config.marker_colors = colors.clone();
        }
        if let Some(deblur) = cli.deblur {
            config.deblur = deblur;
        }
        if let Some(ref algo) = cli.deblur_algorithm {
            config.deblur_algorithm = algo.clone();
        }

        if let Some(geometry_only) = cli.geometry_only {
            config.geometry_only = geometry_only;
        }
        if config.geometry_only {
            if !was_geometry_only && cli.geometry_only == Some(true) {
                config.rotation_action = GeometryAction::Report;
                config.deskew_action = GeometryAction::Report;
                config.rotation_action_configured = true;
                config.deskew_action_configured = true;
            }
            if let Some(action) = cli.geometry_action {
                let action = GeometryAction::from(action);
                config.rotation_action = action;
                config.deskew_action = action;
                config.rotation_action_configured = true;
                config.deskew_action_configured = true;
            }
            if let Some(action) = cli.rotation_action {
                config.rotation_action = action;
                config.rotation_action_configured = true;
            }
            if let Some(action) = cli.deskew_action {
                config.deskew_action = action;
                config.deskew_action_configured = true;
            }
        }
        if let Some(threshold) = cli.rotation_min_confidence {
            config.rotation_min_confidence = threshold;
        }
        if let Some(value) = cli.deskew_max_angle {
            config.deskew_max_angle = value;
        }
        if let Some(value) = cli.deskew_min_confidence {
            config.deskew_min_confidence = value;
        }
        if let Some(value) = cli.deskew_min_features {
            config.deskew_min_features = value;
        }
        if let Some(value) = cli.deskew_noop_angle {
            config.deskew_noop_angle = value;
        }

        config.enforce_geometry_only_constraints();

        config
    }

    /// Get config file search paths
    pub fn search_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from("superbook.toml")];

        if let Some(config_dir) = dirs::config_dir() {
            paths.push(config_dir.join("superbook-pdf").join("config.toml"));
        }

        paths
    }
}

/// CLI override values for merging with config file
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub geometry_only: Option<bool>,
    pub geometry_action: Option<GeometryModeAction>,
    pub rotation_action: Option<GeometryAction>,
    pub rotation_min_confidence: Option<RotationConfidenceThreshold>,
    pub deskew_action: Option<GeometryAction>,
    pub deskew_max_angle: Option<DeskewMaxAngle>,
    pub deskew_min_confidence: Option<DeskewMinConfidence>,
    pub deskew_min_features: Option<DeskewMinFeatures>,
    pub deskew_noop_angle: Option<DeskewNoopAngle>,
    pub dpi: Option<u32>,
    pub deskew: Option<bool>,
    pub margin_trim: Option<f64>,
    pub upscale: Option<bool>,
    pub gpu: Option<bool>,
    pub ocr: Option<bool>,
    pub threads: Option<usize>,
    pub internal_resolution: Option<bool>,
    pub color_correction: Option<bool>,
    pub offset_alignment: Option<bool>,
    pub output_height: Option<u32>,
    pub jpeg_quality: Option<u8>,
    pub max_pages: Option<usize>,
    pub save_debug: Option<bool>,
    pub shadow_removal: Option<String>,
    pub remove_markers: Option<bool>,
    pub marker_colors: Option<Vec<String>>,
    pub deblur: Option<bool>,
    pub deblur_algorithm: Option<String>,
}

impl CliOverrides {
    /// Create new empty overrides
    pub fn new() -> Self {
        Self::default()
    }

    /// Set DPI override
    pub fn with_dpi(mut self, dpi: u32) -> Self {
        self.dpi = Some(dpi);
        self
    }

    /// Set deskew override
    pub fn with_deskew(mut self, deskew: bool) -> Self {
        self.deskew = Some(deskew);
        self
    }

    /// Set margin trim override
    pub fn with_margin_trim(mut self, margin_trim: f64) -> Self {
        self.margin_trim = Some(margin_trim);
        self
    }

    /// Set upscale override
    pub fn with_upscale(mut self, upscale: bool) -> Self {
        self.upscale = Some(upscale);
        self
    }

    /// Set GPU override
    pub fn with_gpu(mut self, gpu: bool) -> Self {
        self.gpu = Some(gpu);
        self
    }

    /// Set OCR override
    pub fn with_ocr(mut self, ocr: bool) -> Self {
        self.ocr = Some(ocr);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CFG-001: Config::default
    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.general.dpi, None);
        assert_eq!(config.processing.deskew, None);
        assert_eq!(config.advanced.internal_resolution, None);
        assert_eq!(config.ocr.enabled, None);
        assert_eq!(config.output.jpeg_quality, None);
    }

    // CFG-002: Config::load_from_path (existing file)
    #[test]
    fn test_config_load_from_path_existing() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[general]
dpi = 600

[processing]
deskew = true
"#,
        )
        .unwrap();

        let config = Config::load_from_path(&config_path).unwrap();
        assert_eq!(config.general.dpi, Some(600));
        assert_eq!(config.processing.deskew, Some(true));
    }

    // CFG-003: Config::load_from_path (non-existent file)
    #[test]
    fn test_config_load_from_path_not_found() {
        let result = Config::load_from_path(Path::new("/nonexistent/config.toml"));
        assert!(matches!(result, Err(ConfigError::NotFound(_))));
    }

    // CFG-004: Config::load (search order)
    #[test]
    fn test_config_search_paths() {
        let paths = Config::search_paths();
        assert!(!paths.is_empty());
        assert_eq!(paths[0], PathBuf::from("superbook.toml"));
    }

    // CFG-005: Config::merge (CLI priority)
    #[test]
    fn test_config_merge_cli_priority() {
        let config = Config {
            general: GeneralConfig {
                dpi: Some(300),
                ..Default::default()
            },
            processing: ProcessingConfig {
                deskew: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };

        let cli = CliOverrides::new().with_dpi(600).with_deskew(false);

        let pipeline = config.merge_with_cli(&cli);
        assert_eq!(pipeline.dpi, 600); // CLI wins
        assert!(!pipeline.deskew); // CLI wins
    }

    // CFG-006: Config::to_pipeline_config
    #[test]
    fn test_config_to_pipeline_config() {
        let config = Config {
            general: GeneralConfig {
                dpi: Some(450),
                threads: Some(8),
                ..Default::default()
            },
            processing: ProcessingConfig {
                deskew: Some(false),
                margin_trim: Some(1.0),
                upscale: Some(true),
                gpu: Some(true),
                ..Default::default()
            },
            advanced: AdvancedConfig {
                internal_resolution: Some(true),
                color_correction: Some(true),
                offset_alignment: Some(true),
                output_height: Some(4000),
            },
            ocr: OcrConfig {
                enabled: Some(true),
                ..Default::default()
            },
            output: OutputConfig {
                jpeg_quality: Some(95),
                ..Default::default()
            },
            ..Default::default()
        };

        let pipeline = config.to_pipeline_config();
        assert_eq!(pipeline.dpi, 450);
        assert_eq!(pipeline.threads, Some(8));
        assert!(!pipeline.deskew);
        assert!((pipeline.margin_trim - 1.0).abs() < f64::EPSILON);
        assert!(pipeline.upscale);
        assert!(pipeline.gpu);
        assert!(pipeline.internal_resolution);
        assert!(pipeline.color_correction);
        assert!(pipeline.offset_alignment);
        assert_eq!(pipeline.output_height, 4000);
        assert!(pipeline.ocr);
        assert_eq!(pipeline.jpeg_quality, 95);
    }

    // CFG-007: TOML parse (complete config)
    #[test]
    fn test_config_toml_parse_complete() {
        let toml = r#"
[general]
dpi = 300
threads = 4
verbose = 2

[processing]
deskew = true
margin_trim = 0.5
upscale = true
gpu = true

[advanced]
internal_resolution = true
color_correction = true
offset_alignment = true
output_height = 3508

[ocr]
enabled = true
language = "ja"

[output]
jpeg_quality = 90
skip_existing = true
"#;

        let config = Config::from_toml(toml).unwrap();
        assert_eq!(config.general.dpi, Some(300));
        assert_eq!(config.general.threads, Some(4));
        assert_eq!(config.general.verbose, Some(2));
        assert_eq!(config.processing.deskew, Some(true));
        assert_eq!(config.processing.margin_trim, Some(0.5));
        assert_eq!(config.advanced.internal_resolution, Some(true));
        assert_eq!(config.ocr.language, Some("ja".to_string()));
        assert_eq!(config.output.jpeg_quality, Some(90));
        assert_eq!(config.output.skip_existing, Some(true));
    }

    // CFG-008: TOML parse (partial config)
    #[test]
    fn test_config_toml_parse_partial() {
        let toml = r#"
[general]
dpi = 600
"#;

        let config = Config::from_toml(toml).unwrap();
        assert_eq!(config.general.dpi, Some(600));
        assert_eq!(config.general.threads, None);
        assert_eq!(config.processing.deskew, None);
    }

    // CFG-009: TOML parse (empty file)
    #[test]
    fn test_config_toml_parse_empty() {
        let config = Config::from_toml("").unwrap();
        assert_eq!(config, Config::default());
    }

    // CFG-010: TOML parse (invalid format)
    #[test]
    fn test_config_toml_parse_invalid() {
        let result = Config::from_toml("this is not valid toml [[[");
        assert!(matches!(result, Err(ConfigError::TomlParse(_))));
    }

    #[test]
    fn test_config_to_toml() {
        let config = Config {
            general: GeneralConfig {
                dpi: Some(300),
                ..Default::default()
            },
            ..Default::default()
        };

        let toml_str = config.to_toml().unwrap();
        assert!(toml_str.contains("dpi = 300"));
    }

    #[test]
    fn test_geometry_config_toml_round_trip_and_safe_pipeline_conversion() {
        let toml = r#"
[processing]
geometry_only = true
geometry_action = "apply"
rotation_action = "off"
deskew_action = "report"
deskew = true
margin_trim = 2.0
upscale = true
gpu = true
shadow_removal = "both"

[advanced]
internal_resolution = true
color_correction = true
offset_alignment = true
output_height = 3508

[ocr]
enabled = true

[cleanup]
marker_removal = true
deblur = true
"#;

        let config = Config::from_toml(toml).unwrap();
        assert_eq!(
            config.processing.geometry_action,
            Some(crate::cli::GeometryModeAction::Apply)
        );
        let serialized = config.to_toml().unwrap();
        let round_trip = Config::from_toml(&serialized).unwrap();
        assert_eq!(round_trip, config);

        let pipeline = config.to_pipeline_config();
        assert!(pipeline.geometry_only);
        assert_eq!(pipeline.rotation_action, crate::cli::GeometryAction::Off);
        assert_eq!(pipeline.deskew_action, crate::cli::GeometryAction::Report);
        assert_eq!(pipeline.margin_trim, 0.0);
        assert!(!pipeline.upscale);
        assert!(!pipeline.gpu);
        assert!(!pipeline.ocr);
        assert_eq!(pipeline.shadow_removal, "none");
        assert!(!pipeline.deblur);
        assert!(!pipeline.internal_resolution);
        assert!(!pipeline.color_correction);
        assert!(!pipeline.remove_markers);
        assert!(!pipeline.offset_alignment);
        assert_eq!(pipeline.output_height, 0);
    }

    #[test]
    fn test_geometry_config_cli_no_deskew_preserves_rotation_action() {
        let config = Config::from_toml(
            r#"
[processing]
geometry_only = true
rotation_action = "report"
deskew_action = "report"
"#,
        )
        .unwrap();
        let cli = CliOverrides::new().with_deskew(false);

        let pipeline = config.merge_with_cli(&cli);

        assert_eq!(pipeline.rotation_action, GeometryAction::Report);
        assert_eq!(pipeline.deskew_action, GeometryAction::Off);
    }

    #[test]
    fn test_rotation_confidence_config_default_and_cli_precedence() {
        let config = Config::from_toml(
            r#"
[processing]
geometry_only = true
rotation_min_confidence = 0.95
"#,
        )
        .unwrap();
        assert_eq!(
            config.to_pipeline_config().rotation_min_confidence.get(),
            0.95
        );

        let omitted = config.merge_with_cli(&CliOverrides::default());
        assert_eq!(omitted.rotation_min_confidence.get(), 0.95);

        let cli = CliOverrides {
            rotation_min_confidence: Some(
                "0.97"
                    .parse::<crate::RotationConfidenceThreshold>()
                    .unwrap(),
            ),
            ..CliOverrides::default()
        };
        let overridden = config.merge_with_cli(&cli);
        assert_eq!(overridden.rotation_min_confidence.get(), 0.97);
    }

    #[test]
    fn deskew_policy_config_defaults_toml_and_cli_precedence_are_validated() {
        let defaults = Config::default().to_pipeline_config();
        assert_eq!(defaults.deskew_max_angle.get(), 5.0);
        assert_eq!(defaults.deskew_min_confidence.get(), 0.90);
        assert_eq!(defaults.deskew_min_features.get(), 100);
        assert_eq!(defaults.deskew_noop_angle.get(), 0.10);
        defaults.deskew_policy().unwrap();

        let config = Config::from_toml(
            r#"
[processing]
geometry_only = true
deskew_max_angle = 4.0
deskew_min_confidence = 0.95
deskew_min_features = 150
deskew_noop_angle = 0.2
"#,
        )
        .unwrap();
        let from_toml = config.to_pipeline_config();
        assert_eq!(from_toml.deskew_max_angle.get(), 4.0);
        assert_eq!(from_toml.deskew_min_confidence.get(), 0.95);
        assert_eq!(from_toml.deskew_min_features.get(), 150);
        assert_eq!(from_toml.deskew_noop_angle.get(), 0.2);
        from_toml.deskew_policy().unwrap();

        let omitted = config.merge_with_cli(&CliOverrides::default());
        assert_eq!(omitted.deskew_max_angle.get(), 4.0);
        assert_eq!(omitted.deskew_min_confidence.get(), 0.95);
        assert_eq!(omitted.deskew_min_features.get(), 150);
        assert_eq!(omitted.deskew_noop_angle.get(), 0.2);

        let overrides = CliOverrides {
            deskew_max_angle: Some(DeskewMaxAngle::new(5.0).unwrap()),
            deskew_min_confidence: Some(DeskewMinConfidence::new(0.90).unwrap()),
            deskew_min_features: Some(DeskewMinFeatures::new(100).unwrap()),
            deskew_noop_angle: Some(DeskewNoopAngle::new(0.10).unwrap()),
            ..CliOverrides::default()
        };
        let overridden = config.merge_with_cli(&overrides);
        assert_eq!(overridden.deskew_max_angle.get(), 5.0);
        assert_eq!(overridden.deskew_min_confidence.get(), 0.90);
        assert_eq!(overridden.deskew_min_features.get(), 100);
        assert_eq!(overridden.deskew_noop_angle.get(), 0.10);
        overridden.deskew_policy().unwrap();
    }

    #[test]
    fn deskew_policy_cross_field_validation_occurs_after_merge() {
        let config = Config::from_toml(
            r#"
[processing]
geometry_only = true
deskew_max_angle = 0.5
deskew_noop_angle = 0.5
"#,
        )
        .unwrap();
        assert!(config.to_pipeline_config().deskew_policy().is_err());

        let repaired = config.merge_with_cli(&CliOverrides {
            deskew_max_angle: Some(DeskewMaxAngle::new(1.0).unwrap()),
            ..CliOverrides::default()
        });
        repaired.deskew_policy().unwrap();
    }

    #[test]
    fn test_rotation_confidence_config_rejects_invalid_values() {
        for value in ["-0.1", "1.1", "nan", "inf"] {
            let toml =
                format!("[processing]\ngeometry_only = true\nrotation_min_confidence = {value}\n");
            assert!(Config::from_toml(&toml).is_err(), "{value}");
        }
    }

    #[test]
    fn test_cli_overrides_builder() {
        let overrides = CliOverrides::new()
            .with_dpi(600)
            .with_deskew(false)
            .with_margin_trim(1.5)
            .with_upscale(true)
            .with_gpu(false)
            .with_ocr(true);

        assert_eq!(overrides.dpi, Some(600));
        assert_eq!(overrides.deskew, Some(false));
        assert_eq!(overrides.margin_trim, Some(1.5));
        assert_eq!(overrides.upscale, Some(true));
        assert_eq!(overrides.gpu, Some(false));
        assert_eq!(overrides.ocr, Some(true));
    }

    #[test]
    fn test_config_error_display() {
        let err = ConfigError::NotFound(PathBuf::from("/test/path"));
        assert!(err.to_string().contains("Config file not found"));
    }

    #[test]
    fn test_config_new() {
        let config = Config::new();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn test_config_merge_empty_cli() {
        let config = Config {
            general: GeneralConfig {
                dpi: Some(300),
                ..Default::default()
            },
            ..Default::default()
        };

        let cli = CliOverrides::new();
        let pipeline = config.merge_with_cli(&cli);
        assert_eq!(pipeline.dpi, 300); // Config value preserved
    }

    #[test]
    fn test_config_merge_partial_cli() {
        let config = Config {
            general: GeneralConfig {
                dpi: Some(300),
                threads: Some(4),
                ..Default::default()
            },
            processing: ProcessingConfig {
                deskew: Some(true),
                margin_trim: Some(0.5),
                ..Default::default()
            },
            ..Default::default()
        };

        let cli = CliOverrides::new().with_dpi(600);
        let pipeline = config.merge_with_cli(&cli);
        assert_eq!(pipeline.dpi, 600); // CLI wins
        assert_eq!(pipeline.threads, Some(4)); // Config preserved
        assert!(pipeline.deskew); // Config preserved
    }
}
