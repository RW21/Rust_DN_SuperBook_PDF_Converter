//! Versioned, deterministic transform-manifest JSONL.
//!
//! A manifest has one header followed by exactly one record for every physical
//! source page. Writers validate and order records before atomically publishing
//! the complete payload.

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

/// Schema version emitted and accepted by this crate.
pub const TRANSFORM_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Errors produced while constructing, validating, or storing a manifest.
#[derive(Debug, Error)]
pub enum TransformManifestError {
    #[error("transform manifest number must be finite, got {0}")]
    NonFiniteNumber(f64),

    #[error("transform confidence must be in 0.0..=1.0, got {0}")]
    ConfidenceOutOfRange(f64),

    #[error("rotation proposal must be 0 or 180 degrees, got {0}")]
    InvalidRotation(i16),

    #[error("physical page index {0} cannot be converted to a one-based page number")]
    PageIndexOverflow(usize),

    #[error("source PDF file name must be one nonempty normal path component: {0:?}")]
    InvalidSourceFileName(String),

    #[error("source PDF SHA-256 must be exactly 64 lowercase hexadecimal characters: {0:?}")]
    InvalidSourceSha256(String),

    #[error("transform manifest tool {0} must not be empty")]
    EmptyToolMetadata(&'static str),

    #[error("transform manifest tool name must be {expected:?}, got {actual:?}")]
    InvalidToolName {
        expected: &'static str,
        actual: String,
    },

    #[error(
        "source PDF changed while hashing: initial size {initial_size}, hashed {hashed_size}, final size {final_size}"
    )]
    SourceChanged {
        initial_size: u64,
        hashed_size: u64,
        final_size: u64,
    },

    #[error("unsupported schema version {actual} in {context}; expected {expected}")]
    SchemaVersion {
        context: String,
        expected: u32,
        actual: u32,
    },

    #[error("manifest page count mismatch: header declares {expected}, found {actual}")]
    PageCount { expected: usize, actual: usize },

    #[error("duplicate physical page index {0}")]
    DuplicatePageIndex(usize),

    #[error("missing or noncontiguous physical page index {expected}; found {actual}")]
    NoncontiguousPageIndex { expected: usize, actual: usize },

    #[error(
        "source page number mismatch for index {page_index}: expected {expected}, found {actual}"
    )]
    SourcePageNumber {
        page_index: usize,
        expected: usize,
        actual: usize,
    },

    #[error("transform manifest is empty")]
    EmptyManifest,

    #[error("first transform manifest record must be a header")]
    HeaderNotFirst,

    #[error("unexpected header record on line {0}")]
    UnexpectedHeader(usize),

    #[error("invalid JSONL record on line {line}: {source}")]
    JsonLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to serialize transform manifest: {0}")]
    Serialization(#[source] serde_json::Error),

    #[error("failed to persist transform manifest: {0}")]
    Persist(#[from] tempfile::PersistError),

    #[error("transform manifest I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "transform manifest was committed at {path}, but directory sync failed and durability is not confirmed: {source}"
    )]
    CommittedButNotConfirmedDurable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl TransformManifestError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// A JSON-safe finite floating-point number.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct FiniteF64(f64);

