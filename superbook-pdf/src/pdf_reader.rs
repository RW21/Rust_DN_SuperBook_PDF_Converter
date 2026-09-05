//! PDF Reader module
//!
//! Provides functionality to read PDF files and extract metadata.
//!
//! # Features
//!
//! - Read PDF files using lopdf
//! - Extract page count, dimensions, and rotation
//! - Extract metadata (title, author, etc.)
//! - Detect encrypted PDFs
//!
//! # Example
//!
//! ```rust,no_run
//! use superbook_pdf::LopdfReader;
//!
//! let reader = LopdfReader::new("document.pdf").unwrap();
//! println!("Pages: {}", reader.info.page_count);
//! println!("Title: {:?}", reader.info.metadata.title);
//! ```

use crate::transform_manifest::PdfObjectId;
use lopdf::{Document, Object, ObjectId};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// PDF reading error types
#[derive(Debug, Error)]
pub enum PdfReaderError {
    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Invalid PDF format: {0}")]
    InvalidFormat(String),

    #[error("Encrypted PDF not supported")]
    EncryptedPdf,

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("PDF parse error: {0}")]
    ParseError(String),
}

pub type Result<T> = std::result::Result<T, PdfReaderError>;

/// PDF document information
#[derive(Debug, Clone)]
pub struct PdfDocument {
    pub path: PathBuf,
    pub page_count: usize,
    pub metadata: PdfMetadata,
    pub pages: Vec<PdfPage>,
    pub is_encrypted: bool,
}

/// PDF metadata
#[derive(Debug, Clone, Default)]
pub struct PdfMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub creation_date: Option<String>,
    pub modification_date: Option<String>,
}

/// Page information
#[derive(Debug, Clone)]
pub struct PdfPage {
    /// 0-indexed page number
    pub index: usize,
    /// Width in points (1 point = 1/72 inch)
    pub width_pt: f64,
    /// Height in points
    pub height_pt: f64,
    /// Rotation (0, 90, 180, 270)
    pub rotation: u16,
    /// Whether the page contains images
    pub has_images: bool,
    /// Whether the page contains text
    pub has_text: bool,
}

/// Exact PDF rectangle coordinates, including a non-zero origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfRect([f64; 4]);

/// Invalid or non-positive PDF rectangle.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("PDF rectangle requires finite coordinates and positive finite dimensions")]
pub struct InvalidPdfRect;

impl PdfRect {
    pub fn try_new(coordinates: [f64; 4]) -> std::result::Result<Self, InvalidPdfRect> {
        if !coordinates.iter().all(|value| value.is_finite())
            || coordinates[2] <= coordinates[0]
            || coordinates[3] <= coordinates[1]
            || !(coordinates[2] - coordinates[0]).is_finite()
            || !(coordinates[3] - coordinates[1]).is_finite()
        {
            return Err(InvalidPdfRect);
        }
        Ok(Self(coordinates.map(|value| {
            if value == 0.0 {
                0.0
            } else {
                value
            }
        })))
    }

    #[must_use]
    pub const fn coordinates(self) -> [f64; 4] {
        self.0
    }
}

/// An inherited page-tree value and the object that defined it.
#[derive(Debug, Clone, PartialEq)]
pub struct InheritedPageValue<T> {
    pub value: T,
    pub defined_on: PdfObjectId,
}

/// Stable physical-page geometry failure classifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalPageIssue {
    MissingMediaBox,
    InvalidMediaBox,
    InvalidCropBox,
    InvalidRotation,
    InvalidParentChain,
}

impl PhysicalPageIssue {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingMediaBox => "missing_media_box",
            Self::InvalidMediaBox => "invalid_media_box",
            Self::InvalidCropBox => "invalid_crop_box",
            Self::InvalidRotation => "invalid_rotation",
            Self::InvalidParentChain => "invalid_parent_chain",
        }
    }
}

/// Page-tree identity and effective inherited geometry for one physical page.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalPageMetadata {
    pub page_index: usize,
    pub source_page_number: usize,
    pub page_object_id: PdfObjectId,
    pub media_box: Option<InheritedPageValue<PdfRect>>,
    pub crop_box: Option<InheritedPageValue<PdfRect>>,
    pub raw_rotation: Option<InheritedPageValue<i32>>,
    pub issues: Vec<PhysicalPageIssue>,
}

impl PhysicalPageMetadata {
    #[must_use]
    pub fn effective_crop_box(&self) -> Option<PdfRect> {
        if self.issues.contains(&PhysicalPageIssue::InvalidCropBox)
            || self.issues.contains(&PhysicalPageIssue::InvalidParentChain)
        {
            return None;
        }
        self.crop_box
            .as_ref()
            .map(|value| value.value)
            .or_else(|| self.media_box.as_ref().map(|value| value.value))
    }

    #[must_use]
    pub fn normalized_rotation(&self) -> Option<u16> {
        if self.issues.contains(&PhysicalPageIssue::InvalidRotation)
            || self.issues.contains(&PhysicalPageIssue::InvalidParentChain)
        {
            return None;
        }
        self.raw_rotation
            .as_ref()
            .map_or(Some(0), |value| Some(value.value.rem_euclid(360) as u16))
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.issues.is_empty()
    }
}

/// PDF Reader trait
pub trait PdfReader {
    /// Open a PDF file
    fn open(path: impl AsRef<Path>) -> Result<PdfDocument>;

    /// Get page information by index
    fn get_page(&self, index: usize) -> Result<&PdfPage>;

    /// Get iterator over all pages
    fn pages(&self) -> impl Iterator<Item = &PdfPage>;

    /// Get document metadata
    fn metadata(&self) -> &PdfMetadata;

    /// Check if PDF is encrypted
    fn is_encrypted(&self) -> bool;
}

/// lopdf-based PDF reader implementation
pub struct LopdfReader {
    #[allow(dead_code)]
    document: Document,
    physical_pages: Vec<PhysicalPageMetadata>,
    pub info: PdfDocument,
}

