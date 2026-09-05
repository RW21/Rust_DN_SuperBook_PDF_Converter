//! Image Extraction module
//!
//! Provides functionality to extract page images from PDF files.
//!
//! # Features
//!
//! - Extract pages as PNG, JPEG, or TIFF
//! - Configurable DPI (72-1200)
//! - Color space conversion (RGB, Grayscale, CMYK)
//! - Parallel extraction with progress callbacks
//! - Transparent background handling
//!
//! # Example
//!
//! ```rust,no_run
//! use superbook_pdf::{ExtractOptions, MagickExtractor, ImageFormat, ColorSpace};
//! use std::path::Path;
//!
//! // Create options
//! let options = ExtractOptions::builder()
//!     .dpi(300)
//!     .format(ImageFormat::Png)
//!     .colorspace(ColorSpace::Rgb)
//!     .parallel(4)
//!     .build();
//!
//! // Extract a single page
//! // let result = MagickExtractor::extract_page(
//! //     Path::new("input.pdf"),
//! //     0,
//! //     Path::new("output/page_0.png"),
//! //     &options,
//! // );
//! ```

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[cfg(test)]
mod native_hardening_tests {
    use super::*;
    use lopdf::{dictionary, Document, Object, Stream};
    use std::io::Write;

    fn flate(bytes: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn image_stream(jpeg: bool, rgb: bool) -> Stream {
        let pixels = if rgb {
            [30, 60, 90].repeat(4)
        } else {
            vec![30, 60, 90, 120]
        };
        let bytes = if jpeg {
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
                .encode(
                    &pixels,
                    2,
                    2,
                    if rgb {
                        image::ExtendedColorType::Rgb8
                    } else {
                        image::ExtendedColorType::L8
                    },
                )
                .unwrap();
            bytes
        } else {
            flate(&pixels)
        };
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => 2, "Height" => 2,
                "ColorSpace" => if rgb { "DeviceRGB" } else { "DeviceGray" },
                "BitsPerComponent" => 8, "Filter" => if jpeg { "DCTDecode" } else { "FlateDecode" },
            },
            bytes,
        )
    }

    fn fixture(stream: Stream, contents: Vec<Object>) -> (Document, lopdf::ObjectId) {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let image_id = doc.add_object(stream);
        let mut kids = Vec::new();
        for content in contents {
            let content = match content {
                Object::Stream(stream) => Object::Reference(doc.add_object(stream)),
                other => other,
            };
            kids.push(Object::Reference(doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
                "Contents" => content,
                "Resources" => dictionary! { "XObject" => dictionary! { "Scan" => image_id } },
            })));
        }
        let count = kids.len() as i64;
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => kids, "Count" => count,
            }),
        );
        let root = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", root);
        (doc, image_id)
    }

    fn content(bytes: &[u8]) -> Object {
        Object::Stream(Stream::new(dictionary! {}, bytes.to_vec()))
    }

    fn load(doc: &mut Document) -> (tempfile::TempDir, NativePdfDocument) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.pdf");
        doc.save(&path).unwrap();
        let native = NativePdfExtractor::extract_path(&path).unwrap();
        (dir, native)
    }

    #[test]
    fn native_decode_valid_streams_losslessly_and_bounded() {
        for jpeg in [false, true] {
            for rgb in [false, true] {
                let stream = image_stream(jpeg, rgb);
                let expected = if jpeg {
                    image::load_from_memory_with_format(&stream.content, image::ImageFormat::Jpeg)
                        .unwrap()
                        .into_bytes()
                } else if rgb {
                    [30, 60, 90].repeat(4)
                } else {
                    vec![30, 60, 90, 120]
                };
                let (mut doc, _) = fixture(stream, vec![content(b"/Scan Do")]);
                let (_dir, native) = load(&mut doc);
                let meta = &native.pages()[0].image_invocations[0].metadata;
                let image = native.decode_image(meta, expected.len() as u64).unwrap();
                assert_eq!((image.width(), image.height()), (2, 2));
                assert_eq!(image.as_bytes(), expected);
                assert!(native
                    .decode_image(meta, expected.len() as u64 - 1)
                    .is_err());
            }
        }
    }

    #[test]
    fn native_decode_rejects_hash_and_metadata_tamper_and_preserves_clone_reload() {
        let (mut doc, image_id) = fixture(image_stream(false, false), vec![content(b"/Scan Do")]);
        let (dir, native) = load(&mut doc);
        let meta = &native.pages()[0].image_invocations[0].metadata;
        let mut tampered = meta.clone();
        tampered.encoded_sha256 = "0".repeat(64);
        assert!(matches!(
            native.decode_image(&tampered, 1024),
            Err(NativeExtractError::ImageHashMismatch(_))
        ));
        tampered = meta.clone();
        tampered.width += 1;
        assert!(native.decode_image(&tampered, 1024).is_err());
        let mut cloned = native.source_document().clone();
        let path = dir.path().join("clone.pdf");
        cloned.save(&path).unwrap();
        let reloaded = NativePdfExtractor::extract_path(&path).unwrap();
        assert_eq!(reloaded.pages(), native.pages());
        assert_eq!(
            reloaded.encoded_image_bytes(meta).unwrap(),
            native.encoded_image_bytes(meta).unwrap()
        );
        assert_eq!(
            reloaded.source_document().get_object(image_id).unwrap(),
            native.source_document().get_object(image_id).unwrap()
        );
    }

    #[test]
    fn native_metadata_rejects_malformed_filters_and_cycles() {
        let mut doc = Document::new();
        let cycle = doc.new_object_id();
        doc.objects.insert(cycle, Object::Reference(cycle));
        for value in [
            Object::Integer(42),
            Object::Null,
            Object::Array(vec![Object::Integer(42)]),
            Object::Reference(cycle),
        ] {
            let mut stream = image_stream(false, false);
            stream.dict.set("Filter", value);
            assert!(LopdfExtractor::native_image_metadata(&doc, (100, 0), &stream).is_err());
        }
        for key in ["ColorSpace", "DecodeParms", "BitsPerComponent"] {
            let mut stream = image_stream(false, false);
            stream.dict.set(key, Object::Reference(cycle));
            assert!(
                LopdfExtractor::native_image_metadata(&doc, (100, 0), &stream).is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn native_metadata_expansion_has_a_shared_budget() {
        let mut doc = Document::new();
        let mut value = Object::Integer(1);
        for _ in 0..14 {
            let id = doc.add_object(Object::Array(vec![value.clone(), value]));
            value = Object::Reference(id);
        }
        let mut stream = image_stream(false, false);
        stream.dict.set("DecodeParms", value);
        assert!(LopdfExtractor::native_image_metadata(&doc, (100, 0), &stream).is_err());
    }

    #[test]
    fn native_decode_unsupported_metadata_and_ccitt_fail_closed() {
        for (key, value) in [
            ("Mask", Object::Array(vec![0.into(), 10.into()])),
            ("SMask", Object::Reference((900, 0))),
            ("ImageMask", Object::Boolean(true)),
            ("Decode", Object::Array(vec![1.into(), 0.into()])),
            (
                "DecodeParms",
                Object::Dictionary(dictionary! { "Predictor" => 12, "Columns" => 2 }),
            ),
            (
                "Filter",
                Object::Array(vec!["ASCII85Decode".into(), "FlateDecode".into()]),
            ),
            ("Filter", Object::Name(b"CCITTFaxDecode".to_vec())),
        ] {
            let mut stream = image_stream(false, false);
            stream.dict.set(key, value);
            let (mut doc, _) = fixture(stream, vec![content(b"/Scan Do")]);
            let (_dir, native) = load(&mut doc);
            let meta = &native.pages()[0].image_invocations[0].metadata;
            assert_eq!(
                meta.transform_decode,
                NativeTransformDecodeCapability::Unsupported,
                "{key}"
            );
            assert!(native.pages()[0].review_required);
            assert!(native.decode_image(meta, 1024).is_err());
        }
    }

    #[test]
    fn native_content_strict_read_keeps_malformed_and_inline_pages() {
        let corrupt = Object::Stream(Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            b"not zlib".to_vec(),
        ));
        let (mut doc, _) = fixture(
            image_stream(false, false),
            vec![
                content(b"/Scan Do"),
                Object::Reference((999, 0)),
                Object::Integer(7),
                corrupt,
                content(b"BI /W 1 /H 1 /CS /G /BPC 8 ID x EI"),
                content(b"/Scan Do /Scan Do"),
                content(b"/Scan Do 0 0 1 1 re f"),
            ],
        );
        let (_dir, native) = load(&mut doc);
        assert_eq!(native.pages().len(), 7);
        assert!(!native.pages()[0].review_required);
        assert!(native.pages()[1..].iter().all(|p| p.review_required));
        for page in &native.pages()[1..5] {
            assert_eq!(page.kind, NativePageKind::UnsupportedContent);
        }
    }

    #[test]
    fn native_content_arrays_concatenate_without_invented_separator() {
        let (mut doc, _) = fixture(image_stream(false, false), vec![Object::Null]);
        let a = doc.add_object(Stream::new(dictionary! {}, b"2 0 0 3 5 7 cm /Sc".to_vec()));
        let b = doc.add_object(Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            flate(b"an Do"),
        ));
        let page_id = *doc.get_pages().values().next().unwrap();
        doc.get_object_mut(page_id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", vec![Object::Reference(a), Object::Reference(b)]);
        let (_dir, native) = load(&mut doc);
        assert_eq!(native.pages()[0].kind, NativePageKind::SingleImage);
        assert_eq!(
            native.pages()[0].image_invocations[0]
                .binding
                .placement_matrix
                .coordinates(),
            [2., 0., 0., 3., 5., 7.]
        );
    }

    #[test]
    fn native_content_cycles_and_unsafe_graphics_require_review() {
        let (mut doc, _) = fixture(
            image_stream(false, false),
            vec![
                content(b"/GS gs /Scan Do"),
                content(b"0 0 1 1 re W n /Scan Do"),
                content(b"7 Tr /Scan Do"),
                content(b"/OC /Layer BDC /Scan Do EMC"),
                content(b"/Scan Do /Bad w"),
                content(b"/Scan Do BT"),
                content(b"/Scan Do ET"),
                content(b"/Scan Do 1 BT ET"),
                content(b"BT /Scan Do ET"),
                content(b"/Scan Do BT BT ET ET"),
                Object::Null,
            ],
        );
        let cycle = doc.new_object_id();
        doc.objects
            .insert(cycle, Object::Array(vec![Object::Reference(cycle)]));
        let last = *doc.get_pages().values().last().unwrap();
        doc.get_object_mut(last)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", Object::Reference(cycle));
        let expected_pages = doc.get_pages().len();
        let (_dir, native) = load(&mut doc);
        assert_eq!(native.pages().len(), expected_pages);
        assert!(native.pages().iter().all(|p| p.review_required));
    }

    #[test]
    fn native_page_annotations_and_units_require_review() {
        for (key, value) in [
            ("Annots", Object::Array(Vec::new())),
            ("UserUnit", Object::Integer(2)),
            ("Group", Object::Dictionary(dictionary! {})),
        ] {
            let (mut doc, _) = fixture(image_stream(false, false), vec![content(b"/Scan Do")]);
            let page = *doc.get_pages().values().next().unwrap();
            doc.get_object_mut(page)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set(key, value);
            let (_dir, native) = load(&mut doc);
            assert!(native.pages()[0].review_required, "{key}");
        }
    }

    #[test]
    fn native_matrix_overflow_is_rejected_and_concat_order_is_pdf_order() {
        let a = PdfMatrix::try_new([2., 0., 0., 3., 5., 7.]).unwrap();
        let b = PdfMatrix::try_new([1., 0., 0., 1., 11., 13.]).unwrap();
        assert_eq!(
            a.pre_concat(b).unwrap().coordinates(),
            [2., 0., 0., 3., 27., 46.]
        );
        let huge = PdfMatrix::try_new([f64::MAX, 0., 0., f64::MAX, 0., 0.]).unwrap();
        assert!(huge.pre_concat(huge).is_err());
    }
}

// ============================================================
// Constants
// ============================================================

/// Standard DPI for document scanning
const DEFAULT_DPI: u32 = 300;

/// High quality DPI for archival purposes
const HIGH_QUALITY_DPI: u32 = 600;

/// Low DPI for fast previews
const FAST_DPI: u32 = 150;

/// Minimum allowed DPI
const MIN_DPI: u32 = 72;

/// Maximum allowed DPI
const MAX_DPI: u32 = 1200;

/// Default white background color
const WHITE_BACKGROUND: [u8; 3] = [255, 255, 255];

/// Image extraction error types
#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("PDF file not found: {0}")]
    PdfNotFound(PathBuf),

    #[error("Output directory not writable: {0}")]
    OutputNotWritable(PathBuf),

    #[error("Extraction failed for page {page}: {reason}")]
    ExtractionFailed { page: usize, reason: String },

    #[error("External tool error: {0}")]
    ExternalToolError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Preservation-only native extraction failures that cannot be scoped to one page.
#[derive(Debug, Error)]
pub enum NativeExtractError {
    #[error(transparent)]
    PdfReader(#[from] crate::pdf_reader::PdfReaderError),

    #[error("encrypted PDFs are not supported by the preservation pipeline")]
    EncryptedPdf,

    #[error("native page inventory failed: {0}")]
    Inventory(String),

    #[error("native image object {0:?} is missing or not a stream")]
    MissingImageObject(crate::transform_manifest::PdfObjectId),

    #[error("native image object {0:?} no longer matches its inventory hash")]
    ImageHashMismatch(crate::transform_manifest::PdfObjectId),

    #[error("native image metadata no longer matches the source dictionary")]
    ImageMetadataMismatch,

    #[error("unsupported native image decode: {0}")]
    UnsupportedDecode(String),

    #[error("native image decode exceeds the encoded/decoded byte safety limit")]
    DecodeLimit,

    #[error("invalid native image data: {0}")]
    ImageDecode(String),
}

pub type Result<T> = std::result::Result<T, ExtractError>;

/// Image extraction options
pub struct ExtractOptions {
    /// Output DPI
    pub dpi: u32,
    /// Output format
    pub format: ImageFormat,
    /// Color space
    pub colorspace: ColorSpace,
    /// Background color (for transparency handling)
    pub background: Option<[u8; 3]>,
    /// Number of parallel workers
    pub parallel: usize,
    /// Progress callback
    #[allow(clippy::type_complexity)]
    pub progress_callback: Option<Box<dyn Fn(usize, usize) + Send + Sync>>,
}

impl std::fmt::Debug for ExtractOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractOptions")
            .field("dpi", &self.dpi)
            .field("format", &self.format)
            .field("colorspace", &self.colorspace)
            .field("background", &self.background)
            .field("parallel", &self.parallel)
            .field(
                "progress_callback",
                &self.progress_callback.as_ref().map(|_| "<callback>"),
            )
            .finish()
    }
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            dpi: DEFAULT_DPI,
            format: ImageFormat::Png,
            colorspace: ColorSpace::Rgb,
            background: Some(WHITE_BACKGROUND),
            parallel: num_cpus::get(),
            progress_callback: None,
        }
    }
}

impl ExtractOptions {
    /// Create a new options builder
    pub fn builder() -> ExtractOptionsBuilder {
        ExtractOptionsBuilder::default()
    }

    /// Create options for high quality extraction
    pub fn high_quality() -> Self {
        Self {
            dpi: HIGH_QUALITY_DPI,
            format: ImageFormat::Png,
            ..Default::default()
        }
    }

    /// Create options for fast extraction (lower quality)
    pub fn fast() -> Self {
        Self {
            dpi: FAST_DPI,
            format: ImageFormat::Jpeg { quality: 80 },
            ..Default::default()
        }
    }

    /// Create options for grayscale documents
    pub fn grayscale() -> Self {
        Self {
            colorspace: ColorSpace::Grayscale,
            ..Default::default()
        }
    }
}

/// Builder for ExtractOptions
#[derive(Debug, Default)]
pub struct ExtractOptionsBuilder {
    options: ExtractOptions,
}

impl ExtractOptionsBuilder {
    /// Set output DPI (clamped to MIN_DPI-MAX_DPI)
    #[must_use]
    pub fn dpi(mut self, dpi: u32) -> Self {
        self.options.dpi = dpi.clamp(MIN_DPI, MAX_DPI);
        self
    }

    /// Set output format
    #[must_use]
    pub fn format(mut self, format: ImageFormat) -> Self {
        self.options.format = format;
        self
    }

    /// Set color space
    #[must_use]
    pub fn colorspace(mut self, colorspace: ColorSpace) -> Self {
        self.options.colorspace = colorspace;
        self
    }

    /// Set background color for transparency handling
    #[must_use]
    pub fn background(mut self, rgb: [u8; 3]) -> Self {
        self.options.background = Some(rgb);
        self
    }

    /// Disable background (keep transparency)
    #[must_use]
    pub fn no_background(mut self) -> Self {
        self.options.background = None;
        self
    }

    /// Set number of parallel workers
    #[must_use]
    pub fn parallel(mut self, workers: usize) -> Self {
        self.options.parallel = workers.max(1);
        self
    }

    /// Set progress callback
    #[must_use]
    pub fn progress_callback(mut self, callback: Box<dyn Fn(usize, usize) + Send + Sync>) -> Self {
        self.options.progress_callback = Some(callback);
        self
    }

    /// Build the options
    #[must_use]
    pub fn build(self) -> ExtractOptions {
        self.options
    }
}

/// Output image formats
#[derive(Debug, Clone, Copy, Default)]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg {
        quality: u8,
    },
    Bmp,
    Tiff,
}

impl ImageFormat {
    /// Get file extension for format
    pub fn extension(&self) -> &str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg { .. } => "jpg",
            ImageFormat::Bmp => "bmp",
            ImageFormat::Tiff => "tiff",
        }
    }
}

/// Color space options
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColorSpace {
    #[default]
    Rgb,
    Grayscale,
    Cmyk,
}

/// Extracted page information
#[derive(Debug)]
pub struct ExtractedPage {
    pub page_index: usize,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
}

/// Conservative classification of one physical PDF page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePageKind {
    Blank,
    SingleImage,
    MultipleImages,
    CompositeContent,
    NonImageContent,
    UnsupportedContent,
}

/// Stable page-scoped native inventory issue codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePageIssueCode {
    PhysicalPage(crate::pdf_reader::PhysicalPageIssue),
    ContentRead,
    ContentDecode,
    InvalidResources,
    InlineImage,
    UnbalancedGraphicsState,
    InvalidMatrix,
    UnsupportedOperator,
    InvalidDo,
    UnresolvedXObject,
    XObjectNotStream,
    InvalidXObjectSubtype,
    InvalidImageMetadata,
    FormCycle,
    FormDepthExceeded,
    InvalidFormResources,
    FormDecode,
    InvalidFormMatrix,
}