impl FiniteF64 {
    pub fn new(value: f64) -> Result<Self, TransformManifestError> {
        if value.is_finite() {
            Ok(Self(if value == 0.0 { 0.0 } else { value }))
        } else {
            Err(TransformManifestError::NonFiniteNumber(value))
        }
    }

    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for FiniteF64 {
    type Error = TransformManifestError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for FiniteF64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if !self.0.is_finite() {
            return Err(serde::ser::Error::custom("non-finite manifest number"));
        }
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for FiniteF64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

/// A finite transform confidence in the inclusive range `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Confidence(FiniteF64);

impl Confidence {
    pub fn new(value: f64) -> Result<Self, TransformManifestError> {
        let value = FiniteF64::new(value)?;
        if (0.0..=1.0).contains(&value.get()) {
            Ok(Self(value))
        } else {
            Err(TransformManifestError::ConfidenceOutOfRange(value.get()))
        }
    }

    #[must_use]
    pub fn get(self) -> f64 {
        self.0.get()
    }
}

impl TryFrom<f64> for Confidence {
    type Error = TransformManifestError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for Confidence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Confidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = FiniteF64::deserialize(deserializer)?;
        Self::new(value.get()).map_err(D::Error::custom)
    }
}

fn validate_confidence(value: Confidence) -> Result<(), TransformManifestError> {
    Confidence::new(value.get()).map(|_| ())
}

/// A validated rotation proposal in degrees (`0` or `180`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationDegrees(i16);

impl RotationDegrees {
    pub fn new(value: i16) -> Result<Self, TransformManifestError> {
        validate_rotation(value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn get(self) -> i16 {
        self.0
    }
}

impl TryFrom<i16> for RotationDegrees {
    type Error = TransformManifestError;

    fn try_from(value: i16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for RotationDegrees {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i16(self.0)
    }
}

impl<'de> Deserialize<'de> for RotationDegrees {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = i16::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

/// Decision made for an analyzed transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformDecision {
    Unchanged,
    Proposed,
    Applied,
    Rejected,
    Failed,
}

/// PDF indirect-object identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfObjectId {
    pub object_number: u32,
    pub generation: u16,
}

/// Native metadata for a directly identified source-page image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceImageMetadata {
    pub object_id: Option<PdfObjectId>,
    pub width: u32,
    pub height: u32,
    pub color_space: Option<String>,
    pub bits_per_component: Option<u8>,
    pub filter: Option<String>,
    pub decode_params: Option<serde_json::Value>,
}

/// Proposed or applied 180-degree rotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotationTransform {
    pub proposed_degrees: RotationDegrees,
    pub score: Option<FiniteF64>,
    pub confidence: Confidence,
    pub decision: TransformDecision,
    pub reason: String,
}

impl RotationTransform {
    pub fn new(
        proposed_degrees: i16,
        score: Option<f64>,
        confidence_value: f64,
        decision: TransformDecision,
        reason: impl Into<String>,
    ) -> Result<Self, TransformManifestError> {
        Ok(Self {
            proposed_degrees: RotationDegrees::new(proposed_degrees)?,
            score: score.map(FiniteF64::new).transpose()?,
            confidence: Confidence::new(confidence_value)?,
            decision,
            reason: reason.into(),
        })
    }
}

/// Proposed or applied skew correction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeskewTransform {
    pub proposed_degrees: FiniteF64,
    pub confidence: Confidence,
    pub feature_count: usize,
    pub decision: TransformDecision,
    pub reason: String,
}