impl LopdfReader {
    /// Create a new PDF reader for the given path
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(PdfReaderError::FileNotFound(path.to_path_buf()));
        }

        let document = Document::load(path).map_err(|e| {
            let err_str = e.to_string();
            if err_str.contains("header") || err_str.contains("PDF") {
                PdfReaderError::InvalidFormat(err_str)
            } else {
                PdfReaderError::ParseError(err_str)
            }
        })?;

        let is_encrypted = document.is_encrypted();
        let page_count = document.get_pages().len();
        let metadata = Self::extract_metadata(&document);
        let pages = Self::extract_pages(&document)?;
        let physical_pages = Self::extract_physical_pages(&document)?;

        Ok(Self {
            document,
            physical_pages,
            info: PdfDocument {
                path: path.to_path_buf(),
                page_count,
                metadata,
                pages,
                is_encrypted,
            },
        })
    }

    /// Strict preservation loader. Never calls legacy geometry/default routines.
    pub(crate) fn new_native(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(PdfReaderError::FileNotFound(path.to_path_buf()));
        }
        let document =
            Document::load(path).map_err(|error| PdfReaderError::ParseError(error.to_string()))?;
        // The presence of Encrypt is sufficient: a library may already have
        // decrypted a document with an empty password during loading.
        if document.trailer.has(b"Encrypt") || document.is_encrypted() {
            return Err(PdfReaderError::EncryptedPdf);
        }
        let ids = Self::strict_page_ids(&document)?;
        let physical_pages = Self::physical_metadata_for_ids(&document, ids)?;
        let info = PdfDocument {
            path: path.to_path_buf(),
            page_count: physical_pages.len(),
            metadata: Self::extract_metadata(&document),
            // Native consumers use physical_pages, never the legacy lossy view.
            pages: Vec::new(),
            is_encrypted: false,
        };
        Ok(Self {
            document,
            physical_pages,
            info,
        })
    }

    fn strict_page_ids(doc: &Document) -> Result<Vec<ObjectId>> {
        let invalid = |message: &str| PdfReaderError::InvalidFormat(message.to_string());
        let catalog_id = doc
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(|_| invalid("missing catalog reference"))?;
        let catalog = doc
            .objects
            .get(&catalog_id)
            .and_then(|object| object.as_dict().ok())
            .ok_or_else(|| invalid("invalid catalog dictionary"))?;
        let root = catalog
            .get(b"Pages")
            .and_then(Object::as_reference)
            .map_err(|_| invalid("missing page-tree reference"))?;
        let mut seen = HashSet::new();
        let mut pages = Vec::new();
        Self::walk_strict_pages(doc, root, None, 0, &mut seen, &mut pages)?;
        if pages.is_empty() {
            return Err(invalid("empty page tree"));
        }
        Ok(pages)
    }

    fn walk_strict_pages(
        doc: &Document,
        id: ObjectId,
        parent: Option<ObjectId>,
        depth: usize,
        seen: &mut HashSet<ObjectId>,
        pages: &mut Vec<ObjectId>,
    ) -> Result<usize> {
        let invalid = |message: &str| PdfReaderError::InvalidFormat(message.to_string());
        if depth >= 128 || seen.len() >= 100_000 || !seen.insert(id) {
            return Err(invalid("cyclic, duplicated, or over-limit page tree"));
        }
        let dict = doc
            .objects
            .get(&id)
            .and_then(|object| object.as_dict().ok())
            .ok_or_else(|| invalid("unresolved page-tree dictionary"))?;
        if let Some(parent) = parent {
            if dict.get(b"Parent").and_then(Object::as_reference).ok() != Some(parent) {
                return Err(invalid("page-tree Parent does not match Kids"));
            }
        } else if dict.has(b"Parent") {
            return Err(invalid("root page-tree node has a Parent"));
        }
        match dict.get(b"Type").and_then(Object::as_name_str).ok() {
            Some("Page") if parent.is_some() => {
                if dict.has(b"Kids") {
                    return Err(invalid("Page node has Kids"));
                }
                pages.push(id);
                Ok(1)
            }
            Some("Pages") => {
                let kids = dict
                    .get(b"Kids")
                    .and_then(Object::as_array)
                    .map_err(|_| invalid("invalid page-tree Kids array"))?;
                if kids.len() > 100_000 {
                    return Err(invalid("over-limit Kids array"));
                }
                let declared = dict
                    .get(b"Count")
                    .and_then(Object::as_i64)
                    .ok()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| invalid("invalid page-tree Count"))?;
                let mut actual = 0usize;
                for kid in kids {
                    let kid = kid
                        .as_reference()
                        .map_err(|_| invalid("Kids entry is not a reference"))?;
                    actual = actual
                        .checked_add(Self::walk_strict_pages(
                            doc,
                            kid,
                            Some(id),
                            depth + 1,
                            seen,
                            pages,
                        )?)
                        .ok_or_else(|| invalid("page count overflow"))?;
                }
                if actual != declared {
                    return Err(invalid("page-tree Count disagrees with physical pages"));
                }
                Ok(actual)
            }
            _ => Err(invalid("invalid page-tree node Type")),
        }
    }

    /// Physical page-tree records with exact inherited geometry and provenance.
    #[must_use]
    pub fn physical_pages(&self) -> &[PhysicalPageMetadata] {
        &self.physical_pages
    }

    /// Parsed source document for preservation-only crate internals.
    pub(crate) fn document(&self) -> &Document {
        &self.document
    }

    fn extract_physical_pages(doc: &Document) -> Result<Vec<PhysicalPageMetadata>> {
        Self::physical_metadata_for_ids(doc, doc.get_pages().into_values().collect())
    }

    fn physical_metadata_for_ids(
        doc: &Document,
        ids: Vec<ObjectId>,
    ) -> Result<Vec<PhysicalPageMetadata>> {
        ids.into_iter()
            .enumerate()
            .map(|(page_index, page_id)| {
                let mut issues = Vec::new();
                let media_box = match Self::inherited_page_object(doc, page_id, b"MediaBox") {
                    Ok(Some((value, defined_on))) => match Self::parse_rect(doc, &value) {
                        Some(value) => Some(InheritedPageValue {
                            value,
                            defined_on: Self::manifest_object_id(defined_on),
                        }),
                        None => {
                            issues.push(PhysicalPageIssue::InvalidMediaBox);
                            None
                        }
                    },
                    Ok(None) => {
                        issues.push(PhysicalPageIssue::MissingMediaBox);
                        None
                    }
                    Err(()) => {
                        issues.push(PhysicalPageIssue::InvalidParentChain);
                        None
                    }
                };
                let crop_box = match Self::inherited_page_object(doc, page_id, b"CropBox") {
                    Ok(Some((value, defined_on))) => match Self::parse_rect(doc, &value) {
                        Some(value) => Some(InheritedPageValue {
                            value,
                            defined_on: Self::manifest_object_id(defined_on),
                        }),
                        None => {
                            issues.push(PhysicalPageIssue::InvalidCropBox);
                            None
                        }
                    },
                    Ok(None) => None,
                    Err(()) => {
                        issues.push(PhysicalPageIssue::InvalidParentChain);
                        None
                    }
                };
                let raw_rotation = match Self::inherited_page_object(doc, page_id, b"Rotate") {
                    Ok(Some((value, defined_on))) => match Self::parse_rotation(doc, &value) {
                        Some(value) => Some(InheritedPageValue {
                            value,
                            defined_on: Self::manifest_object_id(defined_on),
                        }),
                        None => {
                            issues.push(PhysicalPageIssue::InvalidRotation);
                            None
                        }
                    },
                    Ok(None) => None,
                    Err(()) => {
                        issues.push(PhysicalPageIssue::InvalidParentChain);
                        None
                    }
                };
                issues.sort_by_key(|issue| issue.as_str());
                issues.dedup();

                Ok(PhysicalPageMetadata {
                    page_index,
                    source_page_number: page_index.checked_add(1).ok_or_else(|| {
                        PdfReaderError::ParseError("source page number overflow".to_string())
                    })?,
                    page_object_id: Self::manifest_object_id(page_id),
                    media_box,
                    crop_box,
                    raw_rotation,
                    issues,
                })
            })
            .collect()
    }

    pub(crate) fn inherited_page_object(
        doc: &Document,
        page_id: ObjectId,
        key: &[u8],
    ) -> std::result::Result<Option<(Object, ObjectId)>, ()> {
        let mut current = Some(page_id);
        let mut seen = HashSet::new();
        while let Some(object_id) = current {
            if seen.len() >= 128 || !seen.insert(object_id) {
                return Err(());
            }
            let dictionary = doc
                .objects
                .get(&object_id)
                .and_then(|object| object.as_dict().ok())
                .ok_or(())?;
            if let Ok(value) = dictionary.get(key) {
                return Ok(Some((value.clone(), object_id)));
            }
            current = match dictionary.get(b"Parent") {
                Ok(Object::Reference(parent)) => Some(*parent),
                Ok(_) => return Err(()),
                Err(_) => None,
            };
        }
        Ok(None)
    }

    fn parse_rect(doc: &Document, value: &Object) -> Option<PdfRect> {
        let resolved = Self::resolve_object_for_geometry(doc, value)?;
        let values = resolved.as_array().ok()?;
        if values.len() != 4 {
            return None;
        }
        let mut coordinates = [0.0; 4];
        for (destination, source) in coordinates.iter_mut().zip(values) {
            let source = Self::resolve_object_for_geometry(doc, source)?;
            *destination = match source {
                Object::Integer(value) => *value as f64,
                Object::Real(value) => f64::from(*value),
                _ => return None,
            };
        }
        PdfRect::try_new(coordinates).ok()
    }

    fn parse_rotation(doc: &Document, value: &Object) -> Option<i32> {
        let value = Self::resolve_object_for_geometry(doc, value)?;
        let value = value.as_i64().ok()?;
        let value = i32::try_from(value).ok()?;
        (value % 90 == 0).then_some(value)
    }

    fn resolve_object_for_geometry<'a>(doc: &'a Document, value: &'a Object) -> Option<&'a Object> {
        let mut current = value;
        let mut seen = HashSet::new();
        loop {
            match current {
                Object::Reference(object_id) => {
                    if seen.len() >= 128 || !seen.insert(*object_id) {
                        return None;
                    }
                    current = doc.objects.get(object_id)?;
                }
                _ => return Some(current),
            }
        }
    }

    const fn manifest_object_id(object_id: ObjectId) -> PdfObjectId {
        PdfObjectId {
            object_number: object_id.0,
            generation: object_id.1,
        }
    }

    /// Extract metadata from PDF document
    fn extract_metadata(doc: &Document) -> PdfMetadata {
        let mut metadata = PdfMetadata::default();

        // Try to get the Info dictionary
        if let Ok(info_ref) = doc.trailer.get(b"Info") {
            if let Ok(info_ref) = info_ref.as_reference() {
                if let Ok(info_dict) = doc.get_dictionary(info_ref) {
                    metadata.title = Self::get_string_from_dict(info_dict, b"Title");
                    metadata.author = Self::get_string_from_dict(info_dict, b"Author");
                    metadata.subject = Self::get_string_from_dict(info_dict, b"Subject");
                    metadata.keywords = Self::get_string_from_dict(info_dict, b"Keywords");
                    metadata.creator = Self::get_string_from_dict(info_dict, b"Creator");
                    metadata.producer = Self::get_string_from_dict(info_dict, b"Producer");
                    metadata.creation_date = Self::get_string_from_dict(info_dict, b"CreationDate");
                    metadata.modification_date = Self::get_string_from_dict(info_dict, b"ModDate");
                }
            }
        }

        metadata
    }

    /// Helper to extract string from dictionary
    fn get_string_from_dict(dict: &lopdf::Dictionary, key: &[u8]) -> Option<String> {
        dict.get(key).ok().and_then(|obj| {
            match obj {
                lopdf::Object::String(bytes, _) => {
                    // Try UTF-8 first, then Latin-1
                    String::from_utf8(bytes.clone())
                        .ok()
                        .or_else(|| Some(bytes.iter().map(|&b| b as char).collect()))
                }
                _ => None,
            }
        })
    }

    /// Extract page information from PDF document
    fn extract_pages(doc: &Document) -> Result<Vec<PdfPage>> {
        let page_ids = doc.get_pages();
        let mut pages = Vec::with_capacity(page_ids.len());

        for (index, (_, page_id)) in page_ids.iter().enumerate() {
            let page_dict = doc
                .get_dictionary(*page_id)
                .map_err(|e| PdfReaderError::ParseError(e.to_string()))?;

            // Get MediaBox (required) or use default A4
            let (width_pt, height_pt) =
                Self::get_page_size(doc, page_dict).unwrap_or((595.0, 842.0)); // A4 default

            // Get rotation
            let rotation = page_dict
                .get(b"Rotate")
                .ok()
                .and_then(|obj| obj.as_i64().ok())
                .map(|r| (r % 360) as u16)
                .unwrap_or(0);

            // Check for images (simplified check)
            let has_images = page_dict.has(b"Resources")
                && doc
                    .get_dictionary(
                        page_dict
                            .get(b"Resources")
                            .ok()
                            .and_then(|r| r.as_reference().ok())
                            .unwrap_or((0, 0)),
                    )
                    .map(|res| res.has(b"XObject"))
                    .unwrap_or(false);

            // Check for text (simplified check - presence of Contents)
            let has_text = page_dict.has(b"Contents");

            pages.push(PdfPage {
                index,
                width_pt,
                height_pt,
                rotation,
                has_images,
                has_text,
            });
        }

        Ok(pages)
    }

    /// Get page dimensions from MediaBox or CropBox
    fn get_page_size(doc: &Document, page_dict: &lopdf::Dictionary) -> Option<(f64, f64)> {
        // Try CropBox first, then MediaBox
        for key in &[b"CropBox".as_slice(), b"MediaBox".as_slice()] {
            if let Ok(box_obj) = page_dict.get(key) {
                if let Ok(box_arr) = Self::resolve_array(doc, box_obj) {
                    if box_arr.len() >= 4 {
                        let x1 = Self::get_number(&box_arr[0]).unwrap_or(0.0);
                        let y1 = Self::get_number(&box_arr[1]).unwrap_or(0.0);
                        let x2 = Self::get_number(&box_arr[2]).unwrap_or(595.0);
                        let y2 = Self::get_number(&box_arr[3]).unwrap_or(842.0);
                        return Some(((x2 - x1).abs(), (y2 - y1).abs()));
                    }
                }
            }
        }
        None
    }

    /// Resolve an object to an array (following references)
    fn resolve_array<'a>(doc: &'a Document, obj: &'a lopdf::Object) -> Result<Vec<lopdf::Object>> {
        match obj {
            lopdf::Object::Array(arr) => Ok(arr.clone()),
            lopdf::Object::Reference(id) => {
                let resolved = doc
                    .get_object(*id)
                    .map_err(|e| PdfReaderError::ParseError(e.to_string()))?;
                Self::resolve_array(doc, resolved)
            }
            _ => Err(PdfReaderError::ParseError("Expected array".to_string())),
        }
    }

    /// Extract number from PDF object
    fn get_number(obj: &lopdf::Object) -> Option<f64> {
        match obj {
            lopdf::Object::Integer(i) => Some(*i as f64),
            lopdf::Object::Real(f) => Some(*f as f64),
            _ => None,
        }
    }

    /// Get page by index
    pub fn get_page(&self, index: usize) -> Result<&PdfPage> {
        self.info
            .pages
            .get(index)
            .ok_or_else(|| PdfReaderError::ParseError(format!("Page {} not found", index)))
    }

    /// Get metadata
    pub fn metadata(&self) -> &PdfMetadata {
        &self.info.metadata
    }

    /// Check if encrypted
    pub fn is_encrypted(&self) -> bool {
        self.info.is_encrypted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // TC-PDR-002: 存在しないファイル
    #[test]
    fn test_open_nonexistent_file() {
        let result = LopdfReader::new("/nonexistent/file.pdf");
        assert!(matches!(result, Err(PdfReaderError::FileNotFound(_))));
    }

    // TC-PDR-003: 無効なPDFフォーマット
    #[test]
    fn test_open_invalid_pdf() {
        // Create a non-PDF file
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "This is not a PDF").unwrap();

        let result = LopdfReader::new(temp.path());
        assert!(matches!(
            result,
            Err(PdfReaderError::InvalidFormat(_) | PdfReaderError::ParseError(_))
        ));
    }

    // PDF fixture tests

    // TC-PDR-001: 正常なPDF読み込み
    #[test]
    fn test_open_valid_pdf() {
        let path = PathBuf::from("tests/fixtures/sample.pdf");
        let doc = LopdfReader::new(&path).unwrap();

        assert!(doc.info.page_count > 0);
        assert_eq!(doc.info.path, path);
    }

    // TC-PDR-004: ページ数取得
    #[test]
    fn test_page_count() {
        let doc = LopdfReader::new("tests/fixtures/10pages.pdf").unwrap();
        assert_eq!(doc.info.page_count, 10);
    }

    // TC-PDR-005: ページサイズ取得
    #[test]
    fn test_page_dimensions() {
        let doc = LopdfReader::new("tests/fixtures/a4.pdf").unwrap();
        let page = doc.get_page(0).unwrap();

        // A4: 595 x 842 points
        assert!((page.width_pt - 595.0).abs() < 1.0);
        assert!((page.height_pt - 842.0).abs() < 1.0);
    }

    // TC-PDR-006: メタデータ抽出
    #[test]
    fn test_metadata_extraction() {
        let doc = LopdfReader::new("tests/fixtures/with_metadata.pdf").unwrap();
        let meta = doc.metadata();

        assert!(meta.title.is_some());
        assert!(meta.author.is_some());
    }

    // TC-PDR-007: 回転ページの検出
    #[test]
    fn test_rotated_page() {
        let doc = LopdfReader::new("tests/fixtures/rotated.pdf").unwrap();
        let page = doc.get_page(0).unwrap();

        assert_eq!(page.rotation, 90);
    }

    // TC-PDR-008: 暗号化PDF検出
    #[test]
    fn test_encrypted_pdf_detection() {
        let doc = LopdfReader::new("tests/fixtures/encrypted.pdf").unwrap();
        assert!(doc.is_encrypted());
    }

    // TC-PDR-009: Large PDF memory efficiency
    // Requires large_1000pages.pdf fixture and procfs dependency
    #[test]
    #[ignore = "requires external tool"]
    fn test_large_pdf_memory() {
        let doc = LopdfReader::new("tests/fixtures/large_1000pages.pdf").unwrap();
        assert_eq!(doc.info.page_count, 1000);
        // Memory usage check would require procfs crate
    }

    // TC-PDR-010: Concurrent open
    #[test]
    fn test_concurrent_open() {
        use rayon::prelude::*;

        // Use existing fixture files for concurrent test
        let paths = vec![
            "tests/fixtures/sample.pdf",
            "tests/fixtures/a4.pdf",
            "tests/fixtures/10pages.pdf",
            "tests/fixtures/with_metadata.pdf",
        ];

        let results: Vec<_> = paths.par_iter().map(LopdfReader::new).collect();

        assert!(results.iter().all(|r| r.is_ok()));
    }

    // Additional structure tests

    #[test]
    fn test_pdf_document_structure() {
        let doc = PdfDocument {
            path: PathBuf::from("/test/path.pdf"),
            page_count: 5,
            metadata: PdfMetadata::default(),
            pages: vec![],
            is_encrypted: false,
        };

        assert_eq!(doc.path, PathBuf::from("/test/path.pdf"));
        assert_eq!(doc.page_count, 5);
        assert!(!doc.is_encrypted);
    }

    #[test]
    fn test_pdf_metadata_construction() {
        let metadata = PdfMetadata {
            title: Some("Test Title".to_string()),
            author: Some("Test Author".to_string()),
            subject: Some("Test Subject".to_string()),
            keywords: Some("test, keywords".to_string()),
            creator: Some("Test Creator".to_string()),
            producer: Some("Test Producer".to_string()),
            creation_date: Some("D:20240101120000".to_string()),
            modification_date: Some("D:20240102120000".to_string()),
        };

        assert_eq!(metadata.title, Some("Test Title".to_string()));
        assert_eq!(metadata.author, Some("Test Author".to_string()));
        assert!(metadata.creation_date.is_some());
    }

    #[test]
    fn test_pdf_page_structure() {
        let page = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 90,
            has_images: true,
            has_text: true,
        };

        assert_eq!(page.index, 0);
        assert_eq!(page.width_pt, 595.0);
        assert_eq!(page.height_pt, 842.0);
        assert_eq!(page.rotation, 90);
        assert!(page.has_images);
        assert!(page.has_text);
    }

    #[test]
    fn test_error_types() {
        // Test all error variants can be constructed
        let _err1 = PdfReaderError::FileNotFound(PathBuf::from("/test/path"));
        let _err2 = PdfReaderError::InvalidFormat("Invalid format".to_string());
        let _err3 = PdfReaderError::EncryptedPdf;
        let _err4 = PdfReaderError::ParseError("Parse error".to_string());
        let _err5: PdfReaderError =
            std::io::Error::new(std::io::ErrorKind::NotFound, "test").into();
    }

    #[test]
    fn test_default_metadata() {
        let metadata = PdfMetadata::default();

        assert!(metadata.title.is_none());
        assert!(metadata.author.is_none());
        assert!(metadata.subject.is_none());
        assert!(metadata.keywords.is_none());
        assert!(metadata.creator.is_none());
        assert!(metadata.producer.is_none());
        assert!(metadata.creation_date.is_none());
        assert!(metadata.modification_date.is_none());
    }

    #[test]
    fn test_page_index_out_of_bounds() {
        let doc = LopdfReader::new("tests/fixtures/sample.pdf").unwrap();

        // Try to get page beyond count
        let result = doc.get_page(9999);
        assert!(result.is_err());
    }

    // Additional tests for spec coverage

    #[test]
    fn test_pages_iterator() {
        let doc = LopdfReader::new("tests/fixtures/10pages.pdf").unwrap();

        // Iterate over all pages
        let page_count = doc.info.pages.len();
        assert_eq!(page_count, doc.info.page_count);
    }

    #[test]
    fn test_page_rotation_values() {
        // Test that rotation is normalized to valid values
        let page = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 270,
            has_images: false,
            has_text: true,
        };

        // Valid rotations: 0, 90, 180, 270
        assert!(
            page.rotation == 0
                || page.rotation == 90
                || page.rotation == 180
                || page.rotation == 270
        );
    }

    #[test]
    fn test_error_display_messages() {
        let err1 = PdfReaderError::FileNotFound(PathBuf::from("/test/path.pdf"));
        assert!(err1.to_string().contains("not found"));

        let err2 = PdfReaderError::InvalidFormat("bad header".to_string());
        assert!(err2.to_string().contains("Invalid"));

        let err3 = PdfReaderError::EncryptedPdf;
        assert!(err3.to_string().contains("ncrypted"));

        let err4 = PdfReaderError::ParseError("parse failed".to_string());
        assert!(err4.to_string().contains("error"));
    }

    #[test]
    fn test_metadata_clone() {
        let metadata = PdfMetadata {
            title: Some("Test Title".to_string()),
            author: Some("Test Author".to_string()),
            subject: None,
            keywords: None,
            creator: None,
            producer: None,
            creation_date: None,
            modification_date: None,
        };

        let cloned = metadata.clone();
        assert_eq!(cloned.title, metadata.title);
        assert_eq!(cloned.author, metadata.author);
    }

    #[test]
    fn test_pdf_document_clone() {
        let doc = PdfDocument {
            path: PathBuf::from("/test/path.pdf"),
            page_count: 10,
            metadata: PdfMetadata::default(),
            pages: vec![],
            is_encrypted: false,
        };

        let cloned = doc.clone();
        assert_eq!(cloned.path, doc.path);
        assert_eq!(cloned.page_count, doc.page_count);
        assert_eq!(cloned.is_encrypted, doc.is_encrypted);
    }

    #[test]
    fn test_page_dimensions_calculation() {
        let page = PdfPage {
            index: 0,
            width_pt: 595.0,  // A4 width in points
            height_pt: 842.0, // A4 height in points
            rotation: 0,
            has_images: true,
            has_text: true,
        };

        // A4 is 210mm x 297mm, 1 inch = 72 points, 1 inch = 25.4mm
        // width_mm = 595 / 72 * 25.4 ≈ 210
        let width_mm = page.width_pt / 72.0 * 25.4;
        let height_mm = page.height_pt / 72.0 * 25.4;

        assert!((width_mm - 210.0).abs() < 1.0);
        assert!((height_mm - 297.0).abs() < 1.0);
    }

    // Test page rotation effect on dimensions
    #[test]
    fn test_page_rotation_dimensions() {
        // Portrait page
        let portrait = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };
        assert!(portrait.height_pt > portrait.width_pt);

        // Same page rotated 90 degrees would appear landscape
        let rotated = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 90,
            has_images: false,
            has_text: false,
        };
        // After 90 degree rotation, effective dimensions swap
        assert_eq!(rotated.rotation, 90);
    }

    // Test all rotation values
    #[test]
    fn test_all_rotation_values() {
        let rotations = [0, 90, 180, 270];

        for rotation in rotations {
            let page = PdfPage {
                index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation,
                has_images: false,
                has_text: false,
            };
            assert!(page.rotation.is_multiple_of(90));
            assert!(page.rotation < 360);
        }
    }

    // Test metadata with all fields populated
    #[test]
    fn test_metadata_all_fields() {
        let metadata = PdfMetadata {
            title: Some("Complete Document".to_string()),
            author: Some("John Doe".to_string()),
            subject: Some("Testing".to_string()),
            keywords: Some("test, pdf, rust".to_string()),
            creator: Some("Test Creator".to_string()),
            producer: Some("superbook-pdf".to_string()),
            creation_date: Some("2024-01-01".to_string()),
            modification_date: Some("2024-01-02".to_string()),
        };

        assert!(metadata.title.is_some());
        assert!(metadata.author.is_some());
        assert!(metadata.subject.is_some());
        assert!(metadata.keywords.is_some());
        assert!(metadata.creator.is_some());
        assert!(metadata.producer.is_some());
        assert!(metadata.creation_date.is_some());
        assert!(metadata.modification_date.is_some());
    }

    // Test PdfDocument with pages
    #[test]
    fn test_document_with_pages() {
        let pages: Vec<PdfPage> = (0..5)
            .map(|i| PdfPage {
                index: i,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: 0,
                has_images: i % 2 == 0,
                has_text: true,
            })
            .collect();

        let doc = PdfDocument {
            path: PathBuf::from("/test/doc.pdf"),
            page_count: 5,
            metadata: PdfMetadata::default(),
            pages: pages.clone(),
            is_encrypted: false,
        };

        assert_eq!(doc.pages.len(), 5);
        assert_eq!(doc.page_count, 5);

        // Check page indices are sequential
        for (i, page) in doc.pages.iter().enumerate() {
            assert_eq!(page.index, i);
        }
    }

    // Test encrypted document flag
    #[test]
    fn test_encrypted_document() {
        let encrypted_doc = PdfDocument {
            path: PathBuf::from("/test/encrypted.pdf"),
            page_count: 1,
            metadata: PdfMetadata::default(),
            pages: vec![],
            is_encrypted: true,
        };

        assert!(encrypted_doc.is_encrypted);

        let normal_doc = PdfDocument {
            path: PathBuf::from("/test/normal.pdf"),
            page_count: 1,
            metadata: PdfMetadata::default(),
            pages: vec![],
            is_encrypted: false,
        };

        assert!(!normal_doc.is_encrypted);
    }

    // Test page with only images
    #[test]
    fn test_page_images_only() {
        let page = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 0,
            has_images: true,
            has_text: false,
        };

        assert!(page.has_images);
        assert!(!page.has_text);
    }

    // Test page with only text
    #[test]
    fn test_page_text_only() {
        let page = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 0,
            has_images: false,
            has_text: true,
        };

        assert!(!page.has_images);
        assert!(page.has_text);
    }

    // Test empty page
    #[test]
    fn test_empty_page() {
        let page = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };

        assert!(!page.has_images);
        assert!(!page.has_text);
    }

    // Test various page sizes
    #[test]
    fn test_various_page_sizes() {
        // A4 (210 x 297 mm)
        let a4 = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };

        // Letter (8.5 x 11 inches = 612 x 792 points)
        let letter = PdfPage {
            index: 0,
            width_pt: 612.0,
            height_pt: 792.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };

        // Legal (8.5 x 14 inches = 612 x 1008 points)
        let legal = PdfPage {
            index: 0,
            width_pt: 612.0,
            height_pt: 1008.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };

        assert!((a4.width_pt - 595.0).abs() < 1.0);
        assert!((letter.width_pt - 612.0).abs() < 1.0);
        assert!((legal.height_pt - 1008.0).abs() < 1.0);
    }

    // Test IO error conversion
    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let pdf_err: PdfReaderError = io_err.into();

        let msg = pdf_err.to_string().to_lowercase();
        assert!(msg.contains("io") || msg.contains("error"));
    }

    // Test document path handling
    #[test]
    fn test_document_path() {
        let doc = PdfDocument {
            path: PathBuf::from("/long/path/to/document.pdf"),
            page_count: 1,
            metadata: PdfMetadata::default(),
            pages: vec![],
            is_encrypted: false,
        };

        assert_eq!(doc.path.file_name().unwrap(), "document.pdf");
        assert!(doc.path.is_absolute());
    }

    // Additional comprehensive tests

    #[test]
    fn test_pdf_page_debug_impl() {
        let page = PdfPage {
            index: 5,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 90,
            has_images: true,
            has_text: true,
        };

        let debug_str = format!("{:?}", page);
        assert!(debug_str.contains("PdfPage"));
        assert!(debug_str.contains("595"));
        assert!(debug_str.contains("90"));
    }

    #[test]
    fn test_pdf_document_debug_impl() {
        let doc = PdfDocument {
            path: PathBuf::from("/test.pdf"),
            page_count: 10,
            metadata: PdfMetadata::default(),
            pages: vec![],
            is_encrypted: false,
        };

        let debug_str = format!("{:?}", doc);
        assert!(debug_str.contains("PdfDocument"));
        assert!(debug_str.contains("10"));
    }

    #[test]
    fn test_pdf_metadata_debug_impl() {
        let meta = PdfMetadata {
            title: Some("Debug Test".to_string()),
            ..Default::default()
        };

        let debug_str = format!("{:?}", meta);
        assert!(debug_str.contains("PdfMetadata"));
        assert!(debug_str.contains("Debug Test"));
    }

    #[test]
    fn test_error_debug_impl() {
        let err = PdfReaderError::EncryptedPdf;
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("EncryptedPdf"));
    }

    #[test]
    fn test_metadata_default_all_none() {
        let meta = PdfMetadata::default();
        assert!(meta.title.is_none());
        assert!(meta.author.is_none());
        assert!(meta.subject.is_none());
        assert!(meta.keywords.is_none());
        assert!(meta.creator.is_none());
        assert!(meta.producer.is_none());
        assert!(meta.creation_date.is_none());
        assert!(meta.modification_date.is_none());
    }

    #[test]
    fn test_page_size_extreme_small() {
        let tiny = PdfPage {
            index: 0,
            width_pt: 72.0,  // 1 inch
            height_pt: 72.0, // 1 inch square
            rotation: 0,
            has_images: false,
            has_text: false,
        };

        assert_eq!(tiny.width_pt, tiny.height_pt);
    }

    #[test]
    fn test_page_size_extreme_large() {
        let huge = PdfPage {
            index: 0,
            width_pt: 14400.0, // 200 inches wide
            height_pt: 14400.0,
            rotation: 0,
            has_images: true,
            has_text: false,
        };

        assert!(huge.width_pt > 10000.0);
    }

    #[test]
    fn test_document_many_pages() {
        let pages: Vec<PdfPage> = (0..1000)
            .map(|i| PdfPage {
                index: i,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: (i % 4) as u16 * 90,
                has_images: i % 3 == 0,
                has_text: i % 2 == 0,
            })
            .collect();

        let doc = PdfDocument {
            path: PathBuf::from("/large_book.pdf"),
            page_count: 1000,
            metadata: PdfMetadata::default(),
            pages,
            is_encrypted: false,
        };

        assert_eq!(doc.pages.len(), 1000);
        assert_eq!(doc.page_count, 1000);
    }

    #[test]
    fn test_page_clone() {
        let original = PdfPage {
            index: 42,
            width_pt: 612.0,
            height_pt: 792.0,
            rotation: 180,
            has_images: true,
            has_text: true,
        };

        let cloned = original.clone();
        assert_eq!(cloned.index, original.index);
        assert_eq!(cloned.width_pt, original.width_pt);
        assert_eq!(cloned.rotation, original.rotation);
    }

    #[test]
    fn test_error_all_variants() {
        let errors = [
            PdfReaderError::FileNotFound(PathBuf::from("/not/found.pdf")),
            PdfReaderError::InvalidFormat("corrupt header".to_string()),
            PdfReaderError::EncryptedPdf,
            PdfReaderError::ParseError("parse issue".to_string()),
        ];

        for err in &errors {
            let msg = err.to_string();
            assert!(!msg.is_empty());
        }
    }

    #[test]
    fn test_metadata_keywords_parsing() {
        let meta = PdfMetadata {
            keywords: Some("rust, pdf, parsing, test".to_string()),
            ..Default::default()
        };

        let keywords = meta.keywords.as_ref().unwrap();
        assert!(keywords.contains("rust"));
        assert!(keywords.contains("pdf"));
        assert!(keywords.contains("parsing"));
    }

    #[test]
    fn test_metadata_japanese_content() {
        let meta = PdfMetadata {
            title: Some("日本語タイトル".to_string()),
            author: Some("山田太郎".to_string()),
            subject: Some("テスト文書".to_string()),
            ..Default::default()
        };

        assert!(meta.title.as_ref().unwrap().contains("日本語"));
        assert!(meta.author.as_ref().unwrap().contains("山田"));
    }

    #[test]
    fn test_page_aspect_ratios() {
        // Portrait
        let portrait = PdfPage {
            index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };
        let portrait_ratio = portrait.height_pt / portrait.width_pt;
        assert!(portrait_ratio > 1.0); // Taller than wide

        // Landscape
        let landscape = PdfPage {
            index: 0,
            width_pt: 842.0,
            height_pt: 595.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };
        let landscape_ratio = landscape.height_pt / landscape.width_pt;
        assert!(landscape_ratio < 1.0); // Wider than tall

        // Square
        let square = PdfPage {
            index: 0,
            width_pt: 500.0,
            height_pt: 500.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };
        let square_ratio = square.height_pt / square.width_pt;
        assert!((square_ratio - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_document_with_mixed_page_sizes() {
        let pages = vec![
            PdfPage {
                index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: 0,
                has_images: true,
                has_text: true,
            },
            PdfPage {
                index: 1,
                width_pt: 612.0,
                height_pt: 792.0,
                rotation: 0,
                has_images: false,
                has_text: true,
            },
            PdfPage {
                index: 2,
                width_pt: 842.0,
                height_pt: 595.0,
                rotation: 90,
                has_images: true,
                has_text: false,
            },
        ];

        let doc = PdfDocument {
            path: PathBuf::from("/mixed.pdf"),
            page_count: 3,
            metadata: PdfMetadata::default(),
            pages,
            is_encrypted: false,
        };

        // Verify different page sizes
        assert_ne!(doc.pages[0].width_pt, doc.pages[1].width_pt);
        assert_ne!(doc.pages[1].height_pt, doc.pages[2].height_pt);
    }

    #[test]
    fn test_lopdf_reader_construction() {
        // LopdfReader requires a valid PDF path
        // Test that it returns error for nonexistent file
        let result = LopdfReader::new("/nonexistent/file.pdf");
        assert!(result.is_err());
    }

    #[test]
    fn test_page_index_sequential() {
        let pages: Vec<PdfPage> = (0..50)
            .map(|i| PdfPage {
                index: i,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: 0,
                has_images: false,
                has_text: false,
            })
            .collect();

        for (expected_idx, page) in pages.iter().enumerate() {
            assert_eq!(page.index, expected_idx);
        }
    }

    #[test]
    fn test_metadata_dates_format() {
        let meta = PdfMetadata {
            creation_date: Some("D:20240101120000+09'00'".to_string()),
            modification_date: Some("D:20240115093000Z".to_string()),
            ..Default::default()
        };

        // PDF date format starts with D:
        assert!(meta.creation_date.as_ref().unwrap().starts_with("D:"));
        assert!(meta.modification_date.as_ref().unwrap().starts_with("D:"));
    }

    #[test]
    fn test_document_zero_pages() {
        let doc = PdfDocument {
            path: PathBuf::from("/empty.pdf"),
            page_count: 0,
            metadata: PdfMetadata::default(),
            pages: vec![],
            is_encrypted: false,
        };

        assert_eq!(doc.page_count, 0);
        assert!(doc.pages.is_empty());
    }

    #[test]
    fn test_error_file_not_found_path() {
        let path = PathBuf::from("/very/long/path/to/missing/document.pdf");
        let err = PdfReaderError::FileNotFound(path.clone());

        let msg = err.to_string();
        assert!(msg.contains("document.pdf") || msg.contains("not found"));
    }

    #[test]
    fn test_parse_error_details() {
        let details = "Unexpected token at byte 12345";
        let err = PdfReaderError::ParseError(details.to_string());

        let msg = err.to_string();
        assert!(msg.contains("12345") || msg.contains("error"));
    }

    #[test]
    fn test_invalid_format_error() {
        let reason = "Missing PDF header %PDF-";
        let err = PdfReaderError::InvalidFormat(reason.to_string());

        let msg = err.to_string();
        assert!(msg.contains("Invalid") || msg.contains("format"));
    }

    #[test]
    fn test_page_content_combinations() {
        // All combinations of has_images and has_text
        let combinations = [
            (false, false), // Empty page
            (true, false),  // Image only
            (false, true),  // Text only
            (true, true),   // Both
        ];

        for (has_images, has_text) in combinations {
            let page = PdfPage {
                index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: 0,
                has_images,
                has_text,
            };

            assert_eq!(page.has_images, has_images);
            assert_eq!(page.has_text, has_text);
        }
    }

    // ============================================================
    // Error handling tests
    // ============================================================

    #[test]
    fn test_error_file_not_found_display() {
        let path = PathBuf::from("/test/missing.pdf");
        let err = PdfReaderError::FileNotFound(path);
        let msg = format!("{}", err);
        assert!(msg.contains("File not found"));
        assert!(msg.contains("missing.pdf"));
    }

    #[test]
    fn test_error_file_not_found_debug() {
        let path = PathBuf::from("/test/missing.pdf");
        let err = PdfReaderError::FileNotFound(path);
        let debug = format!("{:?}", err);
        assert!(debug.contains("FileNotFound"));
    }

    #[test]
    fn test_error_invalid_format_display() {
        let err = PdfReaderError::InvalidFormat("not a PDF".to_string());
        let msg = format!("{}", err);
        assert!(msg.contains("Invalid PDF format"));
        assert!(msg.contains("not a PDF"));
    }

    #[test]
    fn test_error_invalid_format_debug() {
        let err = PdfReaderError::InvalidFormat("corrupted header".to_string());
        let debug = format!("{:?}", err);
        assert!(debug.contains("InvalidFormat"));
    }

    #[test]
    fn test_error_encrypted_pdf_display() {
        let err = PdfReaderError::EncryptedPdf;
        let msg = format!("{}", err);
        assert!(msg.contains("Encrypted PDF not supported"));
    }

    #[test]
    fn test_error_encrypted_pdf_debug() {
        let err = PdfReaderError::EncryptedPdf;
        let debug = format!("{:?}", err);
        assert!(debug.contains("EncryptedPdf"));
    }

    #[test]
    fn test_error_io_error_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err = PdfReaderError::IoError(io_err);
        let msg = format!("{}", err);
        assert!(msg.contains("IO error"));
    }

    #[test]
    fn test_error_io_error_debug() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = PdfReaderError::IoError(io_err);
        let debug = format!("{:?}", err);
        assert!(debug.contains("IoError"));
    }

    #[test]
    fn test_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "pdf not found");
        let pdf_err: PdfReaderError = io_err.into();
        let msg = format!("{}", pdf_err);
        assert!(msg.contains("IO error"));
    }

    #[test]
    fn test_error_parse_error_display() {
        let err = PdfReaderError::ParseError("invalid object reference".to_string());
        let msg = format!("{}", err);
        assert!(msg.contains("PDF parse error"));
        assert!(msg.contains("invalid object reference"));
    }

    #[test]
    fn test_error_parse_error_debug() {
        let err = PdfReaderError::ParseError("malformed stream".to_string());
        let debug = format!("{:?}", err);
        assert!(debug.contains("ParseError"));
    }

    #[test]
    fn test_error_all_variants_debug_display() {
        let errors: Vec<PdfReaderError> = vec![
            PdfReaderError::FileNotFound(PathBuf::from("/test.pdf")),
            PdfReaderError::InvalidFormat("bad format".to_string()),
            PdfReaderError::EncryptedPdf,
            PdfReaderError::IoError(std::io::Error::other("io")),
            PdfReaderError::ParseError("parse fail".to_string()),
        ];

        for err in &errors {
            let debug = format!("{:?}", err);
            assert!(!debug.is_empty());
            let display = format!("{}", err);
            assert!(!display.is_empty());
        }
    }

    #[test]
    fn test_error_invalid_format_empty_message() {
        let err = PdfReaderError::InvalidFormat(String::new());
        let msg = format!("{}", err);
        assert!(msg.contains("Invalid PDF format"));
    }

    #[test]
    fn test_error_parse_error_special_chars() {
        let err = PdfReaderError::ParseError("line: 42, col: 10".to_string());
        let msg = format!("{}", err);
        assert!(msg.contains("line: 42"));
    }

    // ==================== Concurrency Tests ====================

    #[test]
    fn test_pdf_reader_types_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PdfDocument>();
        assert_send_sync::<PdfMetadata>();
        assert_send_sync::<PdfPage>();
    }

    #[test]
    fn test_concurrent_pdf_document_creation() {
        use std::thread;

        let handles: Vec<_> = (0..4)
            .map(|i| {
                thread::spawn(move || -> PdfDocument {
                    PdfDocument {
                        page_count: i + 1,
                        pages: vec![],
                        metadata: PdfMetadata::default(),
                        path: PathBuf::from(format!("/doc_{}.pdf", i)),
                        is_encrypted: false,
                    }
                })
            })
            .collect();

        for (i, handle) in handles.into_iter().enumerate() {
            let doc: PdfDocument = handle.join().unwrap();
            assert_eq!(doc.page_count, i + 1);
            assert!(!doc.is_encrypted);
        }
    }

    #[test]
    fn test_concurrent_pdf_page_creation() {
        use rayon::prelude::*;

        let pages: Vec<_> = (0..100)
            .into_par_iter()
            .map(|i| PdfPage {
                index: i,
                width_pt: 595.0 + i as f64,
                height_pt: 842.0 + i as f64,
                rotation: if i % 2 == 0 { 0 } else { 90 },
                has_images: true,
                has_text: false,
            })
            .collect();

        assert_eq!(pages.len(), 100);
        assert_eq!(pages[50].width_pt, 645.0);
        assert_eq!(pages[50].rotation, 0);
        assert_eq!(pages[51].rotation, 90);
    }

    #[test]
    fn test_metadata_thread_transfer() {
        use std::thread;

        let metadata = PdfMetadata {
            title: Some("Test Document".to_string()),
            author: Some("Test Author".to_string()),
            subject: None,
            keywords: None,
            creator: Some("Test Creator".to_string()),
            producer: None,
            creation_date: None,
            modification_date: None,
        };

        let handle = thread::spawn(move || -> PdfMetadata {
            assert_eq!(metadata.title, Some("Test Document".to_string()));
            metadata
        });

        let received: PdfMetadata = handle.join().unwrap();
        assert_eq!(received.author, Some("Test Author".to_string()));
    }

    #[test]
    fn test_pdf_document_shared_read() {
        use std::sync::Arc;
        use std::thread;

        let doc = Arc::new(PdfDocument {
            page_count: 10,
            pages: vec![PdfPage {
                index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: 0,
                has_images: true,
                has_text: true,
            }],
            metadata: PdfMetadata::default(),
            path: PathBuf::from("/shared.pdf"),
            is_encrypted: false,
        });

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let d = Arc::clone(&doc);
                thread::spawn(move || -> usize {
                    assert_eq!(d.page_count, 10);
                    assert!(!d.is_encrypted);
                    d.pages.len()
                })
            })
            .collect();

        for handle in handles {
            let len: usize = handle.join().unwrap();
            assert_eq!(len, 1);
        }
    }

    #[test]
    fn test_parallel_error_creation() {
        use rayon::prelude::*;

        let errors: Vec<_> = (0..50)
            .into_par_iter()
            .map(|i| {
                if i % 3 == 0 {
                    PdfReaderError::FileNotFound(PathBuf::from(format!("/file_{}.pdf", i)))
                } else if i % 3 == 1 {
                    PdfReaderError::InvalidFormat(format!("invalid_{}", i))
                } else {
                    PdfReaderError::EncryptedPdf
                }
            })
            .collect();

        assert_eq!(errors.len(), 50);

        let encrypted_count = errors
            .iter()
            .filter(|e| matches!(e, PdfReaderError::EncryptedPdf))
            .count();
        assert!(encrypted_count > 0);
    }

    // ============ Additional Concurrency Tests ============

    #[test]
    fn test_all_types_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PdfDocument>();
        assert_send_sync::<PdfMetadata>();
        assert_send_sync::<PdfPage>();
        assert_send_sync::<PdfReaderError>();
        assert_send_sync::<LopdfReader>();
    }

    #[test]
    fn test_concurrent_metadata_creation() {
        use std::thread;

        let handles: Vec<_> = (0..8)
            .map(|i| {
                thread::spawn(move || PdfMetadata {
                    title: Some(format!("Title {}", i)),
                    author: Some(format!("Author {}", i)),
                    subject: Some(format!("Subject {}", i)),
                    keywords: Some(format!("keyword{}", i)),
                    creator: Some("Creator".to_string()),
                    producer: Some("Producer".to_string()),
                    creation_date: Some("2024-01-01".to_string()),
                    modification_date: Some("2024-12-01".to_string()),
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.len(), 8);
        for (i, meta) in results.iter().enumerate() {
            assert_eq!(meta.title, Some(format!("Title {}", i)));
        }
    }

    #[test]
    fn test_concurrent_page_creation() {
        use rayon::prelude::*;

        let pages: Vec<_> = (0..100)
            .into_par_iter()
            .map(|i| PdfPage {
                index: i,
                width_pt: 595.0 + (i as f64 * 0.1),
                height_pt: 842.0 + (i as f64 * 0.1),
                rotation: (i % 4 * 90) as u16,
                has_images: i % 2 == 0,
                has_text: i % 3 != 0,
            })
            .collect();

        assert_eq!(pages.len(), 100);
        for (i, page) in pages.iter().enumerate() {
            assert_eq!(page.index, i);
            assert_eq!(page.rotation, (i % 4 * 90) as u16);
        }
    }

    #[test]
    fn test_pdf_page_thread_transfer() {
        use std::thread;

        let page = PdfPage {
            index: 42,
            width_pt: 612.0,
            height_pt: 792.0,
            rotation: 90,
            has_images: true,
            has_text: true,
        };

        let handle = thread::spawn(move || {
            assert_eq!(page.index, 42);
            assert_eq!(page.rotation, 90);
            page.width_pt + page.height_pt
        });

        let result = handle.join().unwrap();
        assert!((result - 1404.0).abs() < 0.01);
    }

    // ============ Additional Boundary Tests ============

    #[test]
    fn test_page_dimensions_zero() {
        let page = PdfPage {
            index: 0,
            width_pt: 0.0,
            height_pt: 0.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };
        assert_eq!(page.width_pt, 0.0);
        assert_eq!(page.height_pt, 0.0);
    }

    #[test]
    fn test_page_dimensions_large() {
        // Poster size (A0 in points: 2384 x 3370)
        let page = PdfPage {
            index: 0,
            width_pt: 2384.0,
            height_pt: 3370.0,
            rotation: 0,
            has_images: true,
            has_text: true,
        };
        assert!(page.width_pt > 2000.0);
        assert!(page.height_pt > 3000.0);
    }

    #[test]
    fn test_page_rotation_all_values() {
        let rotations = [0u16, 90, 180, 270];
        for &rot in &rotations {
            let page = PdfPage {
                index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: rot,
                has_images: false,
                has_text: false,
            };
            assert_eq!(page.rotation, rot);
        }
    }

    #[test]
    fn test_page_index_maximum() {
        let page = PdfPage {
            index: usize::MAX,
            width_pt: 595.0,
            height_pt: 842.0,
            rotation: 0,
            has_images: false,
            has_text: false,
        };
        assert_eq!(page.index, usize::MAX);
    }

    #[test]
    fn test_metadata_all_fields_none() {
        let meta = PdfMetadata::default();
        assert!(meta.title.is_none());
        assert!(meta.author.is_none());
        assert!(meta.subject.is_none());
        assert!(meta.keywords.is_none());
        assert!(meta.creator.is_none());
        assert!(meta.producer.is_none());
        assert!(meta.creation_date.is_none());
        assert!(meta.modification_date.is_none());
    }

    #[test]
    fn test_metadata_all_fields_some() {
        let meta = PdfMetadata {
            title: Some("Title".to_string()),
            author: Some("Author".to_string()),
            subject: Some("Subject".to_string()),
            keywords: Some("key1, key2".to_string()),
            creator: Some("Creator".to_string()),
            producer: Some("Producer".to_string()),
            creation_date: Some("D:20240101120000".to_string()),
            modification_date: Some("D:20241201120000".to_string()),
        };
        assert!(meta.title.is_some());
        assert!(meta.author.is_some());
        assert!(meta.subject.is_some());
        assert!(meta.keywords.is_some());
        assert!(meta.creator.is_some());
        assert!(meta.producer.is_some());
        assert!(meta.creation_date.is_some());
        assert!(meta.modification_date.is_some());
    }

    #[test]
    fn test_metadata_unicode_content() {
        let meta = PdfMetadata {
            title: Some("日本語タイトル".to_string()),
            author: Some("著者名".to_string()),
            subject: Some("主題".to_string()),
            keywords: Some("キーワード1, キーワード2".to_string()),
            creator: Some("作成者".to_string()),
            producer: Some("プロデューサー".to_string()),
            creation_date: None,
            modification_date: None,
        };
        assert!(meta.title.as_ref().unwrap().contains("日本語"));
        assert!(meta.author.as_ref().unwrap().contains("著者"));
    }

    #[test]
    fn test_document_zero_pages_boundary() {
        let doc = PdfDocument {
            path: PathBuf::from("empty.pdf"),
            page_count: 0,
            pages: vec![],
            metadata: PdfMetadata::default(),
            is_encrypted: false,
        };
        assert_eq!(doc.page_count, 0);
        assert!(doc.pages.is_empty());
    }

    #[test]
    fn test_document_many_pages_boundary() {
        let pages: Vec<PdfPage> = (0..1000)
            .map(|i| PdfPage {
                index: i,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: 0,
                has_images: true,
                has_text: true,
            })
            .collect();

        let doc = PdfDocument {
            path: PathBuf::from("large.pdf"),
            page_count: 1000,
            pages,
            metadata: PdfMetadata::default(),
            is_encrypted: false,
        };
        assert_eq!(doc.page_count, 1000);
        assert_eq!(doc.pages.len(), 1000);
    }

    #[test]
    fn test_error_file_not_found_path_content() {
        let path = PathBuf::from("/nonexistent/path/file.pdf");
        let error = PdfReaderError::FileNotFound(path.clone());
        let msg = error.to_string();
        assert!(msg.contains("/nonexistent/path/file.pdf"));
    }

    #[test]
    fn test_error_invalid_format_message_content() {
        let error = PdfReaderError::InvalidFormat("magic bytes mismatch".to_string());
        let msg = error.to_string();
        assert!(msg.contains("magic bytes mismatch"));
    }

    #[test]
    fn test_page_standard_sizes() {
        // A4 (595.28 x 841.89 points)
        let a4 = PdfPage {
            index: 0,
            width_pt: 595.28,
            height_pt: 841.89,
            rotation: 0,
            has_images: false,
            has_text: true,
        };
        assert!((a4.width_pt - 595.28).abs() < 0.01);

        // Letter (612 x 792 points)
        let letter = PdfPage {
            index: 1,
            width_pt: 612.0,
            height_pt: 792.0,
            rotation: 0,
            has_images: false,
            has_text: true,
        };
        assert_eq!(letter.width_pt, 612.0);
    }

    #[test]
    fn test_document_clone() {
        let doc = PdfDocument {
            path: PathBuf::from("test.pdf"),
            page_count: 5,
            pages: vec![PdfPage {
                index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                rotation: 0,
                has_images: true,
                has_text: true,
            }],
            metadata: PdfMetadata {
                title: Some("Test".to_string()),
                ..Default::default()
            },
            is_encrypted: false,
        };

        let cloned = doc.clone();
        assert_eq!(cloned.page_count, doc.page_count);
        assert_eq!(cloned.path, doc.path);
        assert_eq!(cloned.metadata.title, doc.metadata.title);
    }

    #[test]
    fn physical_page_metadata_preserves_inherited_values_and_provenance() {
        use lopdf::{dictionary, Document, Object};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("physical-pages.pdf");
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let first_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
        });
        let second_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "CropBox" => Object::Array(vec![11.into(), 12.into(), 311.into(), 512.into()]),
            "Rotate" => Object::Integer(180),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => Object::Name(b"Pages".to_vec()),
                "Kids" => Object::Array(vec![first_id.into(), second_id.into()]),
                "Count" => Object::Integer(2),
                "MediaBox" => Object::Array(vec![1.into(), 2.into(), 401.into(), 602.into()]),
                "CropBox" => Object::Array(vec![3.into(), 4.into(), 399.into(), 600.into()]),
                "Rotate" => Object::Integer(-90),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Catalog".to_vec()),
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", catalog_id);
        doc.save(&path).unwrap();

        let reader = LopdfReader::new(&path).unwrap();
        let pages = reader.physical_pages();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].page_index, 0);
        assert_eq!(pages[0].source_page_number, 1);
        assert_eq!(pages[0].page_object_id.object_number, first_id.0);
        assert_eq!(
            pages[0]
                .media_box
                .as_ref()
                .unwrap()
                .defined_on
                .object_number,
            pages_id.0
        );
        assert_eq!(
            pages[0].media_box.as_ref().unwrap().value.coordinates(),
            [1.0, 2.0, 401.0, 602.0]
        );
        assert_eq!(
            pages[0].crop_box.as_ref().unwrap().defined_on.object_number,
            pages_id.0
        );
        assert_eq!(pages[0].raw_rotation.as_ref().unwrap().value, -90);
        assert_eq!(pages[0].normalized_rotation(), Some(270));
        assert!(pages[0].issues.is_empty());

        assert_eq!(pages[1].page_object_id.object_number, second_id.0);
        assert_eq!(
            pages[1].crop_box.as_ref().unwrap().defined_on.object_number,
            second_id.0
        );
        assert_eq!(
            pages[1]
                .raw_rotation
                .as_ref()
                .unwrap()
                .defined_on
                .object_number,
            second_id.0
        );
        assert_eq!(pages[1].normalized_rotation(), Some(180));
    }

    #[test]
    fn native_loader_does_not_enter_legacy_geometry_recursion() {
        use lopdf::dictionary;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cyclic-geometry.pdf");
        let mut doc = Document::with_version("1.7");
        let root = doc.new_object_id();
        let cyclic = doc.new_object_id();
        doc.objects.insert(cyclic, Object::Reference(cyclic));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => root,
            "MediaBox" => cyclic, "CropBox" => cyclic, "Rotate" => cyclic,
        });
        doc.objects.insert(
            root,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        doc.trailer.set("Root", catalog);
        doc.save(&path).unwrap();
        let reader = LopdfReader::new_native(&path).unwrap();
        let page = &reader.physical_pages()[0];
        assert_eq!(reader.info.page_count, 1);
        assert!(page.issues.contains(&PhysicalPageIssue::InvalidMediaBox));
        assert!(page.issues.contains(&PhysicalPageIssue::InvalidCropBox));
        assert!(page.issues.contains(&PhysicalPageIssue::InvalidRotation));
        assert!(page.effective_crop_box().is_none());
        assert!(page.normalized_rotation().is_none());
    }

    #[test]
    fn native_geometry_rejects_overflow_and_inheritance_cycles() {
        use lopdf::dictionary;
        assert!(PdfRect::try_new([-f64::MAX, 0.0, f64::MAX, 1.0]).is_err());
        let mut doc = Document::with_version("1.7");
        let page = doc.new_object_id();
        doc.objects.insert(
            page,
            Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => page,
            }),
        );
        let records = LopdfReader::physical_metadata_for_ids(&doc, vec![page]).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0]
            .issues
            .contains(&PhysicalPageIssue::InvalidParentChain));
        assert!(records[0].normalized_rotation().is_none());
        assert!(records[0].effective_crop_box().is_none());
    }

    #[test]
    fn native_page_tree_rejects_missing_duplicate_and_miscounted_children() {
        use lopdf::dictionary;
        let mut doc = Document::with_version("1.7");
        let root = doc.new_object_id();
        let leaf = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => root,
            "MediaBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
        });
        doc.objects.insert(
            root,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![leaf.into()], "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        doc.trailer.set("Root", catalog);
        assert_eq!(LopdfReader::strict_page_ids(&doc).unwrap(), vec![leaf]);
        for (kids, count) in [
            (vec![leaf.into(), leaf.into()], 2),
            (vec![Object::Reference((9999, 0))], 1),
            (vec![leaf.into()], 2),
            (vec![root.into()], 1),
            (vec![Object::Integer(42)], 1),
        ] {
            doc.objects.insert(
                root,
                Object::Dictionary(dictionary! {
                    "Type" => "Pages", "Kids" => kids, "Count" => count,
                }),
            );
            assert!(LopdfReader::strict_page_ids(&doc).is_err());
        }
    }

    #[test]
    fn native_page_tree_keeps_nested_kids_order_and_rejects_wrong_parent() {
        use lopdf::dictionary;
        let mut doc = Document::with_version("1.7");
        let root = doc.new_object_id();
        let branch = doc.new_object_id();
        let second = doc.add_object(dictionary! { "Type" => "Page", "Parent" => root });
        let first = doc.add_object(dictionary! { "Type" => "Page", "Parent" => branch });
        doc.objects.insert(
            branch,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Parent" => root, "Kids" => vec![first.into()], "Count" => 1,
            }),
        );
        doc.objects.insert(
            root,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![branch.into(), second.into()], "Count" => 2,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        doc.trailer.set("Root", catalog);
        assert_eq!(
            LopdfReader::strict_page_ids(&doc).unwrap(),
            vec![first, second]
        );
        doc.get_object_mut(first)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Parent", root);
        assert!(LopdfReader::strict_page_ids(&doc).is_err());
    }

    #[test]
    fn physical_page_metadata_reports_invalid_geometry_without_defaults() {
        use lopdf::{dictionary, Document, Object};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("invalid-physical-page.pdf");
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => Object::Name(b"Page".to_vec()),
            "Parent" => Object::Reference(pages_id),
            "Rotate" => Object::Integer(45),
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
        doc.save(&path).unwrap();

        let reader = LopdfReader::new(&path).unwrap();
        let page = &reader.physical_pages()[0];
        assert!(page.media_box.is_none());
        assert_eq!(page.normalized_rotation(), None);
        assert!(page.issues.contains(&PhysicalPageIssue::MissingMediaBox));
        assert!(page.issues.contains(&PhysicalPageIssue::InvalidRotation));
        assert!(PdfRect::try_new([10.0, 0.0, 0.0, 10.0]).is_err());
    }
}