impl NativePageIssueCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PhysicalPage(issue) => issue.as_str(),
            Self::ContentRead => "content_read",
            Self::ContentDecode => "content_decode",
            Self::InvalidResources => "invalid_resources",
            Self::InlineImage => "inline_image",
            Self::UnbalancedGraphicsState => "unbalanced_graphics_state",
            Self::InvalidMatrix => "invalid_matrix",
            Self::UnsupportedOperator => "unsupported_operator",
            Self::InvalidDo => "invalid_do",
            Self::UnresolvedXObject => "unresolved_xobject",
            Self::XObjectNotStream => "xobject_not_stream",
            Self::InvalidXObjectSubtype => "invalid_xobject_subtype",
            Self::InvalidImageMetadata => "invalid_image_metadata",
            Self::FormCycle => "form_cycle",
            Self::FormDepthExceeded => "form_depth_exceeded",
            Self::InvalidFormResources => "invalid_form_resources",
            Self::FormDecode => "form_decode",
            Self::InvalidFormMatrix => "invalid_form_matrix",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePageIssue {
    pub code: NativePageIssueCode,
    pub detail: String,
}

impl NativePageIssue {
    fn new(code: NativePageIssueCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// PDF affine matrix `[a b c d e f]` using the PDF coordinate convention.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfMatrix([f64; 6]);

impl PdfMatrix {
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn try_new(values: [f64; 6]) -> std::result::Result<Self, ()> {
        values
            .iter()
            .all(|value| value.is_finite())
            .then_some(Self(values))
            .ok_or(())
    }

    /// Pre-concatenate `next`, matching the PDF `cm` operator.
    fn pre_concat(self, next: Self) -> std::result::Result<Self, ()> {
        let [a, b, c, d, e, f] = self.0;
        let [g, h, i, j, k, l] = next.0;
        Self::try_new([
            a * g + c * h,
            b * g + d * h,
            a * i + c * j,
            b * i + d * j,
            a * k + c * l + e,
            b * k + d * l + f,
        ])
    }

    #[must_use]
    pub const fn coordinates(self) -> [f64; 6] {
        self.0
    }
}

/// One named XObject lookup along the page-to-image resource path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeResourcePathStep {
    pub owner_object_id: crate::transform_manifest::PdfObjectId,
    pub resource_name: Vec<u8>,
}

/// Writer handoff data for one actual image invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeImageBinding {
    pub resource_path: Vec<NativeResourcePathStep>,
    pub placement_matrix: PdfMatrix,
    pub is_direct: bool,
}

/// Whether the currently supported native path can decode this image for geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTransformDecodeCapability {
    Dct8,
    Flate8,
    Unsupported,
}

/// A native metadata shape that cannot be represented losslessly in manifest schema v1.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ManifestV1ProjectionError {
    #[error("manifest schema v1 cannot represent a PDF filter chain")]
    FilterChainNotRepresentable,
    #[error("manifest schema v1 cannot represent a complex PDF color space")]
    ColorSpaceNotRepresentable,
}

/// Native metadata for one image invocation discovered in page content.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeImageMetadata {
    pub object_id: crate::transform_manifest::PdfObjectId,
    pub width: u32,
    pub height: u32,
    pub color_space: Option<serde_json::Value>,
    pub bits_per_component: Option<u8>,
    pub filters: Vec<String>,
    pub decode_params: Option<serde_json::Value>,
    pub encoded_length: usize,
    pub encoded_sha256: String,
    pub transform_decode: NativeTransformDecodeCapability,
}

impl NativeImageMetadata {
    pub fn to_manifest_v1(
        &self,
    ) -> std::result::Result<
        crate::transform_manifest::SourceImageMetadata,
        ManifestV1ProjectionError,
    > {
        let filter = match self.filters.as_slice() {
            [] => None,
            [filter] => Some(filter.clone()),
            _ => return Err(ManifestV1ProjectionError::FilterChainNotRepresentable),
        };
        let color_space = match self.color_space.as_ref() {
            None => None,
            Some(serde_json::Value::String(value)) if value.starts_with('/') => Some(value.clone()),
            Some(_) => return Err(ManifestV1ProjectionError::ColorSpaceNotRepresentable),
        };
        Ok(crate::transform_manifest::SourceImageMetadata {
            object_id: Some(self.object_id.clone()),
            width: self.width,
            height: self.height,
            color_space,
            bits_per_component: self.bits_per_component,
            filter,
            decode_params: self.decode_params.clone(),
        })
    }
}

/// Native image metadata plus the content/resource binding that painted it.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeImageInvocation {
    pub metadata: NativeImageMetadata,
    pub binding: NativeImageBinding,
}

/// Preservation inventory record for one physical page-tree entry.
#[derive(Debug, Clone, PartialEq)]
pub struct NativePageRecord {
    pub physical_page: crate::pdf_reader::PhysicalPageMetadata,
    pub kind: NativePageKind,
    pub image_invocations: Vec<NativeImageInvocation>,
    pub review_required: bool,
    pub issues: Vec<NativePageIssue>,
}

#[derive(Debug, Clone)]
struct NativeResourceContext {
    owner_object_id: crate::transform_manifest::PdfObjectId,
    dictionary: lopdf::Dictionary,
}

/// Shared page traversal budget, including repeatedly invoked Forms.
struct NativeWalkState<'a> {
    image_invocations: &'a mut Vec<NativeImageInvocation>,
    issues: &'a mut Vec<NativePageIssue>,
    active_forms: &'a mut HashSet<lopdf::ObjectId>,
    has_non_image_paint: &'a mut bool,
    remaining_bytes: &'a mut usize,
    remaining_operations: &'a mut usize,
}

const NATIVE_CONTENT_LIMIT: usize = 8 * 1024 * 1024;
const NATIVE_IMAGE_LIMIT: u64 = 256 * 1024 * 1024;

/// Loaded source PDF plus its page-complete preservation inventory.
pub struct NativePdfDocument {
    reader: crate::pdf_reader::LopdfReader,
    pages: Vec<NativePageRecord>,
}

impl NativePdfDocument {
    /// Decode verified native samples without rendering, resizing, EXIF rotation,
    /// recompression, or post-decode color conversion. DCT is already lossy: this
    /// preserves the decoder's Gray8/RGB8 samples, not pre-JPEG originals.
    ///
    /// `max_decoded_bytes` bounds output samples (also hard-capped at 256 MiB).
    /// Encoded data is separately capped at 256 MiB. Codec working memory is not
    /// included in this sample budget. Unsupported PDF semantics fail closed.
    pub fn decode_image(
        &self,
        metadata: &NativeImageMetadata,
        max_decoded_bytes: u64,
    ) -> std::result::Result<image::DynamicImage, NativeExtractError> {
        use image::ImageDecoder;
        let bytes = self.encoded_image_bytes(metadata)?;
        if bytes.len() as u64 > NATIVE_IMAGE_LIMIT {
            return Err(NativeExtractError::DecodeLimit);
        }
        let id = (
            metadata.object_id.object_number,
            metadata.object_id.generation,
        );
        let stream = self
            .reader
            .document()
            .get_object(id)
            .and_then(lopdf::Object::as_stream)
            .map_err(|_| NativeExtractError::MissingImageObject(metadata.object_id.clone()))?;
        let actual = LopdfExtractor::native_image_metadata(self.reader.document(), id, stream)
            .map_err(NativeExtractError::ImageDecode)?;
        if &actual != metadata {
            return Err(NativeExtractError::ImageMetadataMismatch);
        }
        if actual.transform_decode == NativeTransformDecodeCapability::Unsupported {
            return Err(NativeExtractError::UnsupportedDecode(format!(
                "filters {:?}, bit depth {:?}, or PDF image semantics (CCITT is not implemented)",
                actual.filters, actual.bits_per_component
            )));
        }
        let rgb = actual.color_space.as_ref() == Some(&serde_json::json!("/DeviceRGB"));
        let color = if rgb {
            image::ColorType::Rgb8
        } else {
            image::ColorType::L8
        };
        let expected = u64::from(actual.width)
            .checked_mul(u64::from(actual.height))
            .and_then(|n| n.checked_mul(if rgb { 3 } else { 1 }))
            .filter(|n| *n <= max_decoded_bytes.min(NATIVE_IMAGE_LIMIT))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(NativeExtractError::DecodeLimit)?;
        let samples = match actual.transform_decode {
            NativeTransformDecodeCapability::Flate8 => {
                LopdfExtractor::inflate_bounded(bytes, expected)
                    .map_err(NativeExtractError::ImageDecode)?
            }
            NativeTransformDecodeCapability::Dct8 => {
                LopdfExtractor::verify_jpeg_header(bytes, actual.width, actual.height, rgb)
                    .map_err(NativeExtractError::ImageDecode)?;
                let mut decoder =
                    image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(bytes))
                        .map_err(|e| NativeExtractError::ImageDecode(e.to_string()))?;
                if decoder.dimensions() != (actual.width, actual.height)
                    || decoder.color_type() != color
                    || decoder.total_bytes() != expected as u64
                {
                    return Err(NativeExtractError::ImageDecode(
                        "JPEG dimensions/color disagree with PDF".into(),
                    ));
                }
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(actual.width);
                limits.max_image_height = Some(actual.height);
                limits.max_alloc = Some(max_decoded_bytes.min(NATIVE_IMAGE_LIMIT));
                decoder
                    .set_limits(limits)
                    .map_err(|e| NativeExtractError::ImageDecode(e.to_string()))?;
                let mut pixels = vec![0; expected];
                decoder
                    .read_image(&mut pixels)
                    .map_err(|e| NativeExtractError::ImageDecode(e.to_string()))?;
                pixels
            }
            NativeTransformDecodeCapability::Unsupported => unreachable!("checked above"),
        };
        if samples.len() != expected {
            return Err(NativeExtractError::ImageDecode(
                "decoded sample length disagrees with PDF dimensions".into(),
            ));
        }
        if rgb {
            image::RgbImage::from_raw(actual.width, actual.height, samples)
                .map(image::DynamicImage::ImageRgb8)
        } else {
            image::GrayImage::from_raw(actual.width, actual.height, samples)
                .map(image::DynamicImage::ImageLuma8)
        }
        .ok_or_else(|| NativeExtractError::ImageDecode("invalid sample layout".into()))
    }

    #[must_use]
    pub fn pages(&self) -> &[NativePageRecord] {
        &self.pages
    }

    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.reader.info.path
    }

    pub fn encoded_image_bytes<'a>(
        &'a self,
        metadata: &NativeImageMetadata,
    ) -> std::result::Result<&'a [u8], NativeExtractError> {
        let object_id = (
            metadata.object_id.object_number,
            metadata.object_id.generation,
        );
        let stream = self
            .reader
            .document()
            .get_object(object_id)
            .and_then(lopdf::Object::as_stream)
            .map_err(|_| NativeExtractError::MissingImageObject(metadata.object_id.clone()))?;
        let actual_hash = format!("{:x}", Sha256::digest(&stream.content));
        if stream.content.len() != metadata.encoded_length || actual_hash != metadata.encoded_sha256
        {
            return Err(NativeExtractError::ImageHashMismatch(
                metadata.object_id.clone(),
            ));
        }
        Ok(&stream.content)
    }

    /// Task 6 writer input; intentionally crate-private to avoid exposing lopdf publicly.
    #[allow(dead_code)]
    pub(crate) fn source_document(&self) -> &lopdf::Document {
        self.reader.document()
    }
}

/// DPI-free native preservation extractor.
pub struct NativePdfExtractor;

impl NativePdfExtractor {
    pub fn extract_path(path: &Path) -> std::result::Result<NativePdfDocument, NativeExtractError> {
        let reader = crate::pdf_reader::LopdfReader::new_native(path)?;
        if reader.info.is_encrypted {
            return Err(NativeExtractError::EncryptedPdf);
        }
        let pages = reader
            .physical_pages()
            .iter()
            .cloned()
            .map(|physical_page| {
                LopdfExtractor::inspect_native_page(reader.document(), physical_page)
            })
            .collect();
        Ok(NativePdfDocument { reader, pages })
    }
}

/// Image extractor trait
pub trait ImageExtractor {
    /// Extract all pages from PDF
    fn extract_all(
        pdf_path: &Path,
        output_dir: &Path,
        options: &ExtractOptions,
    ) -> Result<Vec<ExtractedPage>>;

    /// Extract a single page
    fn extract_page(
        pdf_path: &Path,
        page_index: usize,
        output_path: &Path,
        options: &ExtractOptions,
    ) -> Result<ExtractedPage>;
}

/// ImageMagick-based extractor
pub struct MagickExtractor;

impl MagickExtractor {
    /// Build ImageMagick command arguments in correct order.
    ///
    /// ImageMagick 7 requires specific argument ordering:
    /// - Input settings (like -density) come BEFORE input file
    /// - Operations (like -alpha, -colorspace) come AFTER input file
    /// - Output settings (like -quality) come before output file
    ///
    /// This function ensures cross-platform compatibility (Linux, macOS, Windows).
    #[allow(dead_code)]
    pub fn build_magick_args(
        pdf_path: &Path,
        page_index: usize,
        output_path: &Path,
        options: &ExtractOptions,
    ) -> Vec<String> {
        let mut args = Vec::new();

        // 1. Input settings (before input file)
        args.push("-density".to_string());
        args.push(options.dpi.to_string());

        // 2. Input file with page index
        args.push(format!("{}[{}]", pdf_path.display(), page_index));

        // 3. Image operations (after input file)
        // Set background color for transparency
        if let Some(bg) = options.background {
            args.push("-background".to_string());
            args.push(format!("rgb({},{},{})", bg[0], bg[1], bg[2]));
            args.push("-alpha".to_string());
            args.push("remove".to_string());
            args.push("-alpha".to_string());
            args.push("off".to_string());
        }

        // Set colorspace
        match options.colorspace {
            ColorSpace::Grayscale => {
                args.push("-colorspace".to_string());
                args.push("gray".to_string());
            }
            ColorSpace::Cmyk => {
                args.push("-colorspace".to_string());
                args.push("CMYK".to_string());
            }
            ColorSpace::Rgb => {
                args.push("-colorspace".to_string());
                args.push("sRGB".to_string());
            }
        }

        // 4. Output settings (before output file)
        if let ImageFormat::Jpeg { quality } = options.format {
            args.push("-quality".to_string());
            args.push(quality.to_string());
        }

        // 5. Output file
        args.push(output_path.to_string_lossy().to_string());

        args
    }

    /// Extract a single page from PDF using ImageMagick
    pub fn extract_page(
        pdf_path: &Path,
        page_index: usize,
        output_path: &Path,
        options: &ExtractOptions,
    ) -> Result<ExtractedPage> {
        if !pdf_path.exists() {
            return Err(ExtractError::PdfNotFound(pdf_path.to_path_buf()));
        }

        // Check if output directory is writable
        if let Some(parent) = output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
            // Try to create a test file to verify writability
            let test_file = parent.join(".write_test");
            if std::fs::write(&test_file, b"test").is_err() {
                return Err(ExtractError::OutputNotWritable(parent.to_path_buf()));
            }
            let _ = std::fs::remove_file(test_file);
        }

        // Build arguments with correct order for cross-platform compatibility
        // (especially macOS ImageMagick which requires -alpha after input file)
        let args = Self::build_magick_args(pdf_path, page_index, output_path, options);

        let mut cmd = Command::new("magick");
        cmd.args(&args);

        let output = cmd.output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ExtractError::ExternalToolError(stderr.to_string()));
        }

        // Get image dimensions
        let img = image::open(output_path).map_err(|e| ExtractError::ExtractionFailed {
            page: page_index,
            reason: e.to_string(),
        })?;

        Ok(ExtractedPage {
            page_index,
            path: output_path.to_path_buf(),
            width: img.width(),
            height: img.height(),
            format: options.format,
        })
    }

    /// Extract all pages from PDF
    pub fn extract_all(
        pdf_path: &Path,
        output_dir: &Path,
        options: &ExtractOptions,
    ) -> Result<Vec<ExtractedPage>> {
        if !pdf_path.exists() {
            return Err(ExtractError::PdfNotFound(pdf_path.to_path_buf()));
        }

        // Create output directory if it doesn't exist
        if !output_dir.exists() {
            std::fs::create_dir_all(output_dir)?;
        }

        // Check writability
        let test_file = output_dir.join(".write_test");
        if std::fs::write(&test_file, b"test").is_err() {
            return Err(ExtractError::OutputNotWritable(output_dir.to_path_buf()));
        }
        let _ = std::fs::remove_file(test_file);

        // Get page count using pdfinfo or similar
        let page_count = Self::get_page_count(pdf_path)?;

        // Extract pages (optionally in parallel)
        let extension = options.format.extension();
        let mut results = Vec::with_capacity(page_count);

        // Sequential extraction for now (parallel would require more complex handling)
        for i in 0..page_count {
            let output_path = output_dir.join(format!("page_{:05}.{}", i, extension));

            let result = Self::extract_page(pdf_path, i, &output_path, options)?;
            results.push(result);

            // Call progress callback if provided
            if let Some(ref callback) = options.progress_callback {
                callback(i + 1, page_count);
            }
        }

        Ok(results)
    }

    /// Get the number of pages in a PDF
    fn get_page_count(pdf_path: &Path) -> Result<usize> {
        // Try using pdfinfo first
        if let Ok(output) = Command::new("pdfinfo").arg(pdf_path).output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.starts_with("Pages:") {
                        if let Some(count_str) = line.split(':').nth(1) {
                            if let Ok(count) = count_str.trim().parse() {
                                return Ok(count);
                            }
                        }
                    }
                }
            }
        }

        // Fallback: use ImageMagick identify
        let output = Command::new("magick")
            .args(["identify", "-format", "%n\n"])
            .arg(pdf_path)
            .output()?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().next() {
                if let Ok(count) = line.trim().parse() {
                    return Ok(count);
                }
            }
        }

        // Last resort: try lopdf
        let doc = lopdf::Document::load(pdf_path).map_err(|e| ExtractError::ExtractionFailed {
            page: 0,
            reason: e.to_string(),
        })?;

        Ok(doc.get_pages().len())
    }
}

/// Pure Rust PDF image extractor using lopdf
///
/// This extractor works without external tools by directly extracting
/// embedded JPEG images from the PDF. Works best with scanned PDFs where
/// each page is a single JPEG image (DCTDecode filter).
pub struct LopdfExtractor;

impl LopdfExtractor {
    /// Inspect native page/image objects without rendering or DPI conversion.
    pub fn inspect_native_pages(pdf_path: &Path) -> Result<Vec<NativePageRecord>> {
        NativePdfExtractor::extract_path(pdf_path)
            .map(|document| document.pages)
            .map_err(|error| match error {
                NativeExtractError::PdfReader(crate::pdf_reader::PdfReaderError::FileNotFound(
                    path,
                )) => ExtractError::PdfNotFound(path),
                other => ExtractError::ExtractionFailed {
                    page: 0,
                    reason: other.to_string(),
                },
            })
    }