impl DeskewTransform {
    pub fn new(
        proposed_degrees: f64,
        confidence_value: f64,
        feature_count: usize,
        decision: TransformDecision,
        reason: impl Into<String>,
    ) -> Result<Self, TransformManifestError> {
        Ok(Self {
            proposed_degrees: FiniteF64::new(proposed_degrees)?,
            confidence: Confidence::new(confidence_value)?,
            feature_count,
            decision,
            reason: reason.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputTransform {
    pub pixel_changed: bool,
    pub review_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageTransformError {
    pub stage: String,
    pub code: String,
    pub message: String,
}

/// Evidence and decisions for one physical source page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageTransformRecord {
    pub schema_version: u32,
    pub page_index: usize,
    pub source_page_number: usize,
    pub source_image: Option<SourceImageMetadata>,
    pub blank: Option<bool>,
    pub rotation: RotationTransform,
    pub deskew: DeskewTransform,
    pub output: OutputTransform,
    pub errors: Vec<PageTransformError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolMetadata {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePdfIdentity {
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub page_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformManifestHeader {
    pub schema_version: u32,
    pub tool: ToolMetadata,
    pub source_pdf: SourcePdfIdentity,
}

impl TransformManifestHeader {
    pub fn for_source(path: &Path, page_count: usize) -> Result<Self, TransformManifestError> {
        Ok(Self {
            schema_version: TRANSFORM_MANIFEST_SCHEMA_VERSION,
            tool: ToolMetadata {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            source_pdf: source_pdf_identity(path, page_count)?,
        })
    }
}

/// Tagged JSONL record. The tag is emitted before the variant fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum TransformManifestRecord {
    Header(TransformManifestHeader),
    Page(PageTransformRecord),
}

/// Hash the complete source PDF and return stable source identity metadata.
pub fn source_pdf_identity(
    path: &Path,
    page_count: usize,
) -> Result<SourcePdfIdentity, TransformManifestError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| TransformManifestError::InvalidSourceFileName(path.display().to_string()))?
        .to_string();
    validate_source_file_name(&file_name)?;

    let file = File::open(path).map_err(|error| TransformManifestError::io(path, error))?;
    let initial_size = file
        .metadata()
        .map_err(|error| TransformManifestError::io(path, error))?
        .len();
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut hashed_size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|error| TransformManifestError::io(path, error))?;
        if bytes_read == 0 {
            break;
        }
        hashed_size = hashed_size.checked_add(bytes_read as u64).ok_or(
            TransformManifestError::SourceChanged {
                initial_size,
                hashed_size: u64::MAX,
                final_size: initial_size,
            },
        )?;
        hasher.update(&buffer[..bytes_read]);
    }
    let final_size = reader
        .get_ref()
        .metadata()
        .map_err(|error| TransformManifestError::io(path, error))?
        .len();
    ensure_source_unchanged(initial_size, hashed_size, final_size)?;

    Ok(SourcePdfIdentity {
        file_name,
        size_bytes: hashed_size,
        sha256: format!("{:x}", hasher.finalize()),
        page_count,
    })
}

fn ensure_source_unchanged(
    initial_size: u64,
    hashed_size: u64,
    final_size: u64,
) -> Result<(), TransformManifestError> {
    if hashed_size != initial_size || final_size != initial_size {
        return Err(TransformManifestError::SourceChanged {
            initial_size,
            hashed_size,
            final_size,
        });
    }
    Ok(())
}

/// Create a placeholder owned by the coordinator before page processing starts.
pub fn pending_page_record(
    page_index: usize,
) -> Result<PageTransformRecord, TransformManifestError> {
    let source_page_number = page_index
        .checked_add(1)
        .ok_or(TransformManifestError::PageIndexOverflow(page_index))?;
    Ok(PageTransformRecord {
        schema_version: TRANSFORM_MANIFEST_SCHEMA_VERSION,
        page_index,
        source_page_number,
        source_image: None,
        blank: None,
        rotation: RotationTransform::new(0, None, 0.0, TransformDecision::Unchanged, "pending")?,
        deskew: DeskewTransform::new(0.0, 0.0, 0, TransformDecision::Unchanged, "pending")?,
        output: OutputTransform {
            pixel_changed: false,
            review_required: false,
        },
        errors: Vec::new(),
    })
}

fn validate_rotation(degrees: i16) -> Result<(), TransformManifestError> {
    if matches!(degrees, 0 | 180) {
        Ok(())
    } else {
        Err(TransformManifestError::InvalidRotation(degrees))
    }
}

fn validate_source_file_name(file_name: &str) -> Result<(), TransformManifestError> {
    let mut components = Path::new(file_name).components();
    let one_normal_component = matches!(components.next(), Some(Component::Normal(component)) if !component.is_empty())
        && components.next().is_none();
    if one_normal_component && !file_name.contains(['/', '\\']) {
        Ok(())
    } else {
        Err(TransformManifestError::InvalidSourceFileName(
            file_name.to_string(),
        ))
    }
}

fn validate_header(header: &TransformManifestHeader) -> Result<(), TransformManifestError> {
    if header.schema_version != TRANSFORM_MANIFEST_SCHEMA_VERSION {
        return Err(TransformManifestError::SchemaVersion {
            context: "header".to_string(),
            expected: TRANSFORM_MANIFEST_SCHEMA_VERSION,
            actual: header.schema_version,
        });
    }
    if header.tool.name.trim().is_empty() {
        return Err(TransformManifestError::EmptyToolMetadata("name"));
    }
    if header.tool.name != env!("CARGO_PKG_NAME") {
        return Err(TransformManifestError::InvalidToolName {
            expected: env!("CARGO_PKG_NAME"),
            actual: header.tool.name.clone(),
        });
    }
    if header.tool.version.trim().is_empty() {
        return Err(TransformManifestError::EmptyToolMetadata("version"));
    }
    validate_source_file_name(&header.source_pdf.file_name)?;
    if header.source_pdf.sha256.len() != 64
        || !header
            .source_pdf
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(TransformManifestError::InvalidSourceSha256(
            header.source_pdf.sha256.clone(),
        ));
    }
    Ok(())
}

fn validate_page(page: &PageTransformRecord) -> Result<(), TransformManifestError> {
    if page.schema_version != TRANSFORM_MANIFEST_SCHEMA_VERSION {
        return Err(TransformManifestError::SchemaVersion {
            context: format!("page {}", page.page_index),
            expected: TRANSFORM_MANIFEST_SCHEMA_VERSION,
            actual: page.schema_version,
        });
    }
    validate_rotation(page.rotation.proposed_degrees.get())?;
    if let Some(score) = page.rotation.score {
        FiniteF64::new(score.get())?;
    }
    validate_confidence(page.rotation.confidence)?;
    FiniteF64::new(page.deskew.proposed_degrees.get())?;
    validate_confidence(page.deskew.confidence)?;
    Ok(())
}

/// Validate schema, cardinality, and physical-page identity invariants.
pub fn validate_transform_manifest(
    header: &TransformManifestHeader,
    pages: &[PageTransformRecord],
) -> Result<(), TransformManifestError> {
    validate_header(header)?;
    if pages.len() != header.source_pdf.page_count {
        return Err(TransformManifestError::PageCount {
            expected: header.source_pdf.page_count,
            actual: pages.len(),
        });
    }

    let mut seen = HashSet::with_capacity(pages.len());
    for page in pages {
        validate_page(page)?;
        if !seen.insert(page.page_index) {
            return Err(TransformManifestError::DuplicatePageIndex(page.page_index));
        }
        let expected_number = page
            .page_index
            .checked_add(1)
            .ok_or(TransformManifestError::PageIndexOverflow(page.page_index))?;
        if page.source_page_number != expected_number {
            return Err(TransformManifestError::SourcePageNumber {
                page_index: page.page_index,
                expected: expected_number,
                actual: page.source_page_number,
            });
        }
    }

    let mut indices: Vec<_> = seen.into_iter().collect();
    indices.sort_unstable();
    for (expected, actual) in indices.into_iter().enumerate() {
        if actual != expected {
            return Err(TransformManifestError::NoncontiguousPageIndex { expected, actual });
        }
    }
    Ok(())
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                canonicalize_json(value);
            }
        }
        serde_json::Value::Object(object) => {
            let mut fields: Vec<_> = std::mem::take(object).into_iter().collect();
            fields.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            for (key, mut value) in fields {
                canonicalize_json(&mut value);
                object.insert(key, value);
            }
        }
        serde_json::Value::Number(number) if number.is_f64() && number.as_f64() == Some(0.0) => {
            *number = serde_json::Number::from_f64(0.0).expect("zero is a finite JSON number");
        }
        _ => {}
    }
}

