//! Native geometry policy execution and atomic version-3 transform bundles.
use crate::cli::GeometryAction;
pub use crate::geometry_analysis::GeometryEvidenceRecord;
use crate::image_extract::{NativePageRecord, NativePdfExtractor};
use crate::pipeline::{DeskewApplicationMetadata, PdfPipeline, PipelineConfig};
use crate::preservation_bundle::{
    decoded_receipt_hash, hash_file, image_identity, publish_directory, valid_hash, BundleError,
    ImageIdentity, PublishedBundle,
};
use crate::preservation_writer::{write_staged_geometry, PageWriteReceipt};
use crate::transform_manifest::TransformDecision;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};
const VERSION: u32 = 3;
const MANIFEST_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum GeometryError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Native(#[from] crate::image_extract::NativeExtractError),
    #[error(transparent)]
    Writer(#[from] crate::preservation_writer::PreservationWriterError),
    #[error(transparent)]
    Bundle(#[from] BundleError),
    #[error("geometry policy/preparation failed: {0}")]
    Preparation(String),
    #[error("invalid geometry bundle: {0}")]
    Invalid(String),
}
fn invalid(s: &str) -> GeometryError {
    GeometryError::Invalid(s.into())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeometryPolicy {
    pub rotation_action: GeometryAction,
    pub deskew_action: GeometryAction,
    pub rotation_min_confidence: f64,
    pub deskew_max_angle: f64,
    pub deskew_min_confidence: f64,
    pub deskew_min_features: usize,
    pub deskew_noop_angle: f64,
}
impl GeometryPolicy {
    fn from_config(c: &PipelineConfig) -> Self {
        Self {
            rotation_action: c.rotation_action,
            deskew_action: c.deskew_action,
            rotation_min_confidence: c.rotation_min_confidence.get(),
            deskew_max_angle: c.deskew_max_angle.get(),
            deskew_min_confidence: c.deskew_min_confidence.get(),
            deskew_min_features: c.deskew_min_features.get(),
            deskew_noop_angle: c.deskew_noop_angle.get(),
        }
    }
    fn validate(&self) -> Result<(), GeometryError> {
        if ![
            self.rotation_min_confidence,
            self.deskew_max_angle,
            self.deskew_min_confidence,
            self.deskew_noop_angle,
        ]
        .iter()
        .all(|n| n.is_finite())
            || !(0.0..=1.0).contains(&self.rotation_min_confidence)
            || !(0.0..=1.0).contains(&self.deskew_min_confidence)
            || self.deskew_min_features == 0
            || self.deskew_max_angle <= 0.0
            || self.deskew_max_angle > 15.0
            || self.deskew_noop_angle < 0.0
            || self.deskew_noop_angle >= self.deskew_max_angle
        {
            return Err(invalid("invalid policy values"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeometryHeader {
    pub schema_version: u32,
    pub record_type: String,
    pub source_sha256: String,
    pub source_byte_length: u64,
    pub pdf_sha256: String,
    pub pdf_byte_length: u64,
    pub page_count: usize,
    pub policy: GeometryPolicy,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageGeometry {
    pub media_box: Option<[f64; 4]>,
    pub crop_box: Option<[f64; 4]>,
    pub rotation: Option<u16>,
}
fn page_geometry(page: &NativePageRecord) -> PageGeometry {
    let p = &page.physical_page;
    PageGeometry {
        media_box: p.media_box.as_ref().map(|b| b.value.coordinates()),
        crop_box: p.crop_box.as_ref().map(|b| b.value.coordinates()),
        rotation: p.normalized_rotation(),
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeometryPage {
    pub schema_version: u32,
    pub record_type: String,
    pub page_index: usize,
    pub source_page_number: usize,
    pub source_page_object: (u32, u16),
    pub geometry: PageGeometry,
    pub source_images: Vec<ImageIdentity>,
    pub output_images: Vec<ImageIdentity>,
    pub evidence: GeometryEvidenceRecord,
    pub output: PageWriteReceipt,
}
#[derive(Debug, Clone, PartialEq)]
pub struct GeometryManifest {
    pub header: GeometryHeader,
    pub pages: Vec<GeometryPage>,
}
impl GeometryManifest {
    pub fn validate(&self) -> Result<(), GeometryError> {
        let h = &self.header;
        h.policy.validate()?;
        if h.schema_version != VERSION
            || h.record_type != "geometry_header"
            || h.page_count == 0
            || h.page_count > 100_000
            || h.page_count != self.pages.len()
            || !valid_hash(&h.source_sha256)
            || !valid_hash(&h.pdf_sha256)
            || h.source_byte_length == 0
            || h.pdf_byte_length == 0
        {
            return Err(invalid("header/version/cardinality mismatch"));
        }
        for (index, page) in self.pages.iter().enumerate() {
            let e = &page.evidence;
            let r = &e.rotation;
            let d = &e.deskew;
            if page.schema_version != VERSION
                || page.record_type != "geometry_page"
                || page.page_index != index
                || page.source_page_number != index + 1
                || e.page_index != index
                || page.output.page_index != index
                || page.source_page_object.0 == 0
            {
                return Err(invalid("page identity/order mismatch"));
            }
            for rect in [page.geometry.media_box, page.geometry.crop_box]
                .into_iter()
                .flatten()
            {
                crate::pdf_reader::PdfRect::try_new(rect)
                    .map_err(|_| invalid("invalid page rectangle"))?;
            }
            if page
                .geometry
                .rotation
                .is_some_and(|r| ![0, 90, 180, 270].contains(&r))
            {
                return Err(invalid("invalid page rotation"));
            }
            if e.output_matrix
                .is_some_and(|m| !m.iter().all(|x| x.is_finite()))
            {
                return Err(invalid("nonfinite output matrix"));
            }
            for image in page.source_images.iter().chain(&page.output_images) {
                if image.width == 0
                    || image.height == 0
                    || image.object_number == 0
                    || !valid_hash(&image.encoded_sha256)
                {
                    return Err(invalid("invalid image identity"));
                }
            }
            for hash in [
                &page.output.source_image_sha256,
                &page.output.output_image_sha256,
                &page.output.decoded_pixel_sha256,
            ]
            .into_iter()
            .flatten()
            {
                if !valid_hash(hash) {
                    return Err(invalid("invalid output receipt hash"));
                }
            }
            let changed = e.rotation_pixels_changed || e.deskew_pixels_changed;
            if changed == page.output.reused
                || e.deskew_input_after_rotation
                    != (e.rotation_pixels_changed && h.policy.deskew_action != GeometryAction::Off)
            {
                return Err(invalid("contradictory pixel/frame state"));
            }
            if e.rotation_pixels_changed != (r.decision == TransformDecision::Applied)
                || e.deskew_pixels_changed != (d.decision == TransformDecision::Applied)
            {
                return Err(invalid("applied decision lacks verified pixel state"));
            }
            if (h.policy.rotation_action == GeometryAction::Off
                && r.decision != TransformDecision::Unchanged)
                || (h.policy.deskew_action == GeometryAction::Off
                    && d.decision != TransformDecision::Unchanged)
            {
                return Err(invalid("disabled stage contains a transformation decision"));
            }
            if r.reason.is_empty() || d.reason.is_empty() {
                return Err(invalid("missing decision reason"));
            }
            if e.rotation_pixels_changed
                && (h.policy.rotation_action != GeometryAction::Apply
                    || r.proposed_degrees.get() != 180
                    || r.confidence.get() < h.policy.rotation_min_confidence
                    || r.score.map(|s| s.get()).unwrap_or(-1.0)
                        < crate::DEFAULT_ROTATION_MINIMUM_APPLY_SCORE
                    || r.reason != crate::RotationReason::UpsideDownEvidence.to_string())
            {
                return Err(invalid("applied rotation violates policy"));
            }
            if e.deskew_pixels_changed
                && (h.policy.deskew_action != GeometryAction::Apply
                    || d.proposed_degrees.get().abs() > h.policy.deskew_max_angle
                    || d.proposed_degrees.get().abs() <= h.policy.deskew_noop_angle
                    || d.confidence.get() < h.policy.deskew_min_confidence
                    || d.feature_count < h.policy.deskew_min_features
                    || d.reason != crate::DeskewReason::CorrectionEvidence.to_string()
                    || e.deskew_application != Some(DeskewApplicationMetadata::current()))
            {
                return Err(invalid("applied deskew violates policy"));
            }
            if !e.deskew_pixels_changed && e.deskew_application.is_some() {
                return Err(invalid("unused deskew application metadata"));
            }
            if r.decision == TransformDecision::Proposed
                && h.policy.rotation_action != GeometryAction::Report
                || d.decision == TransformDecision::Proposed
                    && h.policy.deskew_action != GeometryAction::Report
            {
                return Err(invalid("unexecuted apply proposal in published bundle"));
            }
            if changed {
                if page.source_images.len() != 1
                    || page.output_images.len() != 1
                    || page.output.decoded_pixel_sha256.is_none()
                    || page.output.source_image_sha256.as_deref()
                        != Some(page.source_images[0].encoded_sha256.as_str())
                    || page.output.output_image_sha256.as_deref()
                        != Some(page.output_images[0].encoded_sha256.as_str())
                {
                    return Err(invalid("incomplete changed image receipt"));
                }
                let source = &page.source_images[0];
                let output = &page.output_images[0];
                if page.geometry.rotation != Some(0)
                    || !e.output_matrix.is_some_and(|[a, b, c, d, _, _]| {
                        a > 0.0 && d > 0.0 && b == 0.0 && c == 0.0
                    })
                    || source.bits_per_component != Some(8)
                    || !matches!(
                        source
                            .color_space
                            .as_ref()
                            .and_then(serde_json::Value::as_str),
                        Some("/DeviceGray" | "/DeviceRGB")
                    )
                {
                    return Err(invalid(
                        "applied geometry has noncanonical display/source layout",
                    ));
                }
                if e.deskew_pixels_changed {
                    let radians = d.proposed_degrees.get().to_radians();
                    let width = (f64::from(source.width) * radians.cos().abs()
                        + f64::from(source.height) * radians.sin().abs())
                    .ceil();
                    let height = (f64::from(source.width) * radians.sin().abs()
                        + f64::from(source.height) * radians.cos().abs())
                    .ceil();
                    if f64::from(output.width) != width
                        || f64::from(output.height) != height
                        || output.width < source.width
                        || output.height < source.height
                        || output.bits_per_component != Some(8)
                        || output
                            .color_space
                            .as_ref()
                            .and_then(serde_json::Value::as_str)
                            != Some("/DeviceRGB")
                    {
                        return Err(invalid(
                            "deskew application does not match expanded RGBA8 layout",
                        ));
                    }
                } else if source.width != output.width
                    || source.height != output.height
                    || source.bits_per_component != output.bits_per_component
                    || source.color_space != output.color_space
                {
                    return Err(invalid("half-turn changed native dimensions or mode"));
                }
            } else if page.source_images != page.output_images
                || page.output.decoded_pixel_sha256.is_some()
                || page.output.source_image_sha256 != page.output.output_image_sha256
            {
                return Err(invalid("unchanged image/receipt mismatch"));
            }
        }
        Ok(())
    }
}

fn write_manifest(path: &Path, manifest: &GeometryManifest) -> Result<(), GeometryError> {
    manifest.validate()?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer(&mut file, &manifest.header)?;
    file.write_all(b"\n")?;
    for page in &manifest.pages {
        serde_json::to_writer(&mut file, page)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}
/// Validate the published PDF, policy/decisions, image receipts and physical-page records.
/// Hashes detect corruption; the two files are not a cryptographically signed statement.
pub fn verify_geometry_bundle(directory: &Path) -> Result<GeometryManifest, GeometryError> {
    let path = directory.join("transforms.jsonl");
    if !fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err(invalid("manifest must be regular file"));
    }
    let mut text = String::new();
    File::open(&path)?
        .take(MANIFEST_LIMIT + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MANIFEST_LIMIT || !text.ends_with('\n') {
        return Err(invalid("manifest truncated or too large"));
    }
    let mut lines = text.lines();
    let header = serde_json::from_str(lines.next().ok_or_else(|| invalid("missing header"))?)?;
    let pages = lines
        .map(serde_json::from_str)
        .collect::<Result<Vec<GeometryPage>, _>>()?;
    let manifest = GeometryManifest { header, pages };
    manifest.validate()?;
    let pdf_path = directory.join("document.pdf");
    if hash_file(&pdf_path)?
        != (
            manifest.header.pdf_sha256.clone(),
            manifest.header.pdf_byte_length,
        )
    {
        return Err(invalid("output PDF hash mismatch"));
    }
    let output_native = NativePdfExtractor::extract_path(&pdf_path)?;
    if output_native.pages().len() != manifest.pages.len() {
        return Err(invalid("output PDF page count mismatch"));
    }
    for (actual, page) in output_native.pages().iter().zip(&manifest.pages) {
        let id = &actual.physical_page.page_object_id;
        if (id.object_number, id.generation) != page.source_page_object
            || page_geometry(actual) != page.geometry
        {
            return Err(invalid("output page identity/geometry mismatch"));
        }
        let images = actual
            .image_invocations
            .iter()
            .map(|i| image_identity(&i.metadata))
            .collect::<Vec<_>>();
        if images != page.output_images {
            return Err(invalid("output image metadata mismatch"));
        }
        let matrix = if actual.image_invocations.len() == 1 {
            Some(
                actual.image_invocations[0]
                    .binding
                    .placement_matrix
                    .coordinates(),
            )
        } else {
            None
        };
        if matrix != page.evidence.output_matrix {
            return Err(invalid("output placement mismatch"));
        }
        if page.output.reused {
            if actual.image_invocations.len() == 1 && actual.image_invocations[0].binding.is_direct
            {
                let image = &actual.image_invocations[0].metadata;
                if page.output.source_image_sha256.as_deref() != Some(image.encoded_sha256.as_str())
                    || page.output.width != Some(image.width)
                    || page.output.height != Some(image.height)
                    || page.output.bits_per_component != image.bits_per_component
                    || page.output.color_space.as_deref()
                        != image
                            .color_space
                            .as_ref()
                            .and_then(serde_json::Value::as_str)
                {
                    return Err(invalid(
                        "unchanged receipt differs from actual source image",
                    ));
                }
            } else if page.output.source_image_sha256.is_some()
                || page.output.width.is_some()
                || page.output.height.is_some()
                || page.output.bits_per_component.is_some()
                || page.output.color_space.is_some()
            {
                return Err(invalid("complex page has fabricated single-image receipt"));
            }
        }
        if !page.output.reused {
            let original_meta = &page.source_images[0];
            let capability = match original_meta.filters.as_slice() {
                [f] if f == "FlateDecode" => {
                    crate::image_extract::NativeTransformDecodeCapability::Flate8
                }
                [f] if f == "DCTDecode" => {
                    crate::image_extract::NativeTransformDecodeCapability::Dct8
                }
                _ => crate::image_extract::NativeTransformDecodeCapability::Unsupported,
            };
            let original = output_native.decode_image(
                &crate::image_extract::NativeImageMetadata {
                    object_id: crate::transform_manifest::PdfObjectId {
                        object_number: original_meta.object_number,
                        generation: original_meta.generation,
                    },
                    width: original_meta.width,
                    height: original_meta.height,
                    color_space: original_meta.color_space.clone(),
                    bits_per_component: original_meta.bits_per_component,
                    filters: original_meta.filters.clone(),
                    decode_params: original_meta.decode_params.clone(),
                    encoded_length: original_meta.encoded_length,
                    encoded_sha256: original_meta.encoded_sha256.clone(),
                    transform_decode: capability,
                },
                256 * 1024 * 1024,
            )?;
            if !page.evidence.deskew_pixels_changed {
                use sha2::{Digest, Sha256};
                let expected = format!("{:x}", Sha256::digest(original.rotate180().as_bytes()));
                if page.output.decoded_pixel_sha256.as_deref() != Some(expected.as_str()) {
                    return Err(invalid(
                        "rotation pixels are not the exact source half-turn",
                    ));
                }
            }
            let image = &page.output_images[0];
            let stream = output_native
                .source_document()
                .get_object((image.object_number, image.generation))
                .and_then(lopdf::Object::as_stream)
                .map_err(|_| invalid("missing output image stream"))?;
            if stream.dict.has(b"SMask") != page.evidence.deskew_pixels_changed {
                return Err(invalid("application alpha layout mismatch"));
            }
            if page.output.width != Some(image.width)
                || page.output.height != Some(image.height)
                || page.output.bits_per_component != image.bits_per_component
                || page.output.color_space.as_deref()
                    != image
                        .color_space
                        .as_ref()
                        .and_then(serde_json::Value::as_str)
                || image.filters != ["FlateDecode"]
            {
                return Err(invalid("output receipt metadata mismatch"));
            }
            let actual_hash = decoded_receipt_hash(
                output_native.source_document(),
                (image.object_number, image.generation),
            )?;
            if page.output.decoded_pixel_sha256.as_deref() != Some(actual_hash.as_str()) {
                return Err(invalid("output decoded pixels differ from receipt"));
            }
        }
    }
    Ok(manifest)
}

/// Execute against an immutable source snapshot and atomically publish one geometry bundle.
pub fn process_geometry(
    pipeline: &PdfPipeline,
    input: &Path,
    destination: &Path,
) -> Result<PublishedBundle, GeometryError> {
    let c = pipeline.config();
    if !c.geometry_only || c.max_pages.is_some() {
        return Err(invalid(
            "geometry mode requires all physical pages; max-pages is unsupported",
        ));
    }
    if !cfg!(target_os = "linux") {
        return Err(BundleError::UnsupportedPlatform.into());
    }
    let policy = GeometryPolicy::from_config(c);
    policy.validate()?;
    if !fs::symlink_metadata(input)?.file_type().is_file() {
        return Err(invalid("source must be regular file"));
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => return Err(BundleError::DestinationExists.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let target = parent.join(
        destination
            .file_name()
            .ok_or_else(|| invalid("missing destination name"))?,
    );
    let stage = tempfile::Builder::new()
        .prefix(".geometry-")
        .tempdir_in(&parent)?;
    let snapshot = stage.path().join("source-snapshot.pdf");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&snapshot)?;
    std::io::copy(&mut File::open(input)?, &mut file)?;
    file.sync_all()?;
    drop(file);
    let source = hash_file(&snapshot)?;
    let native = NativePdfExtractor::extract_path(&snapshot)?;
    let work = stage.path().join("work");
    fs::create_dir(&work)?;
    let prepared = crate::geometry_analysis::prepare(&native, pipeline, &work)
        .map_err(|e| GeometryError::Preparation(e.to_string()))?;
    let pdf_path = stage.path().join("document.pdf");
    let receipt =
        write_staged_geometry(&native, &prepared.same_size, &prepared.expanded, &pdf_path)?;
    if receipt.pages.len() != native.pages().len() || prepared.pages.len() != native.pages().len() {
        return Err(invalid("missing preparation/writer page"));
    }
    let output = NativePdfExtractor::extract_path(&pdf_path)?;
    let mut pages = Vec::with_capacity(native.pages().len());
    for (index, ((mut evidence, written), (source_page, output_page))) in prepared
        .pages
        .into_iter()
        .zip(receipt.pages)
        .zip(native.pages().iter().zip(output.pages()))
        .enumerate()
    {
        if evidence.page_index != index || written.page_index != index {
            return Err(invalid("unordered worker result"));
        }
        let changed = evidence.rotation_pixels_changed || evidence.deskew_pixels_changed;
        if changed == written.reused {
            return Err(invalid("writer receipt contradicts preparation"));
        }
        if evidence.rotation_pixels_changed {
            if evidence.rotation.decision != TransformDecision::Proposed {
                return Err(invalid("rotation was not proposed"));
            }
            evidence.rotation.decision = TransformDecision::Applied;
        }
        if evidence.deskew_pixels_changed {
            if evidence.deskew.decision != TransformDecision::Proposed {
                return Err(invalid("deskew was not proposed"));
            }
            evidence.deskew.decision = TransformDecision::Applied;
        }
        let p = &source_page.physical_page;
        pages.push(GeometryPage {
            schema_version: VERSION,
            record_type: "geometry_page".into(),
            page_index: index,
            source_page_number: p.source_page_number,
            source_page_object: (p.page_object_id.object_number, p.page_object_id.generation),
            geometry: page_geometry(source_page),
            source_images: source_page
                .image_invocations
                .iter()
                .map(|i| image_identity(&i.metadata))
                .collect(),
            output_images: output_page
                .image_invocations
                .iter()
                .map(|i| image_identity(&i.metadata))
                .collect(),
            evidence,
            output: written,
        });
    }
    let manifest = GeometryManifest {
        header: GeometryHeader {
            schema_version: VERSION,
            record_type: "geometry_header".into(),
            source_sha256: source.0.clone(),
            source_byte_length: source.1,
            pdf_sha256: receipt.pdf_sha256,
            pdf_byte_length: receipt.byte_length,
            page_count: pages.len(),
            policy,
        },
        pages,
    };
    write_manifest(&stage.path().join("transforms.jsonl"), &manifest)?;
    let readback = verify_geometry_bundle(stage.path())?;
    if readback != manifest {
        return Err(invalid("geometry manifest readback differs"));
    }
    if hash_file(input)? != source {
        return Err(BundleError::SourceChanged.into());
    }
    fs::remove_dir_all(&work)?;
    fs::remove_file(snapshot)?;
    File::open(stage.path())?.sync_all()?;
    publish_directory(stage.path(), &target)?;
    if let Err(source) = File::open(&parent).and_then(|f| f.sync_all()) {
        return Err(BundleError::DurabilityUncertain {
            path: target,
            source,
        }
        .into());
    }
    // Only after pair-publication may callers observe these Applied records.
    verify_geometry_bundle(&target)?;
    Ok(PublishedBundle {
        directory: target.clone(),
        pdf_path: target.join("document.pdf"),
        manifest_path: target.join("transforms.jsonl"),
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::cli::GeometryAction;
    use lopdf::{dictionary, Document, Object, Stream};
    use std::io::Write;
    fn fixture(path: &Path) {
        let mut pdf = Document::with_version("1.7");
        let pages = pdf.new_object_id();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&[1u8, 2, 3, 4]).unwrap();
        let image = pdf.add_object(Stream::new(dictionary! {"Type"=>"XObject","Subtype"=>"Image","Width"=>2,"Height"=>2,"BitsPerComponent"=>8,"ColorSpace"=>"DeviceGray","Filter"=>"FlateDecode"},z.finish().unwrap()));
        let mut kids = Vec::new();
        for bytes in [
            b"q 20 0 0 20 0 0 cm /Scan Do Q".as_slice(),
            b"",
            b"/Scan Do /Scan Do",
        ] {
            let content = pdf.add_object(Stream::new(dictionary! {}, bytes.to_vec()));
            kids.push(Object::Reference(pdf.add_object(dictionary!{"Type"=>"Page","Parent"=>pages,"MediaBox"=>vec![0.into(),0.into(),20.into(),20.into()],"Resources"=>dictionary!{"XObject"=>dictionary!{"Scan"=>image}},"Contents"=>content})));
        }
        pdf.objects.insert(
            pages,
            Object::Dictionary(dictionary! {"Type"=>"Pages","Count"=>3,"Kids"=>kids}),
        );
        let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages});
        pdf.trailer.set("Root", catalog);
        pdf.save(path).unwrap();
    }
    #[test]
    fn geometry_report_and_off_publish_complete_byte_preserving_records() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.pdf");
        fixture(&source);
        let before = fs::read(&source).unwrap();
        for (index, action) in [GeometryAction::Off, GeometryAction::Report]
            .into_iter()
            .enumerate()
        {
            let pipeline = PdfPipeline::new(PipelineConfig::geometry_only(action, action));
            let output = dir.path().join(format!("geometry-{index}"));
            let published = process_geometry(&pipeline, &source, &output).unwrap();
            let manifest = verify_geometry_bundle(&published.directory).unwrap();
            assert_eq!(manifest.pages.len(), 3);
            assert!(manifest.pages.iter().all(|p| p.output.reused));
            assert!(manifest.pages.iter().all(|p| p.evidence.rotation.decision
                != TransformDecision::Applied
                && p.evidence.deskew.decision != TransformDecision::Applied));
            assert_eq!(fs::read(&source).unwrap(), before);
            assert_eq!(fs::read_dir(&output).unwrap().count(), 2);
            assert!(process_geometry(&pipeline, &source, &output).is_err());
        }
    }
    #[test]
    fn noncanonical_pdf_display_orientation_never_gets_double_corrected() {
        let image = crate::geometry_analysis::tests::upside_down_text();
        for (rotation, content) in [
            (180, b"600 0 0 800 0 0 cm /Scan Do".as_slice()),
            (0, b"-600 0 0 -800 600 800 cm /Scan Do"),
            (180, b"-600 0 0 -800 600 800 cm /Scan Do"),
            (90, b"600 0 0 800 0 0 cm /Scan Do"),
            (0, b"0 800 -600 0 600 0 cm /Scan Do"),
        ] {
            let (dir, native) = crate::geometry_analysis::tests::fixture(&[content], &image, false);
            let mut doc = native.source_document().clone();
            let id = *doc.get_pages().values().next().unwrap();
            doc.get_object_mut(id)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set("Rotate", rotation);
            doc.save(native.source_path()).unwrap();
            let before = fs::read(native.source_path()).unwrap();
            let pipeline = PdfPipeline::new(PipelineConfig::geometry_only(
                GeometryAction::Apply,
                GeometryAction::Apply,
            ));
            let output =
                process_geometry(&pipeline, native.source_path(), &dir.path().join("bundle"))
                    .unwrap();
            let manifest = verify_geometry_bundle(&output.directory).unwrap();
            assert!(manifest.pages[0].output.reused);
            assert!(manifest.pages[0].evidence.review_required);
            assert!(!manifest.pages[0].evidence.rotation_pixels_changed);
            assert_eq!(fs::read(native.source_path()).unwrap(), before);
        }
    }

    #[test]
    fn published_rotation_is_exact_and_only_final_manifest_says_applied() {
        let image = crate::geometry_analysis::tests::upside_down_text();
        for deskew in [GeometryAction::Off, GeometryAction::Report] {
            let (dir, native) = crate::geometry_analysis::tests::fixture(
                &[b"600 0 0 800 0 0 cm /Scan Do"],
                &image,
                false,
            );
            let pipeline =
                PdfPipeline::new(PipelineConfig::geometry_only(GeometryAction::Apply, deskew));
            let published =
                process_geometry(&pipeline, native.source_path(), &dir.path().join("bundle"))
                    .unwrap();
            let manifest = verify_geometry_bundle(&published.directory).unwrap();
            assert_eq!(
                manifest.pages[0].evidence.rotation.decision,
                TransformDecision::Applied
            );
            assert_eq!(
                manifest.pages[0].evidence.deskew_input_after_rotation,
                deskew != GeometryAction::Off
            );
            let output = NativePdfExtractor::extract_path(&published.pdf_path).unwrap();
            let decoded = output
                .decode_image(
                    &output.pages()[0].image_invocations[0].metadata,
                    256 * 1024 * 1024,
                )
                .unwrap();
            assert_eq!(decoded.as_bytes(), image.rotate180().as_bytes());
        }
    }
    #[test]
    fn published_signed_deskews_preserve_scale_geometry_and_verified_samples() {
        for angle in [-2.0, 2.0] {
            let image = crate::geometry_analysis::tests::skewed_edge(angle);
            let (dir, native) = crate::geometry_analysis::tests::fixture(
                &[b"480 0 0 640 60 80 cm /Scan Do"],
                &image,
                false,
            );
            let pipeline = PdfPipeline::new(PipelineConfig::geometry_only(
                GeometryAction::Off,
                GeometryAction::Apply,
            ));
            let published =
                process_geometry(&pipeline, native.source_path(), &dir.path().join("bundle"))
                    .unwrap();
            let manifest = verify_geometry_bundle(&published.directory).unwrap();
            let page = &manifest.pages[0];
            assert_eq!(page.evidence.deskew.decision, TransformDecision::Applied);
            assert_eq!(page.geometry, page_geometry(&native.pages()[0]));
            assert!(page.output_images[0].width > image.width());
            let m = page.evidence.output_matrix.unwrap();
            assert!(
                (m[0] / f64::from(page.output_images[0].width) - 480.0 / f64::from(image.width()))
                    .abs()
                    < 1e-6
            );
            assert!(
                (m[3] / f64::from(page.output_images[0].height)
                    - 640.0 / f64::from(image.height()))
                .abs()
                    < 1e-6
            );
            assert!(page.output.decoded_pixel_sha256.is_some());
        }
    }
    #[test]
    fn rejected_clipping_publishes_unchanged_review_not_applied() {
        let image = crate::geometry_analysis::tests::skewed_edge(2.0);
        let (dir, native) = crate::geometry_analysis::tests::fixture(
            &[b"600 0 0 800 0 0 cm /Scan Do"],
            &image,
            false,
        );
        let pipeline = PdfPipeline::new(PipelineConfig::geometry_only(
            GeometryAction::Off,
            GeometryAction::Apply,
        ));
        let published =
            process_geometry(&pipeline, native.source_path(), &dir.path().join("bundle")).unwrap();
        let manifest = verify_geometry_bundle(&published.directory).unwrap();
        let page = &manifest.pages[0];
        assert_eq!(page.evidence.deskew.reason, "expanded_canvas_would_clip");
        assert_eq!(page.evidence.deskew.decision, TransformDecision::Rejected);
        assert!(page.output.reused && page.evidence.review_required);
        assert_eq!(page.source_images, page.output_images);
    }
    #[test]
    fn geometry_reader_rejects_relabeling_rotation_as_rgba_deskew() {
        let image = crate::geometry_analysis::tests::upside_down_text();
        let (dir, native) = crate::geometry_analysis::tests::fixture(
            &[b"600 0 0 800 0 0 cm /Scan Do"],
            &image,
            false,
        );
        let pipeline = PdfPipeline::new(PipelineConfig::geometry_only(
            GeometryAction::Apply,
            GeometryAction::Off,
        ));
        let output =
            process_geometry(&pipeline, native.source_path(), &dir.path().join("bundle")).unwrap();
        let mut manifest = verify_geometry_bundle(&output.directory).unwrap();
        manifest.header.policy.deskew_action = GeometryAction::Apply;
        let e = &mut manifest.pages[0].evidence;
        e.deskew = crate::transform_manifest::DeskewTransform::new(
            1.0,
            1.0,
            100,
            TransformDecision::Applied,
            crate::DeskewReason::CorrectionEvidence.to_string(),
        )
        .unwrap();
        e.deskew_pixels_changed = true;
        e.deskew_input_after_rotation = true;
        e.deskew_application = Some(DeskewApplicationMetadata::current());
        let mut json = serde_json::to_string(&manifest.header).unwrap() + "\n";
        for page in &manifest.pages {
            json.push_str(&(serde_json::to_string(page).unwrap() + "\n"));
        }
        fs::write(&output.manifest_path, json).unwrap();
        assert!(verify_geometry_bundle(&output.directory).is_err());
    }

    #[test]
    fn geometry_manifest_rejects_applied_claims_in_report_output() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.pdf");
        fixture(&source);
        let pipeline = PdfPipeline::new(PipelineConfig::geometry_only(
            GeometryAction::Report,
            GeometryAction::Report,
        ));
        let output = process_geometry(&pipeline, &source, &dir.path().join("bundle")).unwrap();
        let mut manifest = verify_geometry_bundle(&output.directory).unwrap();
        manifest.pages[0].evidence.rotation.decision = TransformDecision::Applied;
        manifest.pages[0].evidence.rotation_pixels_changed = true;
        assert!(manifest.validate().is_err());
    }
    #[test]
    fn geometry_rejects_max_pages_before_publication() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.pdf");
        fixture(&source);
        let config = PipelineConfig::geometry_only(GeometryAction::Report, GeometryAction::Report)
            .with_max_pages(Some(1));
        let output = dir.path().join("bundle");
        assert!(process_geometry(&PdfPipeline::new(config), &source, &output).is_err());
        assert!(!output.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