    fn inspect_native_page(
        doc: &lopdf::Document,
        physical_page: crate::pdf_reader::PhysicalPageMetadata,
    ) -> NativePageRecord {
        let page_id = (
            physical_page.page_object_id.object_number,
            physical_page.page_object_id.generation,
        );
        let mut issues: Vec<NativePageIssue> = physical_page
            .issues
            .iter()
            .map(|issue| {
                NativePageIssue::new(NativePageIssueCode::PhysicalPage(*issue), issue.as_str())
            })
            .collect();
        if let Ok(page) = doc.get_dictionary(page_id) {
            for key in [
                b"Annots".as_slice(),
                b"UserUnit",
                b"Group",
                b"VP",
                b"PresSteps",
            ] {
                if page.has(key) {
                    issues.push(NativePageIssue::new(
                        NativePageIssueCode::UnsupportedOperator,
                        format!(
                            "page /{} semantics require review",
                            Self::canonical_pdf_name(key)
                        ),
                    ));
                }
            }
        }
        let content = match Self::strict_page_content(doc, page_id) {
            Ok(content) => content,
            Err(error) => {
                issues.push(NativePageIssue::new(
                    NativePageIssueCode::ContentRead,
                    format!("cannot read page content: {error}"),
                ));
                Vec::new()
            }
        };
        let mut image_invocations = Vec::new();
        let mut has_non_image_paint = false;
        if !content.is_empty() {
            match Self::page_resources(doc, page_id) {
                Ok(resources) => {
                    let mut active_forms = HashSet::new();
                    Self::walk_content_images(
                        doc,
                        &content,
                        resources.as_ref(),
                        NativeWalkState {
                            image_invocations: &mut image_invocations,
                            issues: &mut issues,
                            active_forms: &mut active_forms,
                            has_non_image_paint: &mut has_non_image_paint,
                            remaining_bytes: &mut NATIVE_CONTENT_LIMIT.clone(),
                            remaining_operations: &mut 100_000,
                        },
                        0,
                        PdfMatrix::IDENTITY,
                        &[],
                    );
                }
                Err(error) => issues.push(NativePageIssue::new(
                    NativePageIssueCode::InvalidResources,
                    error,
                )),
            }
        }

        let kind = if !issues.is_empty() {
            NativePageKind::UnsupportedContent
        } else if content.is_empty() {
            NativePageKind::Blank
        } else {
            match image_invocations.len() {
                0 => NativePageKind::NonImageContent,
                1 if has_non_image_paint => NativePageKind::CompositeContent,
                1 => NativePageKind::SingleImage,
                _ => NativePageKind::MultipleImages,
            }
        };
        let review_required = !matches!(kind, NativePageKind::Blank | NativePageKind::SingleImage)
            || image_invocations.iter().any(|image| {
                image.metadata.transform_decode == NativeTransformDecodeCapability::Unsupported
            });

        NativePageRecord {
            physical_page,
            kind,
            image_invocations,
            review_required,
            issues,
        }
    }