fn canonicalized_page(page: &PageTransformRecord) -> PageTransformRecord {
    let mut page = page.clone();
    if let Some(decode_params) = page
        .source_image
        .as_mut()
        .and_then(|source_image| source_image.decode_params.as_mut())
    {
        canonicalize_json(decode_params);
    }
    page
}

fn serialize_jsonl(
    header: &TransformManifestHeader,
    pages: &[PageTransformRecord],
) -> Result<Vec<u8>, TransformManifestError> {
    validate_transform_manifest(header, pages)?;
    let mut ordered: Vec<_> = pages.iter().collect();
    ordered.sort_unstable_by_key(|page| page.page_index);

    let mut payload = Vec::new();
    serde_json::to_writer(
        &mut payload,
        &TransformManifestRecord::Header(header.clone()),
    )
    .map_err(TransformManifestError::Serialization)?;
    payload.push(b'\n');
    for page in ordered {
        serde_json::to_writer(
            &mut payload,
            &TransformManifestRecord::Page(canonicalized_page(page)),
        )
        .map_err(TransformManifestError::Serialization)?;
        payload.push(b'\n');
    }
    Ok(payload)
}

/// Atomically replace `path` with a completely validated deterministic JSONL file.
pub fn write_transform_manifest_atomic(
    path: &Path,
    header: &TransformManifestHeader,
    pages: &[PageTransformRecord],
) -> Result<(), TransformManifestError> {
    let payload = serialize_jsonl(header, pages)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temp_file =
        NamedTempFile::new_in(parent).map_err(|error| TransformManifestError::io(parent, error))?;
    let mut writer = BufWriter::new(temp_file);
    writer
        .write_all(&payload)
        .map_err(|error| TransformManifestError::io(path, error))?;
    writer
        .flush()
        .map_err(|error| TransformManifestError::io(path, error))?;
    writer
        .get_ref()
        .as_file()
        .sync_all()
        .map_err(|error| TransformManifestError::io(path, error))?;
    let temp_file = writer
        .into_inner()
        .map_err(|error| TransformManifestError::io(path, error.into_error()))?;
    temp_file.persist(path)?;

    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(
            |source| TransformManifestError::CommittedButNotConfirmedDurable {
                path: path.to_path_buf(),
                source,
            },
        )?;

    Ok(())
}