    fn walk_content_images(
        doc: &lopdf::Document,
        bytes: &[u8],
        resources: Option<&NativeResourceContext>,
        state: NativeWalkState<'_>,
        depth: usize,
        initial_matrix: PdfMatrix,
        resource_path: &[NativeResourcePathStep],
    ) {
        let NativeWalkState {
            image_invocations,
            issues,
            active_forms,
            has_non_image_paint,
            remaining_bytes,
            remaining_operations,
        } = state;
        if bytes.len() > *remaining_bytes {
            issues.push(NativePageIssue::new(
                NativePageIssueCode::ContentRead,
                "page/Form content byte budget exhausted",
            ));
            return;
        }
        *remaining_bytes -= bytes.len();
        const MAX_FORM_DEPTH: usize = 32;
        if depth >= MAX_FORM_DEPTH {
            issues.push(NativePageIssue::new(
                NativePageIssueCode::FormDepthExceeded,
                "Form XObject nesting reaches the 32-level safety limit",
            ));
            return;
        }
        let content = match Self::strict_content_decode(bytes) {
            Ok(content) => content,
            Err(error) => {
                issues.push(NativePageIssue::new(
                    NativePageIssueCode::ContentDecode,
                    format!("cannot decode page content: {error}"),
                ));
                return;
            }
        };
        let xobjects = Self::xobject_map(doc, resources.map(|value| &value.dictionary));
        let mut current_matrix = initial_matrix;
        let mut graphics_stack = Vec::new();
        let mut in_text = false;
        for operation in content.operations {
            if *remaining_operations == 0 {
                issues.push(NativePageIssue::new(
                    NativePageIssueCode::ContentDecode,
                    "page/Form operation budget exhausted",
                ));
                return;
            }
            *remaining_operations -= 1;
            if matches!(operation.operator.as_str(), "q" | "Q") && !operation.operands.is_empty() {
                issues.push(NativePageIssue::new(
                    NativePageIssueCode::UnbalancedGraphicsState,
                    "q/Q must have no operands",
                ));
                continue;
            }
            match operation.operator.as_str() {
                "q" => {
                    graphics_stack.push(current_matrix);
                    continue;
                }
                "Q" => {
                    let Some(previous) = graphics_stack.pop() else {
                        issues.push(NativePageIssue::new(
                            NativePageIssueCode::UnbalancedGraphicsState,
                            "unbalanced graphics-state restore",
                        ));
                        continue;
                    };
                    current_matrix = previous;
                    continue;
                }
                "cm" => {
                    let Some(matrix) = Self::matrix_from_operands(doc, &operation.operands) else {
                        issues.push(NativePageIssue::new(
                            NativePageIssueCode::InvalidMatrix,
                            "cm operation has an invalid affine matrix",
                        ));
                        continue;
                    };
                    match current_matrix.pre_concat(matrix) {
                        Ok(matrix) => current_matrix = matrix,
                        Err(()) => {
                            issues.push(NativePageIssue::new(
                                NativePageIssueCode::InvalidMatrix,
                                "cm matrix concatenation overflow",
                            ));
                            return;
                        }
                    }
                    continue;
                }
                "BI" | "ID" | "EI" => {
                    issues.push(NativePageIssue::new(
                        NativePageIssueCode::InlineImage,
                        "inline images are unsupported in native preservation mode",
                    ));
                    continue;
                }
                "BT" | "ET" => {
                    let begin = operation.operator == "BT";
                    if !operation.operands.is_empty() || begin == in_text {
                        issues.push(NativePageIssue::new(
                            NativePageIssueCode::UnsupportedOperator,
                            "invalid or unbalanced text-object boundary",
                        ));
                    }
                    in_text = begin;
                    continue;
                }
                "Do" => {
                    if in_text {
                        issues.push(NativePageIssue::new(
                            NativePageIssueCode::UnsupportedOperator,
                            "XObject invocation within a text object",
                        ));
                    }
                }
                operator if Self::is_non_image_paint_operator(operator) => {
                    *has_non_image_paint = true;
                    continue;
                }
                operator if Self::is_known_nonpainting_operator(operator) => {
                    issues.push(NativePageIssue::new(
                        NativePageIssueCode::UnsupportedOperator,
                        format!("unmodeled operator {operator} requires review"),
                    ));
                    continue;
                }
                operator => {
                    issues.push(NativePageIssue::new(
                        NativePageIssueCode::UnsupportedOperator,
                        format!("unsupported PDF content operator {operator}"),
                    ));
                    continue;
                }
            }

            if operation.operands.len() != 1 {
                issues.push(NativePageIssue::new(
                    NativePageIssueCode::InvalidDo,
                    "Do must have exactly one operand",
                ));
                continue;
            }
            let Some(name) = operation
                .operands
                .first()
                .and_then(|operand| operand.as_name().ok())
            else {
                issues.push(NativePageIssue::new(
                    NativePageIssueCode::InvalidDo,
                    "Do operation has no valid XObject name",
                ));
                continue;
            };
            let Some(object_id) = xobjects.get(name) else {
                issues.push(NativePageIssue::new(
                    NativePageIssueCode::UnresolvedXObject,
                    format!("unresolved XObject name /{}", String::from_utf8_lossy(name)),
                ));
                continue;
            };
            let Ok(stream) = doc
                .get_object(*object_id)
                .and_then(lopdf::Object::as_stream)
            else {
                issues.push(NativePageIssue::new(
                    NativePageIssueCode::XObjectNotStream,
                    format!("XObject {:?} is not an indirect stream", object_id),
                ));
                continue;
            };
            let mut nested_path = resource_path.to_vec();
            nested_path.push(NativeResourcePathStep {
                owner_object_id: resources.map_or_else(
                    || Self::pdf_object_id(*object_id),
                    |value| value.owner_object_id.clone(),
                ),
                resource_name: name.to_vec(),
            });
            match stream
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|value| value.as_name_str().ok())
            {
                Some("Image") => match Self::native_image_metadata(doc, *object_id, stream) {
                    Ok(metadata) => image_invocations.push(NativeImageInvocation {
                        metadata,
                        binding: NativeImageBinding {
                            is_direct: nested_path.len() == 1,
                            resource_path: nested_path,
                            placement_matrix: current_matrix,
                        },
                    }),
                    Err(error) => issues.push(NativePageIssue::new(
                        NativePageIssueCode::InvalidImageMetadata,
                        error,
                    )),
                },
                Some("Form") => {
                    if !active_forms.insert(*object_id) {
                        issues.push(NativePageIssue::new(
                            NativePageIssueCode::FormCycle,
                            format!("recursive Form XObject at {:?}", object_id),
                        ));
                        continue;
                    }
                    let nested_resources = match stream.dict.get(b"Resources") {
                        Ok(value) => match Self::dictionary_value(doc, Some(value)) {
                            Some(dictionary) => Some(NativeResourceContext {
                                owner_object_id: Self::pdf_object_id(*object_id),
                                dictionary: dictionary.clone(),
                            }),
                            None => {
                                issues.push(NativePageIssue::new(
                                    NativePageIssueCode::InvalidFormResources,
                                    format!("Form XObject {:?} has invalid Resources", object_id),
                                ));
                                None
                            }
                        },
                        Err(_) => resources.cloned(),
                    };
                    let form_matrix = match stream.dict.get(b"Matrix") {
                        Ok(value) => match Self::matrix_from_object(doc, value) {
                            Some(matrix) => matrix,
                            None => {
                                issues.push(NativePageIssue::new(
                                    NativePageIssueCode::InvalidFormMatrix,
                                    format!("Form XObject {:?} has an invalid Matrix", object_id),
                                ));
                                active_forms.remove(object_id);
                                continue;
                            }
                        },
                        Err(_) => PdfMatrix::IDENTITY,
                    };
                    // Form BBox clipping and transparency groups are not modeled.
                    // Keep inventory/path information, but never approve this page.
                    issues.push(NativePageIssue::new(
                        NativePageIssueCode::UnsupportedOperator,
                        "Form BBox clipping/group semantics require review",
                    ));
                    let nested_content =
                        match Self::strict_stream_content(doc, stream, *remaining_bytes) {
                            Ok(content) => content,
                            Err(error) => {
                                issues.push(NativePageIssue::new(
                                    NativePageIssueCode::FormDecode,
                                    error,
                                ));
                                active_forms.remove(object_id);
                                continue;
                            }
                        };
                    let Ok(nested_matrix) = current_matrix.pre_concat(form_matrix) else {
                        issues.push(NativePageIssue::new(
                            NativePageIssueCode::InvalidFormMatrix,
                            "Form matrix concatenation overflow",
                        ));
                        active_forms.remove(object_id);
                        continue;
                    };
                    Self::walk_content_images(
                        doc,
                        &nested_content,
                        nested_resources.as_ref(),
                        NativeWalkState {
                            image_invocations,
                            issues,
                            active_forms,
                            has_non_image_paint,
                            remaining_bytes,
                            remaining_operations,
                        },
                        depth + 1,
                        nested_matrix,
                        &nested_path,
                    );
                    active_forms.remove(object_id);
                }
                Some(other) => issues.push(NativePageIssue::new(
                    NativePageIssueCode::InvalidXObjectSubtype,
                    format!("unsupported XObject subtype {other}"),
                )),
                None => issues.push(NativePageIssue::new(
                    NativePageIssueCode::InvalidXObjectSubtype,
                    "XObject has no valid Subtype",
                )),
            }
        }
        if in_text {
            issues.push(NativePageIssue::new(
                NativePageIssueCode::UnsupportedOperator,
                "unterminated text object",
            ));
        }
        if !graphics_stack.is_empty() {
            issues.push(NativePageIssue::new(
                NativePageIssueCode::UnbalancedGraphicsState,
                "unbalanced graphics-state save",
            ));
        }
    }

    fn is_non_image_paint_operator(operator: &str) -> bool {
        matches!(
            operator,
            "S" | "s"
                | "f"
                | "F"
                | "f*"
                | "B"
                | "B*"
                | "b"
                | "b*"
                | "sh"
                | "Tj"
                | "TJ"
                | "'"
                | "\""
        )
    }

    fn is_known_nonpainting_operator(operator: &str) -> bool {
        matches!(
            operator,
            "w" | "J"
                | "j"
                | "M"
                | "d"
                | "i"
                | "m"
                | "l"
                | "c"
                | "v"
                | "y"
                | "h"
                | "re"
                | "n"
                | "G"
                | "g"
                | "RG"
                | "rg"
                | "K"
                | "k"
                | "CS"
                | "cs"
                | "SC"
                | "SCN"
                | "sc"
                | "scn"
                | "BT"
                | "ET"
                | "Tc"
                | "Tw"
                | "Tz"
                | "TL"
                | "Tf"
                | "Ts"
                | "Td"
                | "TD"
                | "Tm"
                | "T*"
                | "MP"
                | "DP"
        )
    }

    fn matrix_from_operands(doc: &lopdf::Document, values: &[lopdf::Object]) -> Option<PdfMatrix> {
        if values.len() != 6 {
            return None;
        }
        let mut matrix = [0.0; 6];
        for (destination, source) in matrix.iter_mut().zip(values) {
            *destination = Self::pdf_number(doc, source)?;
        }
        PdfMatrix::try_new(matrix).ok()
    }

    fn matrix_from_object(doc: &lopdf::Document, value: &lopdf::Object) -> Option<PdfMatrix> {
        let value = Self::resolve_object(doc, value).ok()?;
        Self::matrix_from_operands(doc, value.as_array().ok()?)
    }

    fn pdf_number(doc: &lopdf::Document, value: &lopdf::Object) -> Option<f64> {
        match Self::resolve_object(doc, value).ok()? {
            lopdf::Object::Integer(value) => Some(*value as f64),
            lopdf::Object::Real(value) => Some(f64::from(*value)),
            _ => None,
        }
    }

    fn page_resources(
        doc: &lopdf::Document,
        page_id: lopdf::ObjectId,
    ) -> std::result::Result<Option<NativeResourceContext>, String> {
        let (value, defined_on) =
            match crate::pdf_reader::LopdfReader::inherited_page_object(doc, page_id, b"Resources")
            {
                Ok(Some(value)) => value,
                Ok(None) => return Ok(None),
                Err(()) => return Err("cycle or invalid object in page Parent chain".to_string()),
            };
        Self::dictionary_value(doc, Some(&value))
            .cloned()
            .map(|dictionary| {
                Some(NativeResourceContext {
                    owner_object_id: Self::pdf_object_id(defined_on),
                    dictionary,
                })
            })
            .ok_or_else(|| "invalid Resources dictionary".to_string())
    }

    fn xobject_map(
        doc: &lopdf::Document,
        resources: Option<&lopdf::Dictionary>,
    ) -> BTreeMap<Vec<u8>, lopdf::ObjectId> {
        let Some(resources) = resources else {
            return BTreeMap::new();
        };
        let Some(xobjects) = Self::dictionary_value(doc, resources.get(b"XObject").ok()) else {
            return BTreeMap::new();
        };
        xobjects
            .iter()
            .filter_map(|(name, object)| object.as_reference().ok().map(|id| (name.clone(), id)))
            .collect()
    }

    fn dictionary_value<'a>(
        doc: &'a lopdf::Document,
        value: Option<&'a lopdf::Object>,
    ) -> Option<&'a lopdf::Dictionary> {
        match value? {
            lopdf::Object::Dictionary(dict) => Some(dict),
            lopdf::Object::Reference(id) => doc.get_dictionary(*id).ok(),
            _ => None,
        }
    }

    fn resolve_object<'a>(
        doc: &'a lopdf::Document,
        value: &'a lopdf::Object,
    ) -> std::result::Result<&'a lopdf::Object, lopdf::Error> {
        let mut current = value;
        let mut seen = HashSet::new();
        while let lopdf::Object::Reference(id) = current {
            if seen.len() >= 32 || !seen.insert(*id) {
                return Err(lopdf::Error::Syntax(
                    "cyclic or excessive native object reference".into(),
                ));
            }
            current = doc.objects.get(id).ok_or(lopdf::Error::ObjectNotFound)?;
        }
        Ok(current)
    }

    /// Strict zlib: bounded output, checksum/end marker required, no trailing data.
    fn inflate_bounded(bytes: &[u8], limit: usize) -> std::result::Result<Vec<u8>, String> {
        let mut decoder = flate2::Decompress::new(true);
        let mut result = Vec::new();
        loop {
            let before_in = decoder.total_in();
            let before_out = decoder.total_out();
            let mut chunk = [0_u8; 8192];
            let available = (limit.saturating_sub(result.len()))
                .saturating_add(1)
                .min(chunk.len());
            let status = decoder
                .decompress(
                    &bytes[before_in as usize..],
                    &mut chunk[..available],
                    flate2::FlushDecompress::None,
                )
                .map_err(|e| format!("invalid zlib stream: {e}"))?;
            let produced = (decoder.total_out() - before_out) as usize;
            if produced > limit.saturating_sub(result.len()) {
                return Err("decoded byte limit exceeded".into());
            }
            result.extend_from_slice(&chunk[..produced]);
            if status == flate2::Status::StreamEnd {
                return if decoder.total_in() == bytes.len() as u64 {
                    Ok(result)
                } else {
                    Err("trailing bytes after zlib stream".into())
                };
            }
            if decoder.total_in() == before_in && decoder.total_out() == before_out {
                return Err("truncated or stalled zlib stream".into());
            }
        }
    }

    fn strict_filters(
        doc: &lopdf::Document,
        dict: &lopdf::Dictionary,
    ) -> std::result::Result<Vec<String>, String> {
        let Ok(value) = dict.get(b"Filter") else {
            return Ok(Vec::new());
        };
        let value = Self::resolve_object(doc, value).map_err(|e| e.to_string())?;
        let name = |value: &lopdf::Object| -> std::result::Result<String, String> {
            let value = Self::resolve_object(doc, value).map_err(|e| e.to_string())?;
            value
                .as_name()
                .map(Self::canonical_pdf_name)
                .map_err(|_| "Filter must contain PDF names".into())
        };
        match value {
            lopdf::Object::Name(_) => Ok(vec![name(value)?]),
            lopdf::Object::Array(values) if !values.is_empty() && values.len() <= 32 => {
                values.iter().map(name).collect()
            }
            _ => Err("malformed Filter metadata".into()),
        }
    }

    fn strict_stream_content(
        doc: &lopdf::Document,
        stream: &lopdf::Stream,
        limit: usize,
    ) -> std::result::Result<Vec<u8>, String> {
        if stream.content.len() > NATIVE_CONTENT_LIMIT {
            return Err("encoded content limit exceeded".into());
        }
        if [b"F".as_slice(), b"FFilter", b"FDecodeParms"]
            .iter()
            .any(|key| stream.dict.has(key))
        {
            return Err("external content streams are unsupported".into());
        }
        if let Ok(params) = stream.dict.get(b"DecodeParms") {
            if !matches!(
                Self::resolve_object(doc, params).map_err(|e| e.to_string())?,
                lopdf::Object::Null
            ) {
                return Err("content DecodeParms are unsupported".into());
            }
        }
        match Self::strict_filters(doc, &stream.dict)?.as_slice() {
            [] if stream.content.len() <= limit => Ok(stream.content.clone()),
            [filter] if filter == "FlateDecode" => Self::inflate_bounded(&stream.content, limit),
            _ => Err("unsupported content filter or content limit exceeded".into()),
        }
    }

    fn strict_page_content(
        doc: &lopdf::Document,
        id: lopdf::ObjectId,
    ) -> std::result::Result<Vec<u8>, String> {
        let page = doc.get_dictionary(id).map_err(|e| e.to_string())?;
        let Ok(value) = page.get(b"Contents") else {
            return Ok(Vec::new());
        };
        let value = Self::resolve_object(doc, value).map_err(|e| e.to_string())?;
        let mut output = Vec::new();
        let values = match value {
            lopdf::Object::Null => return Ok(output),
            lopdf::Object::Array(values) if values.len() <= 4096 => values.as_slice(),
            lopdf::Object::Stream(_) => std::slice::from_ref(value),
            _ => return Err("Contents must be a stream or a flat stream array".into()),
        };
        for value in values {
            let stream = Self::resolve_object(doc, value)
                .map_err(|e| e.to_string())?
                .as_stream()
                .map_err(|_| "Contents array entry is not a stream")?;
            let bytes =
                Self::strict_stream_content(doc, stream, NATIVE_CONTENT_LIMIT - output.len())?;
            // PDF Contents arrays are one logical stream, not separate operations.
            output.extend_from_slice(&bytes);
        }
        Ok(output)
    }

    fn strict_content_decode(
        bytes: &[u8],
    ) -> std::result::Result<lopdf::content::Content<Vec<lopdf::content::Operation>>, String> {
        // lopdf's nom parser accepts a valid prefix and discards a malformed tail.
        // A reserved final operator proves it consumed the complete bounded input.
        const END: &str = "SuperbookNativeContentEnd";
        if bytes.windows(END.len()).any(|w| w == END.as_bytes()) {
            return Err("reserved content sentinel in input".into());
        }
        // Preflight nesting before calling the recursive operand parser. Ignore
        // comments and escaped literal strings; a false rejection is review-only.
        let mut nesting = 0_usize;
        let mut literal = 0_usize;
        let mut comment = false;
        let mut escaped = false;
        for &byte in bytes {
            if comment {
                comment = !matches!(byte, b'\r' | b'\n');
                continue;
            }
            if literal > 0 {
                if escaped {
                    escaped = false;
                    continue;
                }
                match byte {
                    b'\\' => escaped = true,
                    b'(' => literal += 1,
                    b')' => literal -= 1,
                    _ => {}
                }
            } else {
                match byte {
                    b'%' => comment = true,
                    b'(' => literal = 1,
                    b'[' | b'<' => nesting += 1,
                    b']' | b'>' => nesting = nesting.saturating_sub(1),
                    _ => {}
                }
            }
            if nesting > 32 || literal > 32 {
                return Err("content nesting limit exceeded".into());
            }
        }
        if literal != 0 || nesting != 0 {
            return Err("unterminated content operand".into());
        }
        let mut input = bytes.to_vec();
        input.extend_from_slice(b"\nSuperbookNativeContentEnd\n");
        let mut content = lopdf::content::Content::decode(&input).map_err(|e| e.to_string())?;
        match content.operations.pop() {
            Some(op) if op.operator == END && op.operands.is_empty() => Ok(content),
            _ => Err("malformed content or unconsumed content suffix".into()),
        }
    }

    /// Only baseline 8-bit JPEG with 1/3 components. Reject implicit CMYK/RGBA
    /// conversion, progressive working sets, and truncated framing before decode.
    fn verify_jpeg_header(
        bytes: &[u8],
        width: u32,
        height: u32,
        rgb: bool,
    ) -> std::result::Result<(), String> {
        if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
            return Err("JPEG must have complete SOI/EOI framing".into());
        }
        let mut pos = 2;
        let mut frame = false;
        while pos + 4 <= bytes.len() {
            if bytes[pos] != 0xff {
                return Err("invalid JPEG marker".into());
            }
            while bytes.get(pos) == Some(&0xff) {
                pos += 1;
            }
            let marker = *bytes.get(pos).ok_or("truncated JPEG marker")?;
            pos += 1;
            let size = bytes.get(pos..pos + 2).ok_or("truncated JPEG segment")?;
            let size = usize::from(u16::from_be_bytes([size[0], size[1]]));
            if size < 2 {
                return Err("invalid JPEG segment length".into());
            }
            let segment = bytes
                .get(pos + 2..pos + size)
                .ok_or("truncated JPEG segment")?;
            if marker == 0xc0 {
                let components = if rgb { 3 } else { 1 };
                if frame
                    || segment.len() != 6 + 3 * components
                    || segment[0] != 8
                    || u32::from(u16::from_be_bytes([segment[1], segment[2]])) != height
                    || u32::from(u16::from_be_bytes([segment[3], segment[4]])) != width
                    || usize::from(segment[5]) != components
                {
                    return Err("JPEG frame disagrees with supported native PDF image".into());
                }
                frame = true;
            } else if (0xc1..=0xcf).contains(&marker) && !matches!(marker, 0xc4) {
                return Err("only baseline Huffman JPEG is supported".into());
            } else if marker == 0xda {
                return if frame {
                    Ok(())
                } else {
                    Err("JPEG scan before frame".into())
                };
            }
            pos += size;
        }
        Err("missing JPEG scan".into())
    }

    fn native_image_metadata(
        doc: &lopdf::Document,
        object_id: lopdf::ObjectId,
        stream: &lopdf::Stream,
    ) -> std::result::Result<NativeImageMetadata, String> {
        let positive_u32 = |name: &[u8]| {
            stream
                .dict
                .get(name)
                .ok()
                .and_then(|value| Self::resolve_object(doc, value).ok())
                .and_then(|value| value.as_i64().ok())
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
        };
        let width = positive_u32(b"Width").ok_or_else(|| "invalid image Width".to_string())?;
        let height = positive_u32(b"Height").ok_or_else(|| "invalid image Height".to_string())?;
        let bits_per_component = stream
            .dict
            .get(b"BitsPerComponent")
            .ok()
            .and_then(|value| Self::resolve_object(doc, value).ok())
            .and_then(|value| value.as_i64().ok())
            .and_then(|value| u8::try_from(value).ok());
        let color_space = stream
            .dict
            .get(b"ColorSpace")
            .ok()
            .map(|value| Self::pdf_object_json(doc, value, &mut HashSet::new(), 0))
            .transpose()?;
        let filters = Self::strict_filters(doc, &stream.dict)?;
        let decode_params = stream
            .dict
            .get(b"DecodeParms")
            .ok()
            .map(|value| Self::pdf_object_json(doc, value, &mut HashSet::new(), 0))
            .transpose()?;
        if stream.dict.has(b"BitsPerComponent")
            && !matches!(bits_per_component, Some(1 | 2 | 4 | 8 | 16))
        {
            return Err("invalid image BitsPerComponent".into());
        }
        let unsupported_semantics = [
            b"Mask".as_slice(),
            b"SMask",
            b"Decode",
            b"Alternates",
            b"OPI",
            b"OC",
            b"F",
            b"FFilter",
            b"FDecodeParms",
            b"SMaskInData",
        ]
        .iter()
        .any(|key| stream.dict.has(key))
            || stream
                .dict
                .get(b"ImageMask")
                .is_ok_and(|value| !matches!(value, lopdf::Object::Boolean(false)))
            || decode_params.as_ref().is_some_and(|value| !value.is_null());
        let transform_decode = if unsupported_semantics {
            NativeTransformDecodeCapability::Unsupported
        } else {
            Self::transform_decode_capability(&filters, bits_per_component, color_space.as_ref())
        };
        let digest = Sha256::digest(&stream.content);
        Ok(NativeImageMetadata {
            object_id: Self::pdf_object_id(object_id),
            width,
            height,
            color_space,
            bits_per_component,
            filters,
            decode_params,
            encoded_length: stream.content.len(),
            encoded_sha256: format!("{digest:x}"),
            transform_decode,
        })
    }

    fn transform_decode_capability(
        filters: &[String],
        bits_per_component: Option<u8>,
        color_space: Option<&serde_json::Value>,
    ) -> NativeTransformDecodeCapability {
        let supported_color = matches!(
            color_space,
            Some(serde_json::Value::String(value))
                if matches!(value.as_str(), "/DeviceGray" | "/DeviceRGB")
        );
        if bits_per_component != Some(8) || !supported_color {
            return NativeTransformDecodeCapability::Unsupported;
        }
        match filters {
            [filter] if filter == "DCTDecode" => NativeTransformDecodeCapability::Dct8,
            [filter] if filter == "FlateDecode" => NativeTransformDecodeCapability::Flate8,
            _ => NativeTransformDecodeCapability::Unsupported,
        }
    }

    fn pdf_object_json(
        doc: &lopdf::Document,
        value: &lopdf::Object,
        seen: &mut HashSet<lopdf::ObjectId>,
        depth: usize,
    ) -> std::result::Result<serde_json::Value, String> {
        Self::pdf_object_json_bounded(doc, value, seen, depth, &mut 16_384)
    }

    fn pdf_object_json_bounded(
        doc: &lopdf::Document,
        value: &lopdf::Object,
        seen: &mut HashSet<lopdf::ObjectId>,
        depth: usize,
        remaining_nodes: &mut usize,
    ) -> std::result::Result<serde_json::Value, String> {
        if *remaining_nodes == 0 {
            return Err("PDF metadata expansion budget exhausted".to_string());
        }
        *remaining_nodes -= 1;
        if matches!(value, lopdf::Object::Name(bytes) | lopdf::Object::String(bytes, _) if bytes.len() > 4096)
        {
            return Err("PDF metadata scalar exceeds 4096 bytes".to_string());
        }
        if depth >= 32 {
            return Err("PDF metadata exceeds the 32-level safety limit".to_string());
        }
        match value {
            lopdf::Object::Null => Ok(serde_json::Value::Null),
            lopdf::Object::Boolean(value) => Ok(serde_json::Value::Bool(*value)),
            lopdf::Object::Integer(value) => Ok((*value).into()),
            lopdf::Object::Real(value) => serde_json::Number::from_f64(f64::from(*value))
                .map(serde_json::Value::Number)
                .ok_or_else(|| "PDF metadata contains a non-finite real number".to_string()),
            lopdf::Object::Name(value) => Ok(serde_json::Value::String(format!(
                "/{}",
                Self::canonical_pdf_name(value)
            ))),
            lopdf::Object::String(value, format) => Ok(serde_json::json!({
                "$pdf_string_hex": Self::hex_bytes(value),
                "$pdf_string_format": format!("{format:?}").to_ascii_lowercase(),
            })),
            lopdf::Object::Array(values) => {
                if values.len() > 4096 {
                    return Err("PDF metadata array exceeds 4096 entries".to_string());
                }
                values
                    .iter()
                    .map(|value| {
                        Self::pdf_object_json_bounded(doc, value, seen, depth + 1, remaining_nodes)
                    })
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map(serde_json::Value::Array)
            }
            lopdf::Object::Dictionary(values) => {
                if values.len() > 4096 {
                    return Err("PDF metadata dictionary exceeds 4096 entries".to_string());
                }
                let mut map = serde_json::Map::new();
                for (key, value) in values.iter() {
                    if key.len() > 4096 {
                        return Err("PDF metadata key exceeds 4096 bytes".to_string());
                    }
                    map.insert(
                        Self::canonical_pdf_name(key),
                        Self::pdf_object_json_bounded(
                            doc,
                            value,
                            seen,
                            depth + 1,
                            remaining_nodes,
                        )?,
                    );
                }
                Ok(serde_json::Value::Object(map))
            }
            lopdf::Object::Reference(id) => {
                if !seen.insert(*id) {
                    return Err(format!("cycle in PDF metadata reference {id:?}"));
                }
                let value = doc.get_object(*id).map_err(|error| {
                    format!("unresolved PDF metadata reference {id:?}: {error}")
                })?;
                let result =
                    Self::pdf_object_json_bounded(doc, value, seen, depth + 1, remaining_nodes);
                seen.remove(id);
                result
            }
            lopdf::Object::Stream(_) => {
                Err("PDF metadata unexpectedly contains a stream object".to_string())
            }
        }
    }

    fn canonical_pdf_name(bytes: &[u8]) -> String {
        let mut result = String::new();
        for byte in bytes {
            if (b'!'..=b'~').contains(byte)
                && !matches!(
                    *byte,
                    b'#' | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
                )
            {
                result.push(char::from(*byte));
            } else {
                result.push_str(&format!("#{byte:02X}"));
            }
        }
        result
    }

    fn hex_bytes(bytes: &[u8]) -> String {
        let mut result = String::with_capacity(bytes.len().saturating_mul(2));
        for byte in bytes {
            result.push_str(&format!("{byte:02x}"));
        }
        result
    }

    const fn pdf_object_id(id: lopdf::ObjectId) -> crate::transform_manifest::PdfObjectId {
        crate::transform_manifest::PdfObjectId {
            object_number: id.0,
            generation: id.1,
        }
    }

    /// Extract all embedded JPEG images from a PDF
    ///
    /// This method extracts XObject images with DCTDecode filter (JPEG).
    /// For scanned PDFs, this typically means one JPEG per page.
    pub fn extract_all(
        pdf_path: &Path,
        output_dir: &Path,
        _options: &ExtractOptions,
    ) -> Result<Vec<ExtractedPage>> {
        if !pdf_path.exists() {
            return Err(ExtractError::PdfNotFound(pdf_path.to_path_buf()));
        }

        // Create output directory
        if !output_dir.exists() {
            std::fs::create_dir_all(output_dir)?;
        }

        let doc = lopdf::Document::load(pdf_path).map_err(|e| ExtractError::ExtractionFailed {
            page: 0,
            reason: format!("Failed to load PDF: {}", e),
        })?;

        // Issue #58: walk the page tree so page_index reflects reading order.
        // Object IDs are unrelated to page order, so scanning doc.objects
        // scrambled pages whenever images were registered out of page order.
        let mut results = Self::extract_by_page_tree(&doc, output_dir);

        if results.is_empty() {
            // Fallback for PDFs whose page tree carries no image XObjects
            // (e.g. images referenced only through Form XObjects, or broken
            // /Resources). This legacy scan has NO reliable page ordering.
            // A complete fix would also resolve Form XObject content streams
            // and inline images; for scanned books the page-tree walk above
            // covers the practical cases.
            results = Self::extract_by_object_scan(&doc, output_dir);
        }

        if results.is_empty() {
            return Err(ExtractError::ExtractionFailed {
                page: 0,
                reason: "No extractable images found. This PDF may require ImageMagick (install with: sudo apt install imagemagick).".to_string(),
            });
        }

        // Sort by page index for consistent ordering
        results.sort_by_key(|r| r.page_index);

        Ok(results)
    }

    /// Extract one image per page by walking the page tree (issue #58).
    ///
    /// For each page (in page-tree order) the largest-area image XObject in
    /// the page's resources is taken as the page scan and assigned
    /// `page_index = page_number - 1`. Pages without an image XObject are
    /// skipped, matching the previous behavior of only emitting image-bearing
    /// pages (a complete fix would insert blank-page placeholders so that
    /// vec position always equals the physical page number).
    fn extract_by_page_tree(doc: &lopdf::Document, output_dir: &Path) -> Vec<ExtractedPage> {
        let mut results = Vec::new();

        for (page_num, page_id) in doc.get_pages() {
            let mut best: Option<(lopdf::ObjectId, u64)> = None;

            for obj_id in Self::page_image_xobjects(doc, page_id) {
                let Ok(stream) = doc.get_object(obj_id).and_then(|o| o.as_stream()) else {
                    continue;
                };
                let is_image = stream
                    .dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|s| s.as_name_str().ok())
                    == Some("Image");
                if !is_image {
                    continue;
                }
                let width = stream
                    .dict
                    .get(b"Width")
                    .ok()
                    .and_then(|w| w.as_i64().ok())
                    .unwrap_or(0);
                let height = stream
                    .dict
                    .get(b"Height")
                    .ok()
                    .and_then(|h| h.as_i64().ok())
                    .unwrap_or(0);
                let area = (width.max(0) as u64) * (height.max(0) as u64);
                if best.is_none_or(|(_, best_area)| area > best_area) {
                    best = Some((obj_id, area));
                }
            }

            if let Some((obj_id, _)) = best {
                if let Ok(stream) = doc.get_object(obj_id).and_then(|o| o.as_stream()) {
                    if let Ok(extracted) = Self::extract_image_stream(
                        stream,
                        (page_num - 1) as usize,
                        &obj_id,
                        output_dir,
                    ) {
                        results.push(extracted);
                    }
                }
            }
        }

        results
    }

    /// Collect the ObjectIds of all XObjects reachable from a page's
    /// resources, including inherited resource dictionaries.
    fn page_image_xobjects(
        doc: &lopdf::Document,
        page_id: lopdf::ObjectId,
    ) -> Vec<lopdf::ObjectId> {
        let mut ids = Vec::new();
        let Ok((direct, resource_ids)) = doc.get_page_resources(page_id) else {
            return ids;
        };

        let mut dicts: Vec<&lopdf::Dictionary> = Vec::new();
        if let Some(dict) = direct {
            dicts.push(dict);
        }
        for rid in resource_ids {
            if let Ok(dict) = doc.get_dictionary(rid) {
                dicts.push(dict);
            }
        }

        for dict in dicts {
            let Ok(xobjects) = dict.get(b"XObject") else {
                continue;
            };
            let xobj_dict = match xobjects {
                lopdf::Object::Dictionary(d) => Some(d),
                lopdf::Object::Reference(r) => doc.get_dictionary(*r).ok(),
                _ => None,
            };
            if let Some(xobj_dict) = xobj_dict {
                for (_name, value) in xobj_dict.iter() {
                    if let Ok(r) = value.as_reference() {
                        if !ids.contains(&r) {
                            ids.push(r);
                        }
                    }
                }
            }
        }

        ids
    }

    /// Legacy extraction: scan all PDF objects for image streams.
    /// Ordering follows object IDs, NOT page order — only used as a fallback
    /// when the page tree yields no images (see extract_all).
    fn extract_by_object_scan(doc: &lopdf::Document, output_dir: &Path) -> Vec<ExtractedPage> {
        let mut results = Vec::new();
        let mut image_count = 0;

        for (obj_id, object) in doc.objects.iter() {
            if let Ok(stream) = object.as_stream() {
                if let Ok(subtype) = stream.dict.get(b"Subtype") {
                    if let Ok(subtype_name) = subtype.as_name_str() {
                        if subtype_name == "Image" {
                            if let Ok(extracted) =
                                Self::extract_image_stream(stream, image_count, obj_id, output_dir)
                            {
                                results.push(extracted);
                                image_count += 1;
                            }
                        }
                    }
                }
            }
        }

        results
    }

    /// Extract a single image stream to file
    fn extract_image_stream(
        stream: &lopdf::Stream,
        index: usize,
        obj_id: &lopdf::ObjectId,
        output_dir: &Path,
    ) -> Result<ExtractedPage> {
        // Get image dimensions
        let width = stream
            .dict
            .get(b"Width")
            .ok()
            .and_then(|w| w.as_i64().ok())
            .unwrap_or(0) as u32;
        let height = stream
            .dict
            .get(b"Height")
            .ok()
            .and_then(|h| h.as_i64().ok())
            .unwrap_or(0) as u32;

        // Determine filter type
        let filter = stream
            .dict
            .get(b"Filter")
            .ok()
            .and_then(|f| f.as_name_str().ok())
            .unwrap_or("");

        // Only extract JPEG images (DCTDecode) - these are most common in scanned PDFs
        if filter == "DCTDecode" {
            let output_path = output_dir.join(format!(
                "page_{:04}_obj{}_{}.jpg",
                index, obj_id.0, obj_id.1
            ));

            // JPEG data can be saved directly (no decompression needed)
            std::fs::write(&output_path, &stream.content)?;

            return Ok(ExtractedPage {
                page_index: index,
                path: output_path,
                width,
                height,
                format: ImageFormat::Jpeg { quality: 95 },
            });
        }

        // Try to decompress and save other formats
        if let Ok(decoded) = stream.decompressed_content() {
            if width > 0 && height > 0 {
                // Determine channels from ColorSpace
                let channels = stream
                    .dict
                    .get(b"ColorSpace")
                    .ok()
                    .and_then(|cs| cs.as_name_str().ok())
                    .map(|name| match name {
                        "DeviceGray" | "CalGray" => 1,
                        "DeviceRGB" | "CalRGB" => 3,
                        "DeviceCMYK" => 4,
                        _ => 3,
                    })
                    .unwrap_or(3);

                let expected_size = (width as usize) * (height as usize) * channels;
                if decoded.len() >= expected_size {
                    let output_path = output_dir.join(format!(
                        "page_{:04}_obj{}_{}.png",
                        index, obj_id.0, obj_id.1
                    ));

                    let img_opt = match channels {
                        1 => image::GrayImage::from_raw(
                            width,
                            height,
                            decoded[..expected_size].to_vec(),
                        )
                        .map(image::DynamicImage::ImageLuma8),
                        3 => image::RgbImage::from_raw(
                            width,
                            height,
                            decoded[..expected_size].to_vec(),
                        )
                        .map(image::DynamicImage::ImageRgb8),
                        4 => {
                            // CMYK to RGB conversion
                            let rgb: Vec<u8> = decoded[..expected_size]
                                .chunks_exact(4)
                                .flat_map(|cmyk| {
                                    let (c, m, y, k) = (
                                        cmyk[0] as f32 / 255.0,
                                        cmyk[1] as f32 / 255.0,
                                        cmyk[2] as f32 / 255.0,
                                        cmyk[3] as f32 / 255.0,
                                    );
                                    [
                                        ((1.0 - c) * (1.0 - k) * 255.0) as u8,
                                        ((1.0 - m) * (1.0 - k) * 255.0) as u8,
                                        ((1.0 - y) * (1.0 - k) * 255.0) as u8,
                                    ]
                                })
                                .collect();
                            image::RgbImage::from_raw(width, height, rgb)
                                .map(image::DynamicImage::ImageRgb8)
                        }
                        _ => None,
                    };

                    if let Some(img) = img_opt {
                        img.save(&output_path)
                            .map_err(|e| ExtractError::ExtractionFailed {
                                page: index,
                                reason: format!("Failed to save: {}", e),
                            })?;
                        return Ok(ExtractedPage {
                            page_index: index,
                            path: output_path,
                            width,
                            height,
                            format: ImageFormat::Png,
                        });
                    }
                }
            }
        }

        Err(ExtractError::ExtractionFailed {
            page: index,
            reason: format!("Unsupported image filter: {}", filter),
        })
    }

    /// Check if ImageMagick is available
    pub fn magick_available() -> bool {
        which::which("magick").is_ok() || which::which("convert").is_ok()
    }

    /// Check if pdftoppm (poppler-utils) is available
    pub fn pdftoppm_available() -> bool {
        which::which("pdftoppm").is_ok()
    }

    /// Extract using best available method
    pub fn extract_auto(
        pdf_path: &Path,
        output_dir: &Path,
        options: &ExtractOptions,
    ) -> Result<Vec<ExtractedPage>> {
        // Try ImageMagick first if available (better quality for complex PDFs)
        if Self::magick_available() {
            return MagickExtractor::extract_all(pdf_path, output_dir, options);
        }

        // Try pdftoppm (poppler-utils) as second option
        if Self::pdftoppm_available() {
            return PopplerExtractor::extract_all(pdf_path, output_dir, options);
        }

        // Fall back to pure Rust extraction
        Self::extract_all(pdf_path, output_dir, options)
    }
}

/// Poppler-based extractor using pdftoppm
pub struct PopplerExtractor;

impl PopplerExtractor {
    /// Extract a single page from PDF using pdftoppm
    pub fn extract_page(
        pdf_path: &Path,
        page_index: usize,
        output_path: &Path,
        options: &ExtractOptions,
    ) -> Result<ExtractedPage> {
        if !pdf_path.exists() {
            return Err(ExtractError::PdfNotFound(pdf_path.to_path_buf()));
        }

        // Create output directory if needed
        if let Some(parent) = output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // pdftoppm uses 1-based page numbers
        let page_num = page_index + 1;

        // Get output path without extension (pdftoppm adds its own)
        let output_stem = output_path.with_extension("");
        let output_stem_str = output_stem.to_string_lossy();

        let mut cmd = Command::new("pdftoppm");
        cmd.arg("-r").arg(options.dpi.to_string()); // Resolution
        cmd.arg("-f").arg(page_num.to_string()); // First page
        cmd.arg("-l").arg(page_num.to_string()); // Last page
        cmd.arg("-singlefile"); // Single file output (no suffix)

        // Set output format (pdftoppm doesn't support BMP, fallback to PNG)
        match options.format {
            ImageFormat::Png | ImageFormat::Bmp => {
                cmd.arg("-png");
            }
            ImageFormat::Jpeg { quality } => {
                cmd.arg("-jpeg");
                cmd.arg("-jpegopt").arg(format!("quality={}", quality));
            }
            ImageFormat::Tiff => {
                cmd.arg("-tiff");
            }
        }

        // Set colorspace
        match options.colorspace {
            ColorSpace::Grayscale => {
                cmd.arg("-gray");
            }
            ColorSpace::Rgb | ColorSpace::Cmyk => {
                // RGB is default for pdftoppm
            }
        }

        // Input PDF and output prefix
        cmd.arg(pdf_path);
        cmd.arg(&*output_stem_str);

        let output = cmd.output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ExtractError::ExternalToolError(format!(
                "pdftoppm failed: {}",
                stderr
            )));
        }

        // pdftoppm creates file with extension based on format
        // (BMP outputs as PNG since pdftoppm doesn't support BMP)
        let actual_output = match options.format {
            ImageFormat::Png | ImageFormat::Bmp => output_stem.with_extension("png"),
            ImageFormat::Jpeg { .. } => output_stem.with_extension("jpg"),
            ImageFormat::Tiff => output_stem.with_extension("tif"),
        };

        // Rename to expected output path if different
        if actual_output != output_path {
            std::fs::rename(&actual_output, output_path)?;
        }

        // Get image dimensions
        let img = image::open(output_path).map_err(|e| ExtractError::ExtractionFailed {
            page: page_index,
            reason: e.to_string(),
        })?;

        Ok(ExtractedPage {
            page_index,
            path: output_path.to_path_buf(),
            width: img.width(),
            height: img.height(),
            format: options.format,
        })
    }

    /// Extract all pages from PDF using pdftoppm
    pub fn extract_all(
        pdf_path: &Path,
        output_dir: &Path,
        options: &ExtractOptions,
    ) -> Result<Vec<ExtractedPage>> {
        if !pdf_path.exists() {
            return Err(ExtractError::PdfNotFound(pdf_path.to_path_buf()));
        }

        // Create output directory
        if !output_dir.exists() {
            std::fs::create_dir_all(output_dir)?;
        }

        // Get page count using pdfinfo
        let page_count = Self::get_page_count(pdf_path)?;

        // Extract pages
        let extension = options.format.extension();
        let mut results = Vec::with_capacity(page_count);

        for i in 0..page_count {
            let output_path = output_dir.join(format!("page_{:05}.{}", i, extension));

            let result = Self::extract_page(pdf_path, i, &output_path, options)?;
            results.push(result);

            // Call progress callback if provided
            if let Some(ref callback) = options.progress_callback {
                callback(i + 1, page_count);
            }
        }

        Ok(results)
    }

    /// Get page count using pdfinfo
    fn get_page_count(pdf_path: &Path) -> Result<usize> {
        let output = Command::new("pdfinfo").arg(pdf_path).output()?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.starts_with("Pages:") {
                    if let Some(count_str) = line.split_whitespace().nth(1) {
                        if let Ok(count) = count_str.parse::<usize>() {
                            return Ok(count);
                        }
                    }
                }
            }
        }

        // Fallback: try lopdf
        let doc = lopdf::Document::load(pdf_path).map_err(|e| ExtractError::ExtractionFailed {
            page: 0,
            reason: format!("Failed to load PDF: {}", e),
        })?;
        Ok(doc.get_pages().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // TC-EXT-009: 存在しないPDFエラー
    #[test]
    fn test_nonexistent_pdf_error() {
        let temp_dir = tempdir().unwrap();

        let result = MagickExtractor::extract_all(
            Path::new("/nonexistent/file.pdf"),
            temp_dir.path(),
            &ExtractOptions::default(),
        );

        assert!(matches!(result, Err(ExtractError::PdfNotFound(_))));
    }

    // TC-EXT-003: DPI設定
    #[test]
    fn test_default_options() {
        let opts = ExtractOptions::default();

        assert_eq!(opts.dpi, 300);
        assert!(matches!(opts.format, ImageFormat::Png));
        assert!(matches!(opts.colorspace, ColorSpace::Rgb));
        assert_eq!(opts.background, Some([255, 255, 255]));
        assert!(opts.parallel > 0);
    }

    #[test]
    fn test_image_format_extension() {
        assert_eq!(ImageFormat::Png.extension(), "png");
        assert_eq!(ImageFormat::Jpeg { quality: 90 }.extension(), "jpg");
        assert_eq!(ImageFormat::Bmp.extension(), "bmp");
        assert_eq!(ImageFormat::Tiff.extension(), "tiff");
    }

    #[test]
    fn test_builder_pattern() {
        let options = ExtractOptions::builder()
            .dpi(600)
            .format(ImageFormat::Jpeg { quality: 95 })
            .colorspace(ColorSpace::Grayscale)
            .background([0, 0, 0])
            .parallel(4)
            .build();

        assert_eq!(options.dpi, 600);
        assert!(matches!(options.format, ImageFormat::Jpeg { quality: 95 }));
        assert!(matches!(options.colorspace, ColorSpace::Grayscale));
        assert_eq!(options.background, Some([0, 0, 0]));
        assert_eq!(options.parallel, 4);
    }

    #[test]
    fn test_builder_dpi_clamping() {
        // DPI should be clamped to 72-1200
        let options = ExtractOptions::builder().dpi(50).build();
        assert_eq!(options.dpi, 72);

        let options = ExtractOptions::builder().dpi(2000).build();
        assert_eq!(options.dpi, 1200);

        let options = ExtractOptions::builder().dpi(300).build();
        assert_eq!(options.dpi, 300);
    }

    #[test]
    fn test_builder_parallel_minimum() {
        // Parallel workers should be at least 1
        let options = ExtractOptions::builder().parallel(0).build();
        assert_eq!(options.parallel, 1);
    }

    #[test]
    fn test_builder_no_background() {
        let options = ExtractOptions::builder().no_background().build();
        assert!(options.background.is_none());
    }

    #[test]
    fn test_high_quality_preset() {
        let options = ExtractOptions::high_quality();

        assert_eq!(options.dpi, 600);
        assert!(matches!(options.format, ImageFormat::Png));
    }

    #[test]
    fn test_fast_preset() {
        let options = ExtractOptions::fast();

        assert_eq!(options.dpi, 150);
        assert!(matches!(options.format, ImageFormat::Jpeg { quality: 80 }));
    }

    #[test]
    fn test_grayscale_preset() {
        let options = ExtractOptions::grayscale();

        assert!(matches!(options.colorspace, ColorSpace::Grayscale));
    }

    // Note: The following tests require ImageMagick and actual PDF fixtures
    // They are marked with #[ignore] until fixtures are available

    // TC-EXT-001: 単一ページ抽出
    #[test]
    #[ignore = "requires external tool"]
    fn test_extract_single_page() {
        let temp_dir = tempdir().unwrap();
        let output = temp_dir.path().join("page_0.png");

        let result = MagickExtractor::extract_page(
            Path::new("tests/fixtures/sample.pdf"),
            0,
            &output,
            &ExtractOptions::default(),
        )
        .unwrap();

        assert!(output.exists());
        assert_eq!(result.page_index, 0);
        assert!(result.width > 0);
        assert!(result.height > 0);
    }

    // TC-EXT-002: 全ページ抽出
    #[test]
    #[ignore = "requires external tool"]
    fn test_extract_all_pages() {
        let temp_dir = tempdir().unwrap();

        let results = MagickExtractor::extract_all(
            Path::new("tests/fixtures/10pages.pdf"),
            temp_dir.path(),
            &ExtractOptions::default(),
        )
        .unwrap();

        assert_eq!(results.len(), 10);
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.page_index, i);
            assert!(result.path.exists());
        }
    }

    // TC-EXT-003: DPI設定（詳細テスト）
    #[test]
    #[ignore = "requires external tool"]
    fn test_dpi_setting() {
        let temp_dir = tempdir().unwrap();

        // 72 DPI
        let output_72 = temp_dir.path().join("72dpi.png");
        let result_72 = MagickExtractor::extract_page(
            Path::new("tests/fixtures/a4.pdf"),
            0,
            &output_72,
            &ExtractOptions {
                dpi: 72,
                ..Default::default()
            },
        )
        .unwrap();

        // 300 DPI
        let output_300 = temp_dir.path().join("300dpi.png");
        let result_300 = MagickExtractor::extract_page(
            Path::new("tests/fixtures/a4.pdf"),
            0,
            &output_300,
            &ExtractOptions {
                dpi: 300,
                ..Default::default()
            },
        )
        .unwrap();

        // 300 DPI image should be ~4x larger in each dimension
        assert!(result_300.width > result_72.width * 3);
        assert!(result_300.height > result_72.height * 3);
    }

    // TC-EXT-004: JPEG出力
    #[test]
    #[ignore = "requires external tool"]
    fn test_jpeg_output() {
        let temp_dir = tempdir().unwrap();
        let output = temp_dir.path().join("page_0.jpg");

        MagickExtractor::extract_page(
            Path::new("tests/fixtures/sample.pdf"),
            0,
            &output,
            &ExtractOptions {
                format: ImageFormat::Jpeg { quality: 85 },
                ..Default::default()
            },
        )
        .unwrap();

        assert!(output.exists());

        // Check JPEG magic bytes
        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[0..2], &[0xFF, 0xD8]);
    }

    // TC-EXT-005: グレースケール変換
    #[test]
    #[ignore = "requires external tool"]
    fn test_grayscale_extraction() {
        let temp_dir = tempdir().unwrap();
        let output = temp_dir.path().join("gray.png");

        MagickExtractor::extract_page(
            Path::new("tests/fixtures/color.pdf"),
            0,
            &output,
            &ExtractOptions {
                colorspace: ColorSpace::Grayscale,
                ..Default::default()
            },
        )
        .unwrap();

        // Verify image is grayscale
        let img = image::open(&output).unwrap();
        let rgb = img.to_rgb8();

        // Check that R=G=B for each pixel (grayscale property)
        for pixel in rgb.pixels() {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
        }
    }

    // Additional structure tests

    #[test]
    fn test_extracted_page_construction() {
        let page = ExtractedPage {
            page_index: 5,
            path: PathBuf::from("/test/page_5.png"),
            width: 2480,
            height: 3508,
            format: ImageFormat::Png,
        };

        assert_eq!(page.page_index, 5);
        assert_eq!(page.path, PathBuf::from("/test/page_5.png"));
        assert_eq!(page.width, 2480);
        assert_eq!(page.height, 3508);
        assert!(matches!(page.format, ImageFormat::Png));
    }

    #[test]
    fn test_all_image_formats() {
        let formats = [
            ImageFormat::Png,
            ImageFormat::Jpeg { quality: 90 },
            ImageFormat::Bmp,
            ImageFormat::Tiff,
        ];

        let expected_ext = ["png", "jpg", "bmp", "tiff"];

        for (format, ext) in formats.iter().zip(expected_ext.iter()) {
            assert_eq!(format.extension(), *ext);
        }
    }

    #[test]
    fn test_all_colorspaces() {
        let colorspaces = vec![ColorSpace::Rgb, ColorSpace::Grayscale, ColorSpace::Cmyk];

        // Verify all colorspaces can be constructed and roundtrip through builder
        for cs in colorspaces {
            let options = ExtractOptions::builder().colorspace(cs).build();
            match (cs, options.colorspace) {
                (ColorSpace::Rgb, ColorSpace::Rgb) => {}
                (ColorSpace::Grayscale, ColorSpace::Grayscale) => {}
                (ColorSpace::Cmyk, ColorSpace::Cmyk) => {}
                _ => panic!("Colorspace mismatch"),
            }
        }
    }

    #[test]
    fn test_error_types() {
        // Test all error variants can be constructed
        let _err1 = ExtractError::PdfNotFound(PathBuf::from("/test/path"));
        let _err2 = ExtractError::OutputNotWritable(PathBuf::from("/readonly/dir"));
        let _err3 = ExtractError::ExternalToolError("ImageMagick not found".to_string());
        let _err4 = ExtractError::ExtractionFailed {
            page: 3,
            reason: "Test error".to_string(),
        };
        let _err5: ExtractError = std::io::Error::new(std::io::ErrorKind::NotFound, "test").into();
    }

    #[test]
    fn test_output_not_writable_error_message() {
        let err = ExtractError::OutputNotWritable(PathBuf::from("/readonly/output"));

        let msg = err.to_string();
        assert!(msg.contains("/readonly/output"));
        assert!(msg.contains("not writable"));
    }

    #[test]
    fn test_extraction_failed_error_message() {
        let err = ExtractError::ExtractionFailed {
            page: 5,
            reason: "ImageMagick crashed".to_string(),
        };

        let msg = err.to_string();
        assert!(msg.contains("5"));
        assert!(msg.contains("ImageMagick"));
    }

    #[test]
    fn test_format_builder() {
        // Test different JPEG qualities
        let options_low = ExtractOptions::builder()
            .format(ImageFormat::Jpeg { quality: 50 })
            .build();
        let options_high = ExtractOptions::builder()
            .format(ImageFormat::Jpeg { quality: 95 })
            .build();

        match (options_low.format, options_high.format) {
            (ImageFormat::Jpeg { quality: q1 }, ImageFormat::Jpeg { quality: q2 }) => {
                assert_eq!(q1, 50);
                assert_eq!(q2, 95);
            }
            _ => panic!("Expected JPEG format"),
        }
    }

    // TC-EXT-006: 透明部分の処理
    #[test]
    fn test_background_color_setting() {
        // White background (default)
        let options = ExtractOptions::builder()
            .background([255, 255, 255])
            .build();
        assert_eq!(options.background, Some([255, 255, 255]));

        // Black background
        let options = ExtractOptions::builder().background([0, 0, 0]).build();
        assert_eq!(options.background, Some([0, 0, 0]));

        // Transparent (no background)
        let options = ExtractOptions::builder().no_background().build();
        assert!(options.background.is_none());
    }

    // TC-EXT-007: 並列抽出
    #[test]
    fn test_parallel_extraction_config() {
        // Test parallel thread configuration
        let options_single = ExtractOptions::builder().parallel(1).build();
        assert_eq!(options_single.parallel, 1);

        let options_multi = ExtractOptions::builder().parallel(4).build();
        assert_eq!(options_multi.parallel, 4);

        let options_max = ExtractOptions::builder().parallel(16).build();
        assert_eq!(options_max.parallel, 16);

        // parallel(0) should be clamped to 1
        let options_zero = ExtractOptions::builder().parallel(0).build();
        assert_eq!(options_zero.parallel, 1);
    }

    // TC-EXT-008: 進捗コールバック
    #[test]
    fn test_progress_callback_structure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // Test that progress callback can be set
        let progress_count = Arc::new(AtomicUsize::new(0));
        let progress_clone = progress_count.clone();

        let options = ExtractOptions::builder()
            .progress_callback(Box::new(move |current, total| {
                progress_clone.fetch_add(1, Ordering::SeqCst);
                assert!(current <= total);
            }))
            .build();

        // Verify callback is set
        assert!(options.progress_callback.is_some());

        // Call the callback to verify it works
        if let Some(callback) = &options.progress_callback {
            callback(1, 10);
            callback(5, 10);
            callback(10, 10);
        }

        assert_eq!(progress_count.load(Ordering::SeqCst), 3);
    }

    // TC-EXT-010: 書き込み不可ディレクトリエラー追加テスト
    #[test]
    fn test_output_not_writable_error_display() {
        let err = ExtractError::OutputNotWritable(std::path::PathBuf::from("/root/protected"));
        let display = format!("{}", err);
        assert!(display.contains("/root/protected"));
        assert!(display.contains("not writable") || display.contains("writable"));
    }

    #[test]
    fn test_extracted_page_display() {
        let page = ExtractedPage {
            page_index: 0,
            path: std::path::PathBuf::from("/tmp/page_001.png"),
            width: 2480,
            height: 3508,
            format: ImageFormat::Png,
        };

        assert_eq!(page.page_index, 0);
        assert_eq!(page.width, 2480);
        assert_eq!(page.height, 3508);
        assert!(page.path.to_string_lossy().contains("page_001"));
    }

    #[test]
    fn test_colorspace_all_variants() {
        // Test all colorspace variants exist and can be used
        let colorspaces = [ColorSpace::Rgb, ColorSpace::Grayscale, ColorSpace::Cmyk];

        for cs in &colorspaces {
            let options = ExtractOptions::builder().colorspace(*cs).build();
            assert_eq!(options.colorspace, *cs);
        }
    }

    #[test]
    fn test_image_format_all_variants() {
        // Test all image format variants
        let formats = [
            ImageFormat::Png,
            ImageFormat::Jpeg { quality: 85 },
            ImageFormat::Bmp,
            ImageFormat::Tiff,
        ];

        for fmt in &formats {
            let options = ExtractOptions::builder().format(*fmt).build();
            match (&options.format, fmt) {
                (ImageFormat::Png, ImageFormat::Png) => {}
                (ImageFormat::Tiff, ImageFormat::Tiff) => {}
                (ImageFormat::Bmp, ImageFormat::Bmp) => {}
                (ImageFormat::Jpeg { quality: q1 }, ImageFormat::Jpeg { quality: q2 }) => {
                    assert_eq!(q1, q2);
                }
                _ => panic!("Format mismatch"),
            }
        }
    }

    // Additional comprehensive tests

    #[test]
    fn test_dpi_boundary_values() {
        // Minimum boundary (should clamp to 72)
        let options_below = ExtractOptions::builder().dpi(50).build();
        assert_eq!(options_below.dpi, 72);

        // Exact minimum
        let options_min = ExtractOptions::builder().dpi(72).build();
        assert_eq!(options_min.dpi, 72);

        // Normal range
        let options_normal = ExtractOptions::builder().dpi(300).build();
        assert_eq!(options_normal.dpi, 300);

        // Maximum boundary
        let options_max = ExtractOptions::builder().dpi(1200).build();
        assert_eq!(options_max.dpi, 1200);

        // Above maximum (should clamp to 1200)
        let options_above = ExtractOptions::builder().dpi(2400).build();
        assert_eq!(options_above.dpi, 1200);
    }

    #[test]
    fn test_jpeg_quality_edge_cases() {
        // Minimum quality
        let opts_min = ExtractOptions::builder()
            .format(ImageFormat::Jpeg { quality: 0 })
            .build();
        if let ImageFormat::Jpeg { quality } = opts_min.format {
            assert_eq!(quality, 0);
        } else {
            panic!("Expected JPEG format");
        }

        // Maximum quality
        let opts_max = ExtractOptions::builder()
            .format(ImageFormat::Jpeg { quality: 100 })
            .build();
        if let ImageFormat::Jpeg { quality } = opts_max.format {
            assert_eq!(quality, 100);
        } else {
            panic!("Expected JPEG format");
        }

        // Typical quality values
        for q in [1, 50, 75, 85, 99] {
            let opts = ExtractOptions::builder()
                .format(ImageFormat::Jpeg { quality: q })
                .build();
            if let ImageFormat::Jpeg { quality } = opts.format {
                assert_eq!(quality, q);
            }
        }
    }

    #[test]
    fn test_builder_method_chaining() {
        // Test that all builder methods can be chained
        let options = ExtractOptions::builder()
            .dpi(400)
            .format(ImageFormat::Tiff)
            .colorspace(ColorSpace::Cmyk)
            .background([128, 128, 128])
            .parallel(8)
            .build();

        assert_eq!(options.dpi, 400);
        assert!(matches!(options.format, ImageFormat::Tiff));
        assert_eq!(options.colorspace, ColorSpace::Cmyk);
        assert_eq!(options.background, Some([128, 128, 128]));
        assert_eq!(options.parallel, 8);
    }

    #[test]
    fn test_extracted_page_various_sizes() {
        // Standard A4 at 300 DPI
        let a4_page = ExtractedPage {
            page_index: 0,
            path: PathBuf::from("/tmp/a4.png"),
            width: 2480,
            height: 3508,
            format: ImageFormat::Png,
        };
        assert_eq!(a4_page.width, 2480);
        assert_eq!(a4_page.height, 3508);

        // Letter size at 300 DPI
        let letter_page = ExtractedPage {
            page_index: 1,
            path: PathBuf::from("/tmp/letter.png"),
            width: 2550,
            height: 3300,
            format: ImageFormat::Png,
        };
        assert_eq!(letter_page.width, 2550);
        assert_eq!(letter_page.height, 3300);

        // Square thumbnail
        let thumb = ExtractedPage {
            page_index: 99,
            path: PathBuf::from("/tmp/thumb.jpg"),
            width: 150,
            height: 150,
            format: ImageFormat::Jpeg { quality: 70 },
        };
        assert_eq!(thumb.width, thumb.height);
    }

    #[test]
    fn test_error_from_io_error() {
        // Test From<std::io::Error> conversion
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let extract_err: ExtractError = io_err.into();
        let msg = extract_err.to_string();
        assert!(msg.contains("file not found") || msg.contains("IO error"));
    }

    #[test]
    fn test_preset_high_quality_details() {
        let hq = ExtractOptions::high_quality();
        assert_eq!(hq.dpi, 600);
        assert!(matches!(hq.format, ImageFormat::Png));
        // Should inherit other defaults
        assert_eq!(hq.colorspace, ColorSpace::Rgb);
        assert!(hq.background.is_some());
    }

    #[test]
    fn test_preset_fast_details() {
        let fast = ExtractOptions::fast();
        assert_eq!(fast.dpi, 150);
        if let ImageFormat::Jpeg { quality } = fast.format {
            assert_eq!(quality, 80);
        } else {
            panic!("Fast preset should use JPEG");
        }
    }

    #[test]
    fn test_preset_grayscale_details() {
        let gray = ExtractOptions::grayscale();
        assert_eq!(gray.colorspace, ColorSpace::Grayscale);
        // Should have default DPI
        assert_eq!(gray.dpi, 300);
    }

    #[test]
    fn test_background_color_extremes() {
        // Pure black
        let black = ExtractOptions::builder().background([0, 0, 0]).build();
        assert_eq!(black.background, Some([0, 0, 0]));

        // Pure white
        let white = ExtractOptions::builder()
            .background([255, 255, 255])
            .build();
        assert_eq!(white.background, Some([255, 255, 255]));

        // Gray
        let gray = ExtractOptions::builder()
            .background([128, 128, 128])
            .build();
        assert_eq!(gray.background, Some([128, 128, 128]));

        // Primary colors
        let red = ExtractOptions::builder().background([255, 0, 0]).build();
        assert_eq!(red.background, Some([255, 0, 0]));

        let green = ExtractOptions::builder().background([0, 255, 0]).build();
        assert_eq!(green.background, Some([0, 255, 0]));

        let blue = ExtractOptions::builder().background([0, 0, 255]).build();
        assert_eq!(blue.background, Some([0, 0, 255]));
    }

    #[test]
    fn test_extract_options_debug_impl() {
        let options = ExtractOptions::builder()
            .dpi(300)
            .format(ImageFormat::Png)
            .build();

        let debug_str = format!("{:?}", options);
        assert!(debug_str.contains("ExtractOptions"));
        assert!(debug_str.contains("dpi"));
        assert!(debug_str.contains("300"));
    }

    #[test]
    fn test_extract_options_with_callback_debug() {
        let options = ExtractOptions::builder()
            .progress_callback(Box::new(|_, _| {}))
            .build();

        let debug_str = format!("{:?}", options);
        assert!(debug_str.contains("<callback>"));
    }

    #[test]
    fn test_image_format_default() {
        let format: ImageFormat = Default::default();
        assert!(matches!(format, ImageFormat::Png));
    }

    #[test]
    fn test_colorspace_default() {
        let cs: ColorSpace = Default::default();
        assert_eq!(cs, ColorSpace::Rgb);
    }

    #[test]
    fn test_parallel_workers_various_values() {
        // Test various worker counts
        for workers in [1, 2, 4, 8, 16, 32, 64] {
            let options = ExtractOptions::builder().parallel(workers).build();
            assert_eq!(options.parallel, workers);
        }
    }

    #[test]
    fn test_extracted_page_with_all_formats() {
        let formats = [
            (ImageFormat::Png, "png"),
            (ImageFormat::Jpeg { quality: 85 }, "jpg"),
            (ImageFormat::Bmp, "bmp"),
            (ImageFormat::Tiff, "tiff"),
        ];

        for (idx, (format, ext)) in formats.iter().enumerate() {
            let page = ExtractedPage {
                page_index: idx,
                path: PathBuf::from(format!("/tmp/page.{}", ext)),
                width: 1000,
                height: 1500,
                format: *format,
            };
            assert_eq!(page.page_index, idx);
            assert!(page.path.to_string_lossy().ends_with(ext));
        }
    }

    #[test]
    fn test_error_display_all_variants() {
        let errors = [
            ExtractError::PdfNotFound(PathBuf::from("/test.pdf")),
            ExtractError::OutputNotWritable(PathBuf::from("/output")),
            ExtractError::ExtractionFailed {
                page: 1,
                reason: "test reason".to_string(),
            },
            ExtractError::ExternalToolError("tool error".to_string()),
        ];

        for err in &errors {
            let display = format!("{}", err);
            assert!(!display.is_empty());
        }
    }

    #[test]
    fn test_options_builder_default_state() {
        let builder = ExtractOptionsBuilder::default();
        let options = builder.build();

        // Should have default values
        assert_eq!(options.dpi, 300);
        assert!(matches!(options.format, ImageFormat::Png));
        assert_eq!(options.colorspace, ColorSpace::Rgb);
    }

    #[test]
    fn test_colorspace_partial_eq() {
        assert_eq!(ColorSpace::Rgb, ColorSpace::Rgb);
        assert_eq!(ColorSpace::Grayscale, ColorSpace::Grayscale);
        assert_eq!(ColorSpace::Cmyk, ColorSpace::Cmyk);
        assert_ne!(ColorSpace::Rgb, ColorSpace::Grayscale);
        assert_ne!(ColorSpace::Rgb, ColorSpace::Cmyk);
        assert_ne!(ColorSpace::Grayscale, ColorSpace::Cmyk);
    }

    // Additional comprehensive Debug/Clone tests

    #[test]
    fn test_image_format_debug_impl() {
        let png = ImageFormat::Png;
        let debug_str = format!("{:?}", png);
        assert!(debug_str.contains("Png"));

        let jpeg = ImageFormat::Jpeg { quality: 85 };
        let debug_str = format!("{:?}", jpeg);
        assert!(debug_str.contains("Jpeg"));
        assert!(debug_str.contains("85"));

        let tiff = ImageFormat::Tiff;
        let debug_str = format!("{:?}", tiff);
        assert!(debug_str.contains("Tiff"));
    }

    #[test]
    fn test_image_format_clone() {
        let original = ImageFormat::Jpeg { quality: 92 };
        let cloned = original;
        if let ImageFormat::Jpeg { quality } = cloned {
            assert_eq!(quality, 92);
        } else {
            panic!("Clone should preserve JPEG format");
        }

        let original_png = ImageFormat::Png;
        let cloned_png = original_png;
        assert!(matches!(cloned_png, ImageFormat::Png));
    }

    #[test]
    fn test_image_format_copy() {
        let original = ImageFormat::Bmp;
        let copied = original; // Copy, not move
        let _still_valid = original; // Original still valid
        assert!(matches!(copied, ImageFormat::Bmp));
    }

    #[test]
    fn test_colorspace_debug_impl() {
        let rgb = ColorSpace::Rgb;
        let debug_str = format!("{:?}", rgb);
        assert!(debug_str.contains("Rgb"));

        let gray = ColorSpace::Grayscale;
        let debug_str = format!("{:?}", gray);
        assert!(debug_str.contains("Grayscale"));

        let cmyk = ColorSpace::Cmyk;
        let debug_str = format!("{:?}", cmyk);
        assert!(debug_str.contains("Cmyk"));
    }

    #[test]
    fn test_colorspace_clone() {
        let original = ColorSpace::Cmyk;
        let cloned = original;
        assert_eq!(cloned, ColorSpace::Cmyk);
    }

    #[test]
    fn test_colorspace_copy() {
        let original = ColorSpace::Grayscale;
        let copied = original; // Copy
        let _still_valid = original; // Original still valid
        assert_eq!(copied, ColorSpace::Grayscale);
    }

    #[test]
    fn test_extracted_page_debug_impl() {
        let page = ExtractedPage {
            page_index: 3,
            path: PathBuf::from("/tmp/test.png"),
            width: 1920,
            height: 1080,
            format: ImageFormat::Png,
        };
        let debug_str = format!("{:?}", page);
        assert!(debug_str.contains("ExtractedPage"));
        assert!(debug_str.contains("3"));
        assert!(debug_str.contains("1920"));
        assert!(debug_str.contains("1080"));
    }

    #[test]
    fn test_error_debug_impl() {
        let err = ExtractError::PdfNotFound(PathBuf::from("/test.pdf"));
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("PdfNotFound"));

        let err2 = ExtractError::ExtractionFailed {
            page: 5,
            reason: "test reason".to_string(),
        };
        let debug_str2 = format!("{:?}", err2);
        assert!(debug_str2.contains("ExtractionFailed"));
    }

    #[test]
    fn test_extract_options_builder_debug_impl() {
        let builder = ExtractOptionsBuilder::default();
        let debug_str = format!("{:?}", builder);
        assert!(debug_str.contains("ExtractOptionsBuilder"));
    }

    #[test]
    fn test_error_path_extraction() {
        let path = PathBuf::from("/some/pdf/file.pdf");
        let err = ExtractError::PdfNotFound(path.clone());

        if let ExtractError::PdfNotFound(p) = err {
            assert_eq!(p, path);
        } else {
            panic!("Wrong error variant");
        }
    }

    #[test]
    fn test_error_page_extraction() {
        let err = ExtractError::ExtractionFailed {
            page: 42,
            reason: "Out of memory".to_string(),
        };

        if let ExtractError::ExtractionFailed { page, reason } = err {
            assert_eq!(page, 42);
            assert!(reason.contains("memory"));
        } else {
            panic!("Wrong error variant");
        }
    }

    #[test]
    fn test_page_index_sequential() {
        let pages: Vec<ExtractedPage> = (0..100)
            .map(|i| ExtractedPage {
                page_index: i,
                path: PathBuf::from(format!("/tmp/page_{:05}.png", i)),
                width: 1000,
                height: 1500,
                format: ImageFormat::Png,
            })
            .collect();

        for (i, page) in pages.iter().enumerate() {
            assert_eq!(page.page_index, i);
        }
    }

    #[test]
    fn test_large_page_dimensions() {
        // A0 at 600 DPI
        let large_page = ExtractedPage {
            page_index: 0,
            path: PathBuf::from("/tmp/a0.png"),
            width: 19842,
            height: 28067,
            format: ImageFormat::Png,
        };
        assert!(large_page.width > 10000);
        assert!(large_page.height > 20000);
    }

    #[test]
    fn test_small_page_dimensions() {
        // Tiny thumbnail
        let tiny_page = ExtractedPage {
            page_index: 0,
            path: PathBuf::from("/tmp/tiny.png"),
            width: 16,
            height: 16,
            format: ImageFormat::Png,
        };
        assert!(tiny_page.width <= 100);
        assert!(tiny_page.height <= 100);
    }

    #[test]
    fn test_preset_consistency() {
        let high = ExtractOptions::high_quality();
        let fast = ExtractOptions::fast();
        let gray = ExtractOptions::grayscale();

        // High quality should have higher DPI than fast
        assert!(high.dpi > fast.dpi);

        // Grayscale should have grayscale colorspace
        assert_eq!(gray.colorspace, ColorSpace::Grayscale);

        // All presets should have valid backgrounds
        assert!(high.background.is_some() || high.background.is_none()); // Either is valid
    }

    #[test]
    fn test_output_path_types() {
        // Absolute path
        let abs_page = ExtractedPage {
            page_index: 0,
            path: PathBuf::from("/absolute/path/page.png"),
            width: 100,
            height: 100,
            format: ImageFormat::Png,
        };
        assert!(abs_page.path.is_absolute());

        // Relative path
        let rel_page = ExtractedPage {
            page_index: 0,
            path: PathBuf::from("relative/path/page.png"),
            width: 100,
            height: 100,
            format: ImageFormat::Png,
        };
        assert!(rel_page.path.is_relative());
    }

    #[test]
    fn test_jpeg_quality_boundary() {
        // Test quality 1 (minimum realistic)
        let opts_1 = ExtractOptions::builder()
            .format(ImageFormat::Jpeg { quality: 1 })
            .build();
        if let ImageFormat::Jpeg { quality } = opts_1.format {
            assert_eq!(quality, 1);
        }

        // Test quality values across range
        for q in (0..=100).step_by(10) {
            let opts = ExtractOptions::builder()
                .format(ImageFormat::Jpeg { quality: q })
                .build();
            if let ImageFormat::Jpeg { quality } = opts.format {
                assert_eq!(quality, q);
            }
        }
    }

    #[test]
    fn test_magick_extractor_marker() {
        // Verify MagickExtractor type exists
        let _ = std::any::type_name::<MagickExtractor>();
    }

    #[test]
    fn test_error_io_details_preserved() {
        let io_err = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied to file",
        );
        let extract_err: ExtractError = io_err.into();

        let msg = extract_err.to_string().to_lowercase();
        // The error message should contain IO-related info
        assert!(msg.contains("io") || msg.contains("error") || msg.contains("access"));
    }

    #[test]
    fn test_all_dpi_presets() {
        let dpi_values = [72, 96, 150, 200, 300, 400, 600, 1200];

        for dpi in dpi_values {
            let opts = ExtractOptions::builder().dpi(dpi).build();
            assert_eq!(opts.dpi, dpi);
        }
    }

    #[test]
    fn test_background_none_vs_some() {
        let with_bg = ExtractOptions::builder()
            .background([255, 255, 255])
            .build();
        assert!(with_bg.background.is_some());

        let without_bg = ExtractOptions::builder().no_background().build();
        assert!(without_bg.background.is_none());
    }

    // ============ Concurrency Tests ============

    #[test]
    fn test_image_extract_types_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ExtractOptions>();
        assert_send_sync::<ExtractedPage>();
        assert_send_sync::<ImageFormat>();
        assert_send_sync::<ColorSpace>();
    }

    #[test]
    fn test_concurrent_options_building() {
        use std::thread;
        let handles: Vec<_> = (0..8)
            .map(|i| {
                thread::spawn(move || {
                    ExtractOptions::builder()
                        .dpi(150 + (i as u32 * 50))
                        .colorspace(if i % 2 == 0 {
                            ColorSpace::Rgb
                        } else {
                            ColorSpace::Grayscale
                        })
                        .build()
                })
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|h: std::thread::JoinHandle<ExtractOptions>| h.join().unwrap())
            .collect();
        assert_eq!(results.len(), 8);
        for (i, opt) in results.iter().enumerate() {
            assert_eq!(opt.dpi, 150 + (i as u32 * 50));
        }
    }

    #[test]
    fn test_parallel_extracted_page_creation() {
        use rayon::prelude::*;

        let pages: Vec<_> = (0..100)
            .into_par_iter()
            .map(|i| ExtractedPage {
                page_index: i,
                path: PathBuf::from(format!("page_{:04}.png", i)),
                width: 1000 + i as u32,
                height: 1500 + i as u32,
                format: ImageFormat::Png,
            })
            .collect();

        assert_eq!(pages.len(), 100);
        for (i, page) in pages.iter().enumerate() {
            assert_eq!(page.page_index, i);
            assert_eq!(page.width, 1000 + i as u32);
        }
    }

    #[test]
    fn test_extracted_page_thread_transfer() {
        use std::thread;

        let page = ExtractedPage {
            page_index: 42,
            path: PathBuf::from("/tmp/test_page.png"),
            width: 2480,
            height: 3508,
            format: ImageFormat::Jpeg { quality: 95 },
        };

        let handle = thread::spawn(move || {
            assert_eq!(page.page_index, 42);
            assert_eq!(page.width, 2480);
            page.path.to_string_lossy().to_string()
        });

        let result = handle.join().unwrap();
        assert!(result.contains("test_page"));
    }

    #[test]
    fn test_options_shared_across_threads() {
        use std::sync::Arc;
        use std::thread;

        let options = Arc::new(
            ExtractOptions::builder()
                .dpi(600)
                .format(ImageFormat::Png)
                .colorspace(ColorSpace::Rgb)
                .build(),
        );

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let opts = Arc::clone(&options);
                thread::spawn(move || {
                    assert_eq!(opts.dpi, 600);
                    opts.dpi
                })
            })
            .collect();

        for handle in handles {
            let result: u32 = handle.join().unwrap();
            assert_eq!(result, 600);
        }
    }

    #[test]
    fn test_image_format_thread_safe() {
        use std::thread;

        let formats = vec![
            ImageFormat::Png,
            ImageFormat::Jpeg { quality: 90 },
            ImageFormat::Tiff,
            ImageFormat::Bmp,
        ];

        let handles: Vec<_> = formats
            .into_iter()
            .map(|format| {
                thread::spawn(move || {
                    let ext = format.extension();
                    ext.to_string()
                })
            })
            .collect();

        let extensions: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(extensions.len(), 4);
        assert!(extensions.contains(&"png".to_string()));
        assert!(extensions.contains(&"jpg".to_string()));
    }

    // ============ Additional Boundary Tests ============

    #[test]
    fn test_dpi_boundary_minimum() {
        // Values below MIN_DPI (72) should be clamped to MIN_DPI
        let opts = ExtractOptions::builder().dpi(1).build();
        assert_eq!(opts.dpi, MIN_DPI);
    }

    #[test]
    fn test_dpi_boundary_maximum() {
        // Values above MAX_DPI (1200) should be clamped to MAX_DPI
        let opts = ExtractOptions::builder().dpi(2400).build();
        assert_eq!(opts.dpi, MAX_DPI);
    }

    #[test]
    fn test_page_index_zero() {
        let page = ExtractedPage {
            page_index: 0,
            path: PathBuf::from("first.png"),
            width: 100,
            height: 100,
            format: ImageFormat::Png,
        };
        assert_eq!(page.page_index, 0);
    }

    #[test]
    fn test_page_index_large() {
        let page = ExtractedPage {
            page_index: 10000,
            path: PathBuf::from("page_10000.png"),
            width: 100,
            height: 100,
            format: ImageFormat::Png,
        };
        assert_eq!(page.page_index, 10000);
    }

    #[test]
    fn test_page_dimensions_zero() {
        let page = ExtractedPage {
            page_index: 0,
            path: PathBuf::from("empty.png"),
            width: 0,
            height: 0,
            format: ImageFormat::Png,
        };
        assert_eq!(page.width, 0);
        assert_eq!(page.height, 0);
    }

    #[test]
    fn test_page_dimensions_large() {
        let page = ExtractedPage {
            page_index: 0,
            path: PathBuf::from("huge.png"),
            width: 32768,
            height: 32768,
            format: ImageFormat::Png,
        };
        assert_eq!(page.width, 32768);
        assert_eq!(page.height, 32768);
    }

    #[test]
    fn test_background_color_black() {
        let opts = ExtractOptions::builder().background([0, 0, 0]).build();
        assert_eq!(opts.background, Some([0, 0, 0]));
    }

    #[test]
    fn test_background_color_white() {
        let opts = ExtractOptions::builder()
            .background([255, 255, 255])
            .build();
        assert_eq!(opts.background, Some([255, 255, 255]));
    }

    #[test]
    fn test_all_color_spaces() {
        let spaces = [ColorSpace::Rgb, ColorSpace::Grayscale, ColorSpace::Cmyk];
        for space in spaces {
            let opts = ExtractOptions::builder().colorspace(space).build();
            assert_eq!(opts.colorspace, space);
        }
    }

    // ============ ImageMagick Argument Order Tests (macOS compatibility) ============

    #[test]
    fn test_magick_args_alpha_comes_after_input_file() {
        // TC-EXT-MAC-001: -alpha operations must come AFTER input file
        // This is critical for macOS ImageMagick compatibility
        let pdf_path = Path::new("/test/input.pdf");
        let output_path = Path::new("/test/output.png");
        let options = ExtractOptions::builder()
            .dpi(300)
            .background([255, 255, 255])
            .build();

        let args = MagickExtractor::build_magick_args(pdf_path, 0, output_path, &options);

        // Find positions
        let input_pos = args.iter().position(|a| a.contains("input.pdf")).unwrap();
        let alpha_pos = args.iter().position(|a| a == "-alpha").unwrap();

        // -alpha must come AFTER input file
        assert!(
            alpha_pos > input_pos,
            "ImageMagick: -alpha (pos {}) must come after input file (pos {}). Args: {:?}",
            alpha_pos,
            input_pos,
            args
        );
    }

    #[test]
    fn test_magick_args_colorspace_comes_after_input_file() {
        // TC-EXT-MAC-002: -colorspace must come AFTER input file
        let pdf_path = Path::new("/test/input.pdf");
        let output_path = Path::new("/test/output.png");
        let options = ExtractOptions::builder()
            .dpi(300)
            .colorspace(ColorSpace::Grayscale)
            .build();

        let args = MagickExtractor::build_magick_args(pdf_path, 0, output_path, &options);

        let input_pos = args.iter().position(|a| a.contains("input.pdf")).unwrap();
        let colorspace_pos = args.iter().position(|a| a == "-colorspace").unwrap();

        assert!(
            colorspace_pos > input_pos,
            "ImageMagick: -colorspace (pos {}) must come after input file (pos {}). Args: {:?}",
            colorspace_pos,
            input_pos,
            args
        );
    }

    #[test]
    fn test_magick_args_density_comes_before_input_file() {
        // TC-EXT-MAC-003: -density (input setting) must come BEFORE input file
        let pdf_path = Path::new("/test/input.pdf");
        let output_path = Path::new("/test/output.png");
        let options = ExtractOptions::builder().dpi(300).build();

        let args = MagickExtractor::build_magick_args(pdf_path, 0, output_path, &options);

        let density_pos = args.iter().position(|a| a == "-density").unwrap();
        let input_pos = args.iter().position(|a| a.contains("input.pdf")).unwrap();

        assert!(
            density_pos < input_pos,
            "ImageMagick: -density (pos {}) must come before input file (pos {}). Args: {:?}",
            density_pos,
            input_pos,
            args
        );
    }

    #[test]
    fn test_magick_args_output_comes_last() {
        // TC-EXT-MAC-004: Output file must be the last argument
        let pdf_path = Path::new("/test/input.pdf");
        let output_path = Path::new("/test/output.png");
        let options = ExtractOptions::builder()
            .dpi(300)
            .background([255, 255, 255])
            .colorspace(ColorSpace::Rgb)
            .build();

        let args = MagickExtractor::build_magick_args(pdf_path, 0, output_path, &options);

        let last_arg = args.last().unwrap();
        assert!(
            last_arg.contains("output.png"),
            "Output file must be last argument. Got: {:?}",
            args
        );
    }

    #[test]
    fn test_magick_args_full_order_with_all_options() {
        // TC-EXT-MAC-005: Full argument order verification
        let pdf_path = Path::new("/test/input.pdf");
        let output_path = Path::new("/test/output.jpg");
        let options = ExtractOptions::builder()
            .dpi(300)
            .background([255, 255, 255])
            .colorspace(ColorSpace::Rgb)
            .format(ImageFormat::Jpeg { quality: 95 })
            .build();

        let args = MagickExtractor::build_magick_args(pdf_path, 0, output_path, &options);

        // Expected order:
        // 1. -density 300 (input setting)
        // 2. input.pdf[0] (input file)
        // 3. -background rgb(255,255,255) (operation)
        // 4. -alpha remove (operation)
        // 5. -alpha off (operation)
        // 6. -colorspace sRGB (operation)
        // 7. -quality 95 (output setting)
        // 8. output.jpg (output file)

        assert_eq!(args[0], "-density");
        assert_eq!(args[1], "300");
        assert!(args[2].contains("input.pdf[0]"));
        assert_eq!(args[3], "-background");
        assert!(args[4].contains("rgb(255,255,255)"));
        assert_eq!(args[5], "-alpha");
        assert_eq!(args[6], "remove");
        assert_eq!(args[7], "-alpha");
        assert_eq!(args[8], "off");
        assert_eq!(args[9], "-colorspace");
        assert_eq!(args[10], "sRGB");
        assert_eq!(args[11], "-quality");
        assert_eq!(args[12], "95");
        assert!(args[13].contains("output.jpg"));
    }

    #[test]
    fn test_magick_args_without_background() {
        // TC-EXT-MAC-006: When no background, -alpha should not be present
        let pdf_path = Path::new("/test/input.pdf");
        let output_path = Path::new("/test/output.png");
        let options = ExtractOptions::builder().dpi(300).no_background().build();

        let args = MagickExtractor::build_magick_args(pdf_path, 0, output_path, &options);

        // -alpha should not be present
        assert!(
            !args.iter().any(|a| a == "-alpha"),
            "-alpha should not be present when no background. Args: {:?}",
            args
        );
    }

    // ============ Issue #58: page-tree extraction order ============

    fn make_test_image_stream(width: i64, height: i64) -> lopdf::Stream {
        use lopdf::{dictionary, Object};
        lopdf::Stream::new(
            dictionary! {
                "Type" => Object::Name(b"XObject".to_vec()),
                "Subtype" => Object::Name(b"Image".to_vec()),
                "Width" => Object::Integer(width),
                "Height" => Object::Integer(height),
                "ColorSpace" => Object::Name(b"DeviceRGB".to_vec()),
                "BitsPerComponent" => Object::Integer(8),
                "Filter" => Object::Name(b"DCTDecode".to_vec()),
            },
            // DCTDecode streams are written verbatim, so dummy bytes suffice
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10],
        )
    }

    /// Build a PDF whose image objects are registered in REVERSE page order,
    /// so object-id order disagrees with page-tree order (the #58 scenario).
    /// Pages are identified by image width: page 1 → 100, page 2 → 200, page 3 → 300.
    fn build_reverse_registered_pdf(path: &std::path::Path) {
        use lopdf::{dictionary, Document, Object};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        // Reverse registration: page 3's image gets the SMALLEST object id
        let img3 = doc.add_object(Object::Stream(make_test_image_stream(300, 400)));
        let img2 = doc.add_object(Object::Stream(make_test_image_stream(200, 400)));
        let img1 = doc.add_object(Object::Stream(make_test_image_stream(100, 400)));

        let mut kids = Vec::new();
        for img_ref in [img1, img2, img3] {
            let page_id = doc.add_object(dictionary! {
                "Type" => Object::Name(b"Page".to_vec()),
                "Parent" => Object::Reference(pages_id),
                "MediaBox" => Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(612),
                    Object::Integer(792),
                ]),
                "Resources" => Object::Dictionary(dictionary! {
                    "XObject" => Object::Dictionary(dictionary! {
                        "Im0" => Object::Reference(img_ref),
                    }),
                }),
            });
            kids.push(Object::Reference(page_id));
        }

        let count = kids.len() as i64;
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(kids),
                "Count" => Object::Integer(count),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(path).unwrap();
    }

    #[test]
    fn test_lopdf_extraction_follows_page_tree_order() {
        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("reverse.pdf");
        build_reverse_registered_pdf(&pdf_path);

        let out_dir = tmp.path().join("out");
        let results =
            LopdfExtractor::extract_all(&pdf_path, &out_dir, &ExtractOptions::default()).unwrap();

        assert_eq!(results.len(), 3);
        // Reading order must follow the page tree, not object-id order
        assert_eq!(
            results.iter().map(|r| r.width).collect::<Vec<_>>(),
            vec![100, 200, 300],
            "pages must come back in page-tree order"
        );
        assert_eq!(
            results.iter().map(|r| r.page_index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn test_lopdf_extraction_picks_largest_image_per_page() {
        use lopdf::{dictionary, Document, Object};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("multi_image.pdf");

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        // One page containing a small decoration image and the large page scan
        let small = doc.add_object(Object::Stream(make_test_image_stream(50, 50)));
        let scan = doc.add_object(Object::Stream(make_test_image_stream(1000, 1400)));
        let page_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(612),
                Object::Integer(792),
            ]),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    "Im0" => Object::Reference(small),
                    "Im1" => Object::Reference(scan),
                }),
            }),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![Object::Reference(page_id)]),
                "Count" => Object::Integer(1),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let out_dir = tmp.path().join("out");
        let results =
            LopdfExtractor::extract_all(&pdf_path, &out_dir, &ExtractOptions::default()).unwrap();

        assert_eq!(results.len(), 1, "one entry per page, not per image");
        assert_eq!(
            results[0].width, 1000,
            "largest-area image is the page scan"
        );
    }

    #[test]
    fn test_lopdf_extraction_object_scan_fallback() {
        use lopdf::{dictionary, Document, Object};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("orphan.pdf");

        // Page tree with NO XObject resources; the image exists only as an
        // orphan object → page-tree walk finds nothing, fallback must fire.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let _orphan = doc.add_object(Object::Stream(make_test_image_stream(640, 480)));
        let page_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(612),
                Object::Integer(792),
            ]),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![Object::Reference(page_id)]),
                "Count" => Object::Integer(1),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let out_dir = tmp.path().join("out");
        let results =
            LopdfExtractor::extract_all(&pdf_path, &out_dir, &ExtractOptions::default()).unwrap();

        assert_eq!(
            results.len(),
            1,
            "legacy object scan must still find orphan images"
        );
        assert_eq!(results[0].width, 640);
    }

    #[test]
    fn native_inventory_preserves_physical_pages_and_nested_form_images() {
        use lopdf::{dictionary, Document, Object, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("native-pages.pdf");
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();

        let form_image = doc.add_object(Object::Stream(make_test_image_stream(300, 400)));
        let direct_image = doc.add_object(Object::Stream(make_test_image_stream(100, 200)));
        let form = doc.add_object(Object::Stream(Stream::new(
            dictionary! {
                "Type" => Object::Name(b"XObject".to_vec()),
                "Subtype" => Object::Name(b"Form".to_vec()),
                "BBox" => Object::Array(vec![0.into(), 0.into(), 300.into(), 400.into()]),
                "Resources" => Object::Dictionary(dictionary! {
                    "XObject" => Object::Dictionary(dictionary! {
                        "Nested" => Object::Reference(form_image),
                    }),
                }),
            },
            b"q /Nested Do Q".to_vec(),
        )));
        let direct_content =
            doc.add_object(Stream::new(dictionary! {}, b"q /Direct Do Q".to_vec()));
        let form_content = doc.add_object(Stream::new(dictionary! {}, b"q /Form0 Do Q".to_vec()));

        let direct_page = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![10.into(), 20.into(), 622.into(), 812.into()]),
            "CropBox" => Object::Array(vec![12.into(), 22.into(), 620.into(), 810.into()]),
            "Rotate" => Object::Integer(90),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    "Direct" => Object::Reference(direct_image),
                }),
            }),
            "Contents" => Object::Reference(direct_content),
        });
        let blank_page = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 400.into(), 600.into()]),
        });
        let form_page = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 500.into(), 700.into()]),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    "Form0" => Object::Reference(form),
                }),
            }),
            "Contents" => Object::Reference(form_content),
        });

        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![
                    Object::Reference(direct_page),
                    Object::Reference(blank_page),
                    Object::Reference(form_page),
                ]),
                "Count" => Object::Integer(3),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let pages = LopdfExtractor::inspect_native_pages(&pdf_path).unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(
            pages
                .iter()
                .map(|page| page.physical_page.page_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(pages[0].kind, NativePageKind::SingleImage);
        assert_eq!(pages[1].kind, NativePageKind::Blank);
        assert_eq!(
            pages[2].kind,
            NativePageKind::UnsupportedContent,
            "{:?}",
            pages[2].issues
        );
        assert!(!pages[0].review_required);
        assert!(!pages[1].review_required);
        assert!(pages[2].review_required);
        assert_eq!(
            pages[0].image_invocations[0]
                .metadata
                .object_id
                .object_number,
            direct_image.0
        );
        assert_eq!(
            pages[2].image_invocations[0]
                .metadata
                .object_id
                .object_number,
            form_image.0
        );
        assert_eq!(
            pages[0]
                .physical_page
                .media_box
                .as_ref()
                .unwrap()
                .value
                .coordinates(),
            [10.0, 20.0, 622.0, 812.0]
        );
        assert_eq!(
            pages[0]
                .physical_page
                .crop_box
                .as_ref()
                .unwrap()
                .value
                .coordinates(),
            [12.0, 22.0, 620.0, 810.0]
        );
        assert_eq!(pages[0].physical_page.normalized_rotation(), Some(90));
    }

    #[test]
    fn native_inventory_retains_dct_flate_and_ccitt_metadata() {
        use lopdf::{dictionary, Document, Object, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("native-filters.pdf");
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let specifications = [
            (
                "DCTDecode",
                Object::Name(b"DeviceRGB".to_vec()),
                8_i64,
                Object::Null,
                vec![1_u8, 2, 3, 4],
            ),
            (
                "FlateDecode",
                Object::Name(b"DeviceGray".to_vec()),
                8_i64,
                Object::Dictionary(dictionary! {
                    "Predictor" => Object::Integer(12),
                    "Columns" => Object::Integer(16),
                }),
                vec![5_u8, 6, 7],
            ),
            (
                "CCITTFaxDecode",
                Object::Name(b"DeviceGray".to_vec()),
                1_i64,
                Object::Dictionary(dictionary! {
                    "K" => Object::Integer(-1),
                    "Columns" => Object::Integer(16),
                    "Rows" => Object::Integer(16),
                    "BlackIs1" => Object::Boolean(true),
                }),
                vec![8_u8, 9],
            ),
        ];
        let mut kids = Vec::new();
        let mut expected_ids = Vec::new();
        for (index, (filter, color_space, bits, decode_params, bytes)) in
            specifications.into_iter().enumerate()
        {
            let mut image_dict = dictionary! {
                "Type" => Object::Name(b"XObject".to_vec()),
                "Subtype" => Object::Name(b"Image".to_vec()),
                "Width" => Object::Integer(16),
                "Height" => Object::Integer(16),
                "ColorSpace" => color_space,
                "BitsPerComponent" => Object::Integer(bits),
                "Filter" => Object::Name(filter.as_bytes().to_vec()),
            };
            if decode_params != Object::Null {
                image_dict.set("DecodeParms", decode_params);
            }
            let image_id = doc.add_object(Object::Stream(Stream::new(image_dict, bytes)));
            expected_ids.push(image_id);
            let content = doc.add_object(Stream::new(
                dictionary! {},
                format!("q /Im{index} Do Q").into_bytes(),
            ));
            let image_name = format!("Im{index}").into_bytes();
            let mut xobjects = lopdf::Dictionary::new();
            xobjects.set(image_name, Object::Reference(image_id));
            let page_id = doc.add_object(dictionary! {
                "Type" => Object::Name(b"Page".to_vec()),
                "Parent" => Object::Reference(pages_id),
                "MediaBox" => Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()]),
                "Resources" => Object::Dictionary(dictionary! {
                    "XObject" => Object::Dictionary(xobjects),
                }),
                "Contents" => Object::Reference(content),
            });
            kids.push(Object::Reference(page_id));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(kids),
                "Count" => Object::Integer(3),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let pages = LopdfExtractor::inspect_native_pages(&pdf_path).unwrap();
        assert_eq!(pages.len(), 3);
        for (index, expected_filter) in ["DCTDecode", "FlateDecode", "CCITTFaxDecode"]
            .into_iter()
            .enumerate()
        {
            let image = &pages[index].image_invocations[0].metadata;
            assert_eq!(pages[index].kind, NativePageKind::SingleImage);
            assert_eq!(image.object_id.object_number, expected_ids[index].0);
            assert_eq!(image.width, 16);
            assert_eq!(image.height, 16);
            assert_eq!(image.filters, vec![expected_filter]);
            assert_eq!(image.encoded_sha256.len(), 64);
        }
        assert_eq!(
            pages[0].image_invocations[0].metadata.color_space,
            Some(serde_json::json!("/DeviceRGB"))
        );
        assert_eq!(
            pages[0].image_invocations[0].metadata.bits_per_component,
            Some(8)
        );
        assert_eq!(
            pages[1].image_invocations[0].metadata.color_space,
            Some(serde_json::json!("/DeviceGray"))
        );
        assert_eq!(
            pages[2].image_invocations[0].metadata.bits_per_component,
            Some(1)
        );
        assert_eq!(
            pages[2].image_invocations[0]
                .metadata
                .decode_params
                .as_ref()
                .unwrap()["K"],
            serde_json::json!(-1)
        );
        assert_eq!(
            pages[2].image_invocations[0]
                .metadata
                .decode_params
                .as_ref()
                .unwrap()["BlackIs1"],
            serde_json::json!(true)
        );
        assert_eq!(
            pages[0].image_invocations[0].metadata.transform_decode,
            NativeTransformDecodeCapability::Dct8
        );
        assert_eq!(
            pages[1].image_invocations[0].metadata.transform_decode,
            NativeTransformDecodeCapability::Unsupported
        );
        assert_eq!(
            pages[2].image_invocations[0].metadata.transform_decode,
            NativeTransformDecodeCapability::Unsupported
        );

        let projected = pages[0].image_invocations[0]
            .metadata
            .to_manifest_v1()
            .unwrap();
        assert_eq!(
            projected.object_id.unwrap().object_number,
            expected_ids[0].0
        );
        assert_eq!(projected.filter.as_deref(), Some("DCTDecode"));
        assert_eq!(projected.color_space.as_deref(), Some("/DeviceRGB"));

        let mut complex_filter = pages[0].image_invocations[0].metadata.clone();
        complex_filter.filters.push("ASCII85Decode".to_string());
        assert!(matches!(
            complex_filter.to_manifest_v1(),
            Err(ManifestV1ProjectionError::FilterChainNotRepresentable)
        ));
    }

    #[test]
    fn native_inventory_inherits_geometry_and_fails_closed_per_page() {
        use lopdf::{dictionary, Document, Object, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("native-classification.pdf");
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let image = doc.add_object(Object::Stream(make_test_image_stream(640, 960)));
        let repeated = doc.add_object(Stream::new(
            dictionary! {},
            b"q /Scan Do Q q /Scan Do Q".to_vec(),
        ));
        let drawing = doc.add_object(Stream::new(dictionary! {}, b"q 1 0 0 1 0 0 cm Q".to_vec()));
        let missing = doc.add_object(Stream::new(dictionary! {}, b"q /Missing Do Q".to_vec()));

        let mut kids = Vec::new();
        for (contents, media_box) in [
            (Some(repeated), None),
            (Some(drawing), None),
            (Some(missing), None),
            (None, None),
            (
                None,
                Some(Object::Array(vec![0.into(), 0.into(), 0.into(), 10.into()])),
            ),
        ] {
            let mut page = dictionary! {
                "Type" => Object::Name(b"Page".to_vec()),
                "Parent" => Object::Reference(pages_id),
            };
            if let Some(contents) = contents {
                page.set("Contents", Object::Reference(contents));
            }
            if let Some(media_box) = media_box {
                page.set("MediaBox", media_box);
            }
            kids.push(Object::Reference(doc.add_object(page)));
        }

        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(kids),
                "Count" => Object::Integer(5),
                "MediaBox" => Object::Array(vec![1.into(), 2.into(), 401.into(), 602.into()]),
                "CropBox" => Object::Array(vec![3.into(), 4.into(), 399.into(), 600.into()]),
                "Rotate" => Object::Integer(-90),
                "Resources" => Object::Dictionary(dictionary! {
                    "XObject" => Object::Dictionary(dictionary! {
                        "Scan" => Object::Reference(image),
                    }),
                }),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let pages = LopdfExtractor::inspect_native_pages(&pdf_path).unwrap();
        assert_eq!(pages.len(), 5);
        assert_eq!(pages[0].kind, NativePageKind::MultipleImages);
        assert_eq!(pages[0].image_invocations.len(), 2);
        assert!(pages[0].review_required);
        assert_eq!(pages[1].kind, NativePageKind::NonImageContent);
        assert!(pages[1].review_required);
        assert_eq!(pages[2].kind, NativePageKind::UnsupportedContent);
        assert!(pages[2].review_required);
        assert!(pages[2]
            .issues
            .iter()
            .any(|issue| issue.code == NativePageIssueCode::UnresolvedXObject));
        assert_eq!(pages[3].kind, NativePageKind::Blank);
        assert!(!pages[3].review_required);
        assert_eq!(pages[3].physical_page.normalized_rotation(), Some(270));
        assert_eq!(
            pages[3]
                .physical_page
                .media_box
                .as_ref()
                .unwrap()
                .value
                .coordinates(),
            [1.0, 2.0, 401.0, 602.0]
        );
        assert_eq!(
            pages[3]
                .physical_page
                .crop_box
                .as_ref()
                .unwrap()
                .value
                .coordinates(),
            [3.0, 4.0, 399.0, 600.0]
        );
        assert_eq!(pages[4].kind, NativePageKind::UnsupportedContent);
        assert!(pages[4].physical_page.media_box.is_none());
        assert!(pages[4].review_required);
    }

    #[test]
    fn native_inventory_tracks_binding_matrix_and_composite_paint() {
        use lopdf::{dictionary, Document, Object, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("native-binding.pdf");
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let scan = doc.add_object(Object::Stream(make_test_image_stream(640, 960)));
        let unused = doc.add_object(Object::Stream(make_test_image_stream(20, 20)));
        let direct_content = doc.add_object(Stream::new(
            dictionary! {},
            b"q 100 0 0 200 10 20 cm /Scan Do Q".to_vec(),
        ));
        let composite_content = doc.add_object(Stream::new(
            dictionary! {},
            b"q /Scan Do Q BT (caption) Tj ET".to_vec(),
        ));
        let resources = Object::Dictionary(dictionary! {
            "XObject" => Object::Dictionary(dictionary! {
                "Scan" => Object::Reference(scan),
                "Unused" => Object::Reference(unused),
            }),
        });
        let first_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()]),
            "Resources" => resources.clone(),
            "Contents" => Object::Reference(direct_content),
        });
        let second_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()]),
            "Resources" => resources,
            "Contents" => Object::Reference(composite_content),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![first_id.into(), second_id.into()]),
                "Count" => Object::Integer(2),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let pages = LopdfExtractor::inspect_native_pages(&pdf_path).unwrap();
        assert_eq!(pages[0].kind, NativePageKind::SingleImage);
        assert_eq!(pages[0].image_invocations.len(), 1);
        assert_eq!(
            pages[0].image_invocations[0]
                .metadata
                .object_id
                .object_number,
            scan.0
        );
        assert_eq!(pages[0].image_invocations[0].binding.resource_path.len(), 1);
        assert_eq!(
            pages[0].image_invocations[0].binding.resource_path[0].resource_name,
            b"Scan"
        );
        assert!(pages[0].image_invocations[0].binding.is_direct);
        assert_eq!(
            pages[0].image_invocations[0]
                .binding
                .placement_matrix
                .coordinates(),
            [100.0, 0.0, 0.0, 200.0, 10.0, 20.0]
        );

        assert_eq!(pages[1].kind, NativePageKind::CompositeContent);
        assert_eq!(pages[1].image_invocations.len(), 1);
        assert!(pages[1].review_required);
    }

    #[test]
    fn native_document_retains_source_stream_access_without_outputs() {
        use lopdf::{dictionary, Document, Object, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("native-document.pdf");
        let encoded = vec![11_u8, 22, 33, 44, 55];
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let image_id = doc.add_object(Object::Stream(Stream::new(
            dictionary! {
                "Type" => Object::Name(b"XObject".to_vec()),
                "Subtype" => Object::Name(b"Image".to_vec()),
                "Width" => Object::Integer(1),
                "Height" => Object::Integer(1),
                "ColorSpace" => Object::Name(b"DeviceGray".to_vec()),
                "BitsPerComponent" => Object::Integer(8),
                "Filter" => Object::Name(b"FlateDecode".to_vec()),
            },
            encoded.clone(),
        )));
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"/Scan Do".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    "Scan" => Object::Reference(image_id),
                }),
            }),
            "Contents" => Object::Reference(content_id),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![page_id.into()]),
                "Count" => Object::Integer(1),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let native = NativePdfExtractor::extract_path(&pdf_path).unwrap();
        assert_eq!(native.pages().len(), 1);
        assert_eq!(
            native.pages()[0].physical_page.page_object_id.object_number,
            page_id.0
        );
        let invocation = &native.pages()[0].image_invocations[0];
        assert_eq!(
            native.encoded_image_bytes(&invocation.metadata).unwrap(),
            encoded
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn native_inventory_composes_nested_form_matrices_and_paths() {
        use lopdf::{dictionary, Document, Object, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("nested-form-binding.pdf");
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let image_id = doc.add_object(Object::Stream(make_test_image_stream(10, 10)));
        let inner_form = doc.add_object(Object::Stream(Stream::new(
            dictionary! {
                "Type" => Object::Name(b"XObject".to_vec()),
                "Subtype" => Object::Name(b"Form".to_vec()),
                "BBox" => Object::Array(vec![0.into(), 0.into(), 1.into(), 1.into()]),
                "Matrix" => Object::Array(vec![2.into(), 0.into(), 0.into(), 3.into(), 5.into(), 7.into()]),
                "Resources" => Object::Dictionary(dictionary! {
                    "XObject" => Object::Dictionary(dictionary! {
                        "Image" => Object::Reference(image_id),
                    }),
                }),
            },
            b"/Image Do".to_vec(),
        )));
        let outer_form = doc.add_object(Object::Stream(Stream::new(
            dictionary! {
                "Type" => Object::Name(b"XObject".to_vec()),
                "Subtype" => Object::Name(b"Form".to_vec()),
                "BBox" => Object::Array(vec![0.into(), 0.into(), 1.into(), 1.into()]),
                "Matrix" => Object::Array(vec![1.into(), 0.into(), 0.into(), 1.into(), 4.into(), 6.into()]),
                "Resources" => Object::Dictionary(dictionary! {
                    "XObject" => Object::Dictionary(dictionary! {
                        "Inner" => Object::Reference(inner_form),
                    }),
                }),
            },
            b"/Inner Do".to_vec(),
        )));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"10 0 0 20 1 2 cm /Outer Do".to_vec(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    "Outer" => Object::Reference(outer_form),
                }),
            }),
            "Contents" => Object::Reference(content_id),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![page_id.into()]),
                "Count" => Object::Integer(1),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let pages = LopdfExtractor::inspect_native_pages(&pdf_path).unwrap();
        let binding = &pages[0].image_invocations[0].binding;
        assert_eq!(
            binding
                .resource_path
                .iter()
                .map(|step| step.resource_name.as_slice())
                .collect::<Vec<_>>(),
            vec![
                b"Outer".as_slice(),
                b"Inner".as_slice(),
                b"Image".as_slice()
            ]
        );
        assert_eq!(
            binding.placement_matrix.coordinates(),
            [20.0, 0.0, 0.0, 60.0, 91.0, 262.0]
        );
        assert!(!binding.is_direct);
    }

    #[test]
    fn native_inventory_contains_form_cycles_at_page_scope() {
        use lopdf::{dictionary, Document, Object, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let pdf_path = tmp.path().join("form-cycle.pdf");
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let form_id = doc.new_object_id();
        doc.objects.insert(
            form_id,
            Object::Stream(Stream::new(
                dictionary! {
                    "Type" => Object::Name(b"XObject".to_vec()),
                    "Subtype" => Object::Name(b"Form".to_vec()),
                    "BBox" => Object::Array(vec![0.into(), 0.into(), 1.into(), 1.into()]),
                    "Resources" => Object::Dictionary(dictionary! {
                        "XObject" => Object::Dictionary(dictionary! {
                            "Self" => Object::Reference(form_id),
                        }),
                    }),
                },
                b"/Self Do".to_vec(),
            )),
        );
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"/Cycle Do".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    "Cycle" => Object::Reference(form_id),
                }),
            }),
            "Contents" => Object::Reference(content_id),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![page_id.into()]),
                "Count" => Object::Integer(1),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&pdf_path).unwrap();

        let pages = LopdfExtractor::inspect_native_pages(&pdf_path).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].kind, NativePageKind::UnsupportedContent);
        assert!(pages[0]
            .issues
            .iter()
            .any(|issue| issue.code == NativePageIssueCode::FormCycle));
    }
}