/// Read and validate a complete transform manifest.
pub fn read_transform_manifest(
    path: &Path,
) -> Result<(TransformManifestHeader, Vec<PageTransformRecord>), TransformManifestError> {
    let file = File::open(path).map_err(|error| TransformManifestError::io(path, error))?;
    let reader = BufReader::new(file);
    let mut records = reader.lines().enumerate();
    let Some((_, first_line)) = records.next() else {
        return Err(TransformManifestError::EmptyManifest);
    };
    let first_line = first_line.map_err(|error| TransformManifestError::io(path, error))?;
    let first: TransformManifestRecord = serde_json::from_str(&first_line)
        .map_err(|source| TransformManifestError::JsonLine { line: 1, source })?;
    let TransformManifestRecord::Header(header) = first else {
        return Err(TransformManifestError::HeaderNotFirst);
    };

    let mut pages = Vec::new();
    for (line_index, line) in records {
        let line_number = line_index + 1;
        let line = line.map_err(|error| TransformManifestError::io(path, error))?;
        let record: TransformManifestRecord =
            serde_json::from_str(&line).map_err(|source| TransformManifestError::JsonLine {
                line: line_number,
                source,
            })?;
        match record {
            TransformManifestRecord::Header(_) => {
                return Err(TransformManifestError::UnexpectedHeader(line_number));
            }
            TransformManifestRecord::Page(page) => pages.push(page),
        }
    }

    validate_transform_manifest(&header, &pages)?;
    Ok((header, pages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn test_header(page_count: usize) -> TransformManifestHeader {
        TransformManifestHeader {
            schema_version: TRANSFORM_MANIFEST_SCHEMA_VERSION,
            tool: ToolMetadata {
                name: "superbook-pdf".to_string(),
                version: "0.1.0".to_string(),
            },
            source_pdf: SourcePdfIdentity {
                file_name: "source.pdf".to_string(),
                size_bytes: 123,
                sha256: "a".repeat(64),
                page_count,
            },
        }
    }

    fn page(index: usize) -> PageTransformRecord {
        pending_page_record(index).unwrap()
    }

    #[test]
    fn test_page_transform_record_serde_round_trip() {
        let record = PageTransformRecord {
            schema_version: TRANSFORM_MANIFEST_SCHEMA_VERSION,
            page_index: 2,
            source_page_number: 3,
            source_image: Some(SourceImageMetadata {
                object_id: Some(PdfObjectId {
                    object_number: 17,
                    generation: 2,
                }),
                width: 2500,
                height: 4200,
                color_space: Some("gray".to_string()),
                bits_per_component: Some(1),
                filter: Some("CCITTFaxDecode".to_string()),
                decode_params: Some(json!({"Columns": 2500, "K": -1})),
            }),
            blank: Some(false),
            rotation: RotationTransform::new(
                180,
                Some(0.875),
                0.95,
                TransformDecision::Applied,
                "above_threshold",
            )
            .unwrap(),
            deskew: DeskewTransform::new(
                -0.75,
                0.8,
                42,
                TransformDecision::Proposed,
                "report_only",
            )
            .unwrap(),
            output: OutputTransform {
                pixel_changed: true,
                review_required: false,
            },
            errors: vec![PageTransformError {
                stage: "deskew".to_string(),
                code: "warning".to_string(),
                message: "example".to_string(),
            }],
        };

        let tagged = TransformManifestRecord::Page(record.clone());
        let encoded = serde_json::to_string(&tagged).unwrap();
        assert!(encoded.starts_with("{\"record_type\":\"page\",\"schema_version\":1"));
        assert!(encoded.contains("\"decision\":\"applied\""));
        assert!(encoded.contains("\"decision\":\"proposed\""));
        assert!(!encoded.contains(":null,\"confidence\""));

        let decoded: TransformManifestRecord = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, tagged);
    }

    #[test]
    fn test_non_finite_angles_are_rejected() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(FiniteF64::new(value).is_err());
            assert!(
                DeskewTransform::new(value, 0.5, 10, TransformDecision::Unchanged, "test").is_err()
            );
        }
        assert!(serde_json::from_str::<FiniteF64>("null").is_err());
    }

    #[test]
    fn test_non_finite_confidences_are_rejected() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(Confidence::new(value).is_err());
            assert!(RotationTransform::new(
                0,
                Some(value),
                0.5,
                TransformDecision::Unchanged,
                "test"
            )
            .is_err());
            assert!(
                RotationTransform::new(0, None, value, TransformDecision::Unchanged, "test")
                    .is_err()
            );
            assert!(
                DeskewTransform::new(0.0, value, 10, TransformDecision::Unchanged, "test").is_err()
            );
        }

        assert_eq!(
            serde_json::to_string(&FiniteF64::new(0.5).unwrap()).unwrap(),
            "0.5"
        );
        for invalid_json_number in ["NaN", "Infinity", "-Infinity"] {
            assert!(serde_json::from_str::<FiniteF64>(invalid_json_number).is_err());
            assert!(serde_json::from_str::<Confidence>(invalid_json_number).is_err());
        }
    }

    #[test]
    fn test_confidence_outside_unit_interval_is_rejected() {
        for value in [-0.000_001, 1.000_001] {
            assert!(Confidence::new(value).is_err());
            assert!(serde_json::from_str::<Confidence>(&value.to_string()).is_err());
            assert!(
                RotationTransform::new(0, None, value, TransformDecision::Unchanged, "test")
                    .is_err()
            );
            assert!(
                DeskewTransform::new(0.0, value, 0, TransformDecision::Unchanged, "test").is_err()
            );
        }

        let invalid_json = r#"{
            "record_type":"page",
            "schema_version":1,
            "page_index":0,
            "source_page_number":1,
            "source_image":null,
            "blank":null,
            "rotation":{"proposed_degrees":0,"score":null,"confidence":1.1,"decision":"unchanged","reason":"test"},
            "deskew":{"proposed_degrees":0.0,"confidence":0.0,"feature_count":0,"decision":"unchanged","reason":"test"},
            "output":{"pixel_changed":false,"review_required":false},
            "errors":[]
        }"#;
        assert!(serde_json::from_str::<TransformManifestRecord>(invalid_json).is_err());
    }

    #[test]
    fn test_invalid_rotation_proposal_is_rejected() {
        assert!(RotationDegrees::new(90).is_err());
        assert!(
            RotationTransform::new(90, None, 0.5, TransformDecision::Proposed, "test").is_err()
        );

        let invalid_json = r#"{
            "proposed_degrees":90,
            "score":null,
            "confidence":0.5,
            "decision":"proposed",
            "reason":"test"
        }"#;
        assert!(serde_json::from_str::<RotationTransform>(invalid_json).is_err());
    }

    #[test]
    fn test_manifest_serde_rejects_unknown_fields_at_every_level() {
        let header = serde_json::to_value(TransformManifestRecord::Header(test_header(1))).unwrap();
        let page_record = serde_json::to_value(TransformManifestRecord::Page(page(0))).unwrap();

        let mut unknown_header = header.clone();
        unknown_header
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), json!(true));
        assert!(serde_json::from_value::<TransformManifestRecord>(unknown_header).is_err());

        let mut unknown_page = page_record.clone();
        unknown_page
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), json!(true));
        assert!(serde_json::from_value::<TransformManifestRecord>(unknown_page).is_err());

        let mut unknown_nested = page_record;
        unknown_nested["rotation"]
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), json!(true));
        assert!(serde_json::from_value::<TransformManifestRecord>(unknown_nested).is_err());
    }

    #[test]
    fn test_signed_zero_is_normalized() {
        let value = FiniteF64::new(-0.0).unwrap();
        assert!(value.get().is_sign_positive());
        assert_eq!(serde_json::to_string(&value).unwrap(), "0.0");

        let decoded: FiniteF64 = serde_json::from_str("-0.0").unwrap();
        assert!(decoded.get().is_sign_positive());
    }

    #[test]
    fn test_manifest_writes_header_then_pages_in_physical_order() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("book.transforms.jsonl");
        let pages: Vec<_> = [7, 2, 9, 0, 4, 8, 1, 6, 3, 5]
            .into_iter()
            .map(page)
            .collect();

        write_transform_manifest_atomic(&path, &test_header(10), &pages).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.ends_with('\n'));
        let records: Vec<TransformManifestRecord> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 11);
        assert!(matches!(
            records.first(),
            Some(TransformManifestRecord::Header(_))
        ));
        let indices: Vec<_> = records
            .iter()
            .skip(1)
            .map(|record| match record {
                TransformManifestRecord::Page(page) => page.page_index,
                TransformManifestRecord::Header(_) => panic!("header after first line"),
            })
            .collect();
        assert_eq!(indices, (0..10).collect::<Vec<_>>());

        let reversed: Vec<_> = (0..10).rev().map(page).collect();
        write_transform_manifest_atomic(&path, &test_header(10), &reversed).unwrap();
        assert_eq!(fs::read(&path).unwrap(), contents.as_bytes());
    }

    #[test]
    fn test_manifest_accepts_required_semantic_page_cases() {
        let mut blank = page(0);
        blank.blank = Some(true);
        blank.rotation.reason = "blank_page".to_string();
        blank.deskew.reason = "blank_page".to_string();

        let mut low_confidence = page(1);
        low_confidence.blank = Some(false);
        low_confidence.deskew =
            DeskewTransform::new(0.2, 0.05, 4, TransformDecision::Rejected, "below_threshold")
                .unwrap();

        let mut corrected = page(2);
        corrected.blank = Some(false);
        corrected.rotation = RotationTransform::new(
            180,
            Some(0.9),
            0.95,
            TransformDecision::Applied,
            "above_threshold",
        )
        .unwrap();
        corrected.output.pixel_changed = true;

        let mut failed = page(3);
        failed.blank = None;
        failed.rotation.decision = TransformDecision::Failed;
        failed.rotation.reason = "worker_failed".to_string();
        failed.output.review_required = true;
        failed.errors.push(PageTransformError {
            stage: "rotation".to_string(),
            code: "worker_failed".to_string(),
            message: "detector exited".to_string(),
        });

        let mut untouched = page(4);
        untouched.blank = Some(false);
        untouched.rotation.reason = "disabled".to_string();
        untouched.deskew.reason = "disabled".to_string();

        let pages = vec![blank, low_confidence, corrected, failed, untouched];
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("cases.transforms.jsonl");
        write_transform_manifest_atomic(&path, &test_header(5), &pages).unwrap();
        let (header, decoded) = read_transform_manifest(&path).unwrap();

        assert_eq!(header, test_header(5));
        assert_eq!(decoded, pages);
    }

    #[test]
    fn test_manifest_represents_page_without_source_image_xobject() {
        let mut unsupported = page(0);
        unsupported.source_image = None;
        unsupported.output.review_required = true;
        unsupported.errors.push(PageTransformError {
            stage: "extraction".to_string(),
            code: "no_image_xobject".to_string(),
            message: "physical page has no directly usable image XObject".to_string(),
        });

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("unsupported.transforms.jsonl");
        write_transform_manifest_atomic(&path, &test_header(1), &[unsupported]).unwrap();
        let (_, pages) = read_transform_manifest(&path).unwrap();

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page_index, 0);
        assert_eq!(pages[0].source_page_number, 1);
        assert!(pages[0].source_image.is_none());
        assert!(pages[0].output.review_required);
        assert_eq!(pages[0].errors[0].code, "no_image_xobject");
    }

    #[test]
    fn test_manifest_rejects_duplicate_missing_and_noncontiguous_pages() {
        let cases = vec![
            (test_header(2), vec![page(0), page(0)]),
            (test_header(2), vec![page(0)]),
            (test_header(2), vec![page(0), page(2)]),
            (test_header(1), vec![page(1)]),
        ];

        for (header, pages) in cases {
            assert!(validate_transform_manifest(&header, &pages).is_err());
        }

        let mut misnumbered = page(0);
        misnumbered.source_page_number = 2;
        assert!(validate_transform_manifest(&test_header(1), &[misnumbered]).is_err());
    }

    #[test]
    fn test_page_index_overflow_is_a_typed_error() {
        assert!(matches!(
            pending_page_record(usize::MAX),
            Err(TransformManifestError::PageIndexOverflow(usize::MAX))
        ));

        let mut overflow = page(0);
        overflow.page_index = usize::MAX;
        overflow.source_page_number = usize::MAX;
        assert!(matches!(
            validate_transform_manifest(&test_header(1), &[overflow]),
            Err(TransformManifestError::PageIndexOverflow(usize::MAX))
        ));
    }

    #[test]
    fn test_source_length_changes_are_typed_errors() {
        for (initial_size, hashed_size, final_size) in [(10, 11, 11), (10, 9, 9), (10, 10, 11)] {
            assert!(matches!(
                ensure_source_unchanged(initial_size, hashed_size, final_size),
                Err(TransformManifestError::SourceChanged { .. })
            ));
        }
        ensure_source_unchanged(10, 10, 10).unwrap();
    }

    #[test]
    fn test_manifest_rejects_schema_mismatches() {
        let mut header = test_header(1);
        header.schema_version += 1;
        assert!(validate_transform_manifest(&header, &[page(0)]).is_err());

        let mut invalid_page = page(0);
        invalid_page.schema_version += 1;
        assert!(validate_transform_manifest(&test_header(1), &[invalid_page]).is_err());
    }

    #[test]
    fn test_manifest_rejects_invalid_source_identity_and_empty_tool_metadata() {
        let invalid_file_names = [
            "",
            "/source.pdf",
            "dir/source.pdf",
            "dir\\source.pdf",
            ".",
            "..",
        ];
        for file_name in invalid_file_names {
            let mut header = test_header(1);
            header.source_pdf.file_name = file_name.to_string();
            assert!(validate_transform_manifest(&header, &[page(0)]).is_err());
        }

        for sha256 in ["a".repeat(63), "A".repeat(64), "g".repeat(64)] {
            let mut header = test_header(1);
            header.source_pdf.sha256 = sha256;
            assert!(validate_transform_manifest(&header, &[page(0)]).is_err());
        }

        for field in ["name", "version"] {
            let mut header = test_header(1);
            if field == "name" {
                header.tool.name.clear();
            } else {
                header.tool.version.clear();
            }
            assert!(validate_transform_manifest(&header, &[page(0)]).is_err());
        }

        let mut wrong_tool = test_header(1);
        wrong_tool.tool.name = "different-tool".to_string();
        assert!(matches!(
            validate_transform_manifest(&wrong_tool, &[page(0)]),
            Err(TransformManifestError::InvalidToolName { .. })
        ));

        let mut old_tool = test_header(1);
        old_tool.tool.version = "0.0.1".to_string();
        validate_transform_manifest(&old_tool, &[page(0)]).unwrap();
    }

    #[test]
    fn test_decode_params_are_recursively_canonicalized() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("canonical.transforms.jsonl");
        let mut record = page(0);
        record.source_image = Some(SourceImageMetadata {
            object_id: None,
            width: 1,
            height: 1,
            color_space: None,
            bits_per_component: None,
            filter: None,
            decode_params: Some(json!({
                "z": {"z": -0.0, "a": 1},
                "a": [{"z": 2, "a": -0.0}]
            })),
        });

        write_transform_manifest_atomic(&path, &test_header(1), &[record]).unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains(r#""decode_params":{"a":[{"a":0.0,"z":2}],"z":{"a":1,"z":0.0}}"#));
        assert!(!contents.contains("-0.0"));
    }

    #[test]
    fn test_atomic_write_replaces_existing_manifest() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("replace.transforms.jsonl");
        write_transform_manifest_atomic(&path, &test_header(1), &[page(0)]).unwrap();
        read_transform_manifest(&path).unwrap();

        write_transform_manifest_atomic(&path, &test_header(2), &[page(1), page(0)]).unwrap();

        let (header, pages) = read_transform_manifest(&path).unwrap();
        assert_eq!(header.source_pdf.page_count, 2);
        assert_eq!(
            pages.iter().map(|p| p.page_index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let entries: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
        assert_eq!(
            entries.len(),
            1,
            "temporary file must not remain after persist"
        );
    }

    #[test]
    fn test_failed_write_preserves_existing_manifest() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("preserve.transforms.jsonl");
        write_transform_manifest_atomic(&path, &test_header(1), &[page(0)]).unwrap();
        let old = fs::read(&path).unwrap();
        read_transform_manifest(&path).unwrap();
        let mut invalid_header = test_header(1);
        invalid_header.source_pdf.sha256 = "invalid".to_string();

        assert!(write_transform_manifest_atomic(&path, &invalid_header, &[page(0)]).is_err());
        assert_eq!(fs::read(&path).unwrap(), old);
        let entries: Vec<_> = fs::read_dir(temp_dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_read_rejects_duplicate_or_misordered_header() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("invalid.transforms.jsonl");
        let header =
            serde_json::to_string(&TransformManifestRecord::Header(test_header(1))).unwrap();
        let page = serde_json::to_string(&TransformManifestRecord::Page(page(0))).unwrap();

        fs::write(&path, format!("{header}\n{header}\n{page}\n")).unwrap();
        assert!(read_transform_manifest(&path).is_err());

        fs::write(&path, format!("{page}\n{header}\n")).unwrap();
        assert!(read_transform_manifest(&path).is_err());
    }

    #[test]
    fn test_source_identity_is_sha256_of_complete_pdf() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source_path = temp_dir.path().join("source.pdf");
        fs::write(&source_path, b"complete pdf bytes\n").unwrap();

        let identity = source_pdf_identity(&source_path, 7).unwrap();
        assert_eq!(identity.file_name, "source.pdf");
        assert_eq!(identity.size_bytes, 19);
        assert_eq!(identity.page_count, 7);
        assert_eq!(
            identity.sha256,
            "085e4029fa3332b9823179415ec6766d07df73332fc0d050874c80bff11183a2"
        );

        let header = TransformManifestHeader::for_source(&source_path, 7).unwrap();
        assert_eq!(header.schema_version, TRANSFORM_MANIFEST_SCHEMA_VERSION);
        assert_eq!(header.tool.name, env!("CARGO_PKG_NAME"));
        assert_eq!(header.tool.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(header.source_pdf, identity);
    }

    #[cfg(unix)]
    #[test]
    fn test_source_identity_rejects_non_utf8_file_name() {
        use std::os::unix::ffi::OsStringExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let source_path = temp_dir
            .path()
            .join(std::ffi::OsString::from_vec(b"source\xff.pdf".to_vec()));
        fs::write(&source_path, b"pdf").unwrap();

        assert!(matches!(
            source_pdf_identity(&source_path, 1),
            Err(TransformManifestError::InvalidSourceFileName(_))
        ));
    }
}
