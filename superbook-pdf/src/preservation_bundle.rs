//! Atomic native PDF/output-audit bundles. This is not the geometry policy dispatcher.

use crate::image_extract::{NativePdfDocument, NativePdfExtractor};
use crate::preservation_writer::{write_staged, PageWriteReceipt, RasterReplacement};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const VERSION: u32 = 2;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Native(#[from] crate::image_extract::NativeExtractError),
    #[error(transparent)]
    Writer(#[from] crate::preservation_writer::PreservationWriterError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid preservation bundle: {0}")]
    Invalid(String),
    #[error("destination already exists; immutable bundles are never replaced")]
    DestinationExists,
    #[error("source changed during bundle creation")]
    SourceChanged,
    #[error("atomic no-replace bundle publication is not implemented on this platform")]
    UnsupportedPlatform,
    #[error("bundle exists at {path:?}, but parent sync failed: {source}")]
    DurabilityUncertain {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OutputHeader {
    pub schema_version: u32,
    pub record_type: String,
    pub source_sha256: String,
    pub source_byte_length: u64,
    pub pdf_sha256: String,
    pub pdf_byte_length: u64,
    pub page_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImageIdentity {
    pub object_number: u32,
    pub generation: u16,
    pub encoded_sha256: String,
    pub encoded_length: usize,
    pub width: u32,
    pub height: u32,
    pub bits_per_component: Option<u8>,
    pub color_space: Option<serde_json::Value>,
    pub filters: Vec<String>,
    pub decode_params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PageOutputRecord {
    pub schema_version: u32,
    pub record_type: String,
    pub page_index: usize,
    pub source_page_number: usize,
    pub source_page_object: (u32, u16),
    pub media_box: Option<[f64; 4]>,
    pub crop_box: Option<[f64; 4]>,
    pub rotation: Option<u16>,
    pub source_images: Vec<ImageIdentity>,
    pub review_required: bool,
    pub issue_codes: Vec<String>,
    pub output: PageWriteReceipt,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputManifest {
    pub header: OutputHeader,
    pub pages: Vec<PageOutputRecord>,
}

#[derive(Debug)]
pub struct PublishedBundle {
    pub directory: PathBuf,
    pub pdf_path: PathBuf,
    pub manifest_path: PathBuf,
}

fn invalid(message: &str) -> BundleError {
    BundleError::Invalid(message.into())
}
pub(crate) fn hash_file(path: &Path) -> Result<(String, u64), BundleError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(invalid("expected regular file"));
    }
    let mut input = File::open(path)?;
    let mut digest = Sha256::new();
    let mut length = 0u64;
    let mut bytes = [0u8; 65536];
    loop {
        let count = input.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        digest.update(&bytes[..count]);
        length = length
            .checked_add(count as u64)
            .ok_or_else(|| invalid("file length overflow"))?;
    }
    Ok((format!("{:x}", digest.finalize()), length))
}
pub(crate) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl OutputManifest {
    pub fn validate(&self) -> Result<(), BundleError> {
        let h = &self.header;
        if h.schema_version != VERSION
            || h.record_type != "preservation_output"
            || !valid_hash(&h.source_sha256)
            || !valid_hash(&h.pdf_sha256)
            || h.source_byte_length == 0
            || h.pdf_byte_length == 0
            || h.page_count == 0
            || h.page_count != self.pages.len()
            || h.page_count > 100_000
        {
            return Err(invalid("invalid header, version or page cardinality"));
        }
        for (index, page) in self.pages.iter().enumerate() {
            if page.schema_version != VERSION
                || page.record_type != "page_output"
                || page.page_index != index
                || page.source_page_number != index + 1
                || page.output.page_index != index
                || page.source_page_object.0 == 0
            {
                return Err(invalid("unordered, misnumbered or invalid page record"));
            }
            for rect in [page.media_box, page.crop_box].into_iter().flatten() {
                crate::pdf_reader::PdfRect::try_new(rect)
                    .map_err(|_| invalid("invalid page box"))?;
            }
            if page
                .rotation
                .is_some_and(|r| ![0, 90, 180, 270].contains(&r))
            {
                return Err(invalid("invalid page rotation"));
            }
            for image in &page.source_images {
                if !valid_hash(&image.encoded_sha256)
                    || image.object_number == 0
                    || image.width == 0
                    || image.height == 0
                {
                    return Err(invalid("invalid source image identity"));
                }
            }
            for value in [
                &page.output.source_image_sha256,
                &page.output.output_image_sha256,
                &page.output.decoded_pixel_sha256,
            ]
            .into_iter()
            .flatten()
            {
                if !valid_hash(value) {
                    return Err(invalid("invalid receipt hash"));
                }
            }
            if page.output.reused {
                if page.output.source_image_sha256 != page.output.output_image_sha256
                    || page.output.decoded_pixel_sha256.is_some()
                {
                    return Err(invalid("inconsistent unchanged receipt"));
                }
            } else if page.source_images.len() != 1
                || page.output.decoded_pixel_sha256.is_none()
                || page.output.output_image_sha256.is_none()
                || page.output.source_image_sha256.as_deref()
                    != Some(page.source_images[0].encoded_sha256.as_str())
                || page.output.width != Some(page.source_images[0].width)
                || page.output.height != Some(page.source_images[0].height)
                || !matches!(page.output.bits_per_component, Some(8 | 16))
            {
                return Err(invalid("incomplete replacement receipt"));
            }
        }
        Ok(())
    }
}

fn build_manifest(
    native: &NativePdfDocument,
    source: (String, u64),
    receipt: crate::preservation_writer::StagedPdfReceipt,
) -> Result<OutputManifest, BundleError> {
    if native.pages().len() != receipt.pages.len() {
        return Err(invalid("writer omitted pages"));
    }
    let mut pages = Vec::with_capacity(receipt.pages.len());
    for (page, output) in native.pages().iter().zip(receipt.pages) {
        let physical = &page.physical_page;
        pages.push(PageOutputRecord {
            schema_version: VERSION,
            record_type: "page_output".into(),
            page_index: physical.page_index,
            source_page_number: physical.source_page_number,
            source_page_object: (
                physical.page_object_id.object_number,
                physical.page_object_id.generation,
            ),
            media_box: physical.media_box.as_ref().map(|b| b.value.coordinates()),
            crop_box: physical.crop_box.as_ref().map(|b| b.value.coordinates()),
            rotation: physical.normalized_rotation(),
            source_images: page
                .image_invocations
                .iter()
                .map(|image| {
                    let m = &image.metadata;
                    ImageIdentity {
                        object_number: m.object_id.object_number,
                        generation: m.object_id.generation,
                        encoded_sha256: m.encoded_sha256.clone(),
                        encoded_length: m.encoded_length,
                        width: m.width,
                        height: m.height,
                        bits_per_component: m.bits_per_component,
                        color_space: m.color_space.clone(),
                        filters: m.filters.clone(),
                        decode_params: m.decode_params.clone(),
                    }
                })
                .collect(),
            review_required: page.review_required,
            issue_codes: page
                .issues
                .iter()
                .map(|i| i.code.as_str().to_string())
                .collect(),
            output,
        });
    }
    let manifest = OutputManifest {
        header: OutputHeader {
            schema_version: VERSION,
            record_type: "preservation_output".into(),
            source_sha256: source.0,
            source_byte_length: source.1,
            pdf_sha256: receipt.pdf_sha256,
            pdf_byte_length: receipt.byte_length,
            page_count: pages.len(),
        },
        pages,
    };
    manifest.validate()?;
    Ok(manifest)
}

fn write_manifest(path: &Path, manifest: &OutputManifest) -> Result<(), BundleError> {
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

/// Verify schema/cardinality and bind an output manifest to its exact PDF bytes.
/// This detects accidental corruption, not an attacker rewriting both files.
pub fn verify_bundle(directory: &Path) -> Result<OutputManifest, BundleError> {
    let path = directory.join("output-manifest.jsonl");
    if !fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err(invalid("manifest is not a regular file"));
    }
    let mut text = String::new();
    File::open(path)?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_MANIFEST_BYTES || !text.ends_with('\n') {
        return Err(invalid("over-limit or incomplete manifest"));
    }
    let mut lines = text.lines();
    let header: OutputHeader =
        serde_json::from_str(lines.next().ok_or_else(|| invalid("missing header"))?)?;
    let pages = lines
        .map(serde_json::from_str)
        .collect::<Result<Vec<PageOutputRecord>, _>>()?;
    let manifest = OutputManifest { header, pages };
    manifest.validate()?;
    let (hash, length) = hash_file(&directory.join("document.pdf"))?;
    if hash != manifest.header.pdf_sha256 || length != manifest.header.pdf_byte_length {
        return Err(invalid("output PDF hash/length mismatch"));
    }
    let output = NativePdfExtractor::extract_path(&directory.join("document.pdf"))?;
    if output.pages().len() != manifest.pages.len() {
        return Err(invalid("PDF page count differs from manifest"));
    }
    for (actual, recorded) in output.pages().iter().zip(&manifest.pages) {
        let p = &actual.physical_page;
        if (p.page_object_id.object_number, p.page_object_id.generation)
            != recorded.source_page_object
            || p.page_index != recorded.page_index
            || p.media_box.as_ref().map(|b| b.value.coordinates()) != recorded.media_box
            || p.crop_box.as_ref().map(|b| b.value.coordinates()) != recorded.crop_box
            || p.normalized_rotation() != recorded.rotation
        {
            return Err(invalid("PDF page identity/geometry differs from manifest"));
        }
        if recorded.output.reused {
            let images: Vec<_> = actual
                .image_invocations
                .iter()
                .map(|i| image_identity(&i.metadata))
                .collect();
            if images != recorded.source_images {
                return Err(invalid("reused image metadata differs from manifest"));
            }
            if actual.image_invocations.len() == 1 && actual.image_invocations[0].binding.is_direct
            {
                let image = &actual.image_invocations[0].metadata;
                if recorded.output.source_image_sha256.as_deref()
                    != Some(image.encoded_sha256.as_str())
                    || recorded.output.width != Some(image.width)
                    || recorded.output.height != Some(image.height)
                    || recorded.output.bits_per_component != image.bits_per_component
                    || recorded.output.color_space.as_deref()
                        != image
                            .color_space
                            .as_ref()
                            .and_then(serde_json::Value::as_str)
                {
                    return Err(invalid("unchanged receipt differs from source image"));
                }
            } else if recorded.output.source_image_sha256.is_some()
                || recorded.output.width.is_some()
                || recorded.output.height.is_some()
                || recorded.output.bits_per_component.is_some()
                || recorded.output.color_space.is_some()
            {
                return Err(invalid(
                    "complex page has a fabricated single-image receipt",
                ));
            }
        } else {
            if actual.image_invocations.len() != 1 {
                return Err(invalid("replacement no longer resolves to one image"));
            }
            let image = &actual.image_invocations[0].metadata;
            let source_doc = output.source_document();
            let object = (image.object_id.object_number, image.object_id.generation);
            if recorded.output.decoded_pixel_sha256.as_deref()
                != Some(decoded_receipt_hash(source_doc, object)?.as_str())
            {
                return Err(invalid("decoded pixel hash differs from receipt"));
            }
            if recorded.output.output_image_sha256.as_deref() != Some(image.encoded_sha256.as_str())
                || recorded.output.width != Some(image.width)
                || recorded.output.height != Some(image.height)
                || recorded.output.bits_per_component != image.bits_per_component
                || recorded.output.color_space.as_deref()
                    != image
                        .color_space
                        .as_ref()
                        .and_then(serde_json::Value::as_str)
                || image.filters != ["FlateDecode"]
            {
                return Err(invalid("replacement image differs from verified receipt"));
            }
        }
    }
    Ok(manifest)
}

pub(crate) fn decoded_receipt_hash(
    doc: &lopdf::Document,
    id: lopdf::ObjectId,
) -> Result<String, BundleError> {
    let stream = doc
        .get_object(id)
        .and_then(lopdf::Object::as_stream)
        .map_err(|_| invalid("missing output image"))?;
    fn components(s: &lopdf::Stream) -> Result<(usize, usize, usize), BundleError> {
        let width = s
            .dict
            .get(b"Width")
            .and_then(lopdf::Object::as_i64)
            .ok()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| invalid("invalid output width"))?;
        let height = s
            .dict
            .get(b"Height")
            .and_then(lopdf::Object::as_i64)
            .ok()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| invalid("invalid output height"))?;
        let bits = s
            .dict
            .get(b"BitsPerComponent")
            .and_then(lopdf::Object::as_i64)
            .map_err(|_| invalid("invalid output bit depth"))?;
        let color = s
            .dict
            .get(b"ColorSpace")
            .and_then(lopdf::Object::as_name_str)
            .map_err(|_| invalid("invalid output color space"))?;
        let channels = match color {
            "DeviceGray" => 1,
            "DeviceRGB" => 3,
            _ => return Err(invalid("unsupported output color space")),
        };
        if width == 0
            || height == 0
            || ![8, 16].contains(&bits)
            || s.dict.has(b"DecodeParms")
            || s.dict
                .get(b"Filter")
                .and_then(lopdf::Object::as_name_str)
                .ok()
                != Some("FlateDecode")
        {
            return Err(invalid("unsupported output sample layout"));
        }
        let pixels = width
            .checked_mul(height)
            .ok_or_else(|| invalid("pixel count overflow"))?;
        Ok((pixels, channels, bits as usize / 8))
    }
    fn decode(s: &lopdf::Stream, expected: usize) -> Result<Vec<u8>, BundleError> {
        if expected > 256 * 1024 * 1024 {
            return Err(invalid("output receipt sample budget exceeded"));
        }
        let mut decoder = flate2::read::ZlibDecoder::new(s.content.as_slice());
        let mut bytes = Vec::new();
        decoder
            .by_ref()
            .take(expected as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() != expected || decoder.total_in() != s.content.len() as u64 {
            return Err(invalid("invalid output sample length"));
        }
        Ok(bytes)
    }
    let (pixels, channels, bytes_per_sample) = components(stream)?;
    let stride = channels * bytes_per_sample;
    let color_bytes = decode(
        stream,
        pixels
            .checked_mul(stride)
            .ok_or_else(|| invalid("sample length overflow"))?,
    )?;
    let mut digest = Sha256::new();
    if let Ok(value) = stream.dict.get(b"SMask") {
        let mask = value
            .as_reference()
            .ok()
            .and_then(|id| doc.get_object(id).ok())
            .and_then(|o| o.as_stream().ok())
            .ok_or_else(|| invalid("invalid output alpha mask"))?;
        if components(mask)? != (pixels, 1, bytes_per_sample)
            || mask.dict.has(b"SMask")
            || mask.dict.get(b"Width").ok() != stream.dict.get(b"Width").ok()
            || mask.dict.get(b"Height").ok() != stream.dict.get(b"Height").ok()
        {
            return Err(invalid("alpha layout differs from color image"));
        }
        let alpha = decode(
            mask,
            pixels
                .checked_mul(bytes_per_sample)
                .ok_or_else(|| invalid("alpha length overflow"))?,
        )?;
        for (color, alpha) in color_bytes
            .chunks_exact(stride)
            .zip(alpha.chunks_exact(bytes_per_sample))
        {
            digest.update(color);
            digest.update(alpha);
        }
    } else {
        digest.update(&color_bytes);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(crate) fn image_identity(m: &crate::image_extract::NativeImageMetadata) -> ImageIdentity {
    ImageIdentity {
        object_number: m.object_id.object_number,
        generation: m.object_id.generation,
        encoded_sha256: m.encoded_sha256.clone(),
        encoded_length: m.encoded_length,
        width: m.width,
        height: m.height,
        bits_per_component: m.bits_per_component,
        color_space: m.color_space.clone(),
        filters: m.filters.clone(),
        decode_params: m.decode_params.clone(),
    }
}

/// Publish a new immutable PDF/audit directory; never overwrite an existing path.
/// Automatic rotation/deskew policy is deliberately not inferred from replacements.
pub fn write_preservation_bundle(
    input: &Path,
    replacements: &[RasterReplacement],
    destination: &Path,
) -> Result<PublishedBundle, BundleError> {
    if !cfg!(target_os = "linux") {
        return Err(BundleError::UnsupportedPlatform);
    }
    if !fs::symlink_metadata(input)?.file_type().is_file() {
        return Err(invalid("source must be a regular file"));
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => return Err(BundleError::DestinationExists),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("missing bundle name"))?;
    let target = parent.join(name);
    let staged = tempfile::Builder::new()
        .prefix(".preservation-")
        .tempdir_in(&parent)?;
    // Parse the same immutable bytes whose hash will be recorded, not a separately
    // reopened source file that can change between hashing and parsing.
    let snapshot = staged.path().join("source-snapshot.pdf");
    let mut snapshot_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&snapshot)?;
    std::io::copy(&mut File::open(input)?, &mut snapshot_file)?;
    snapshot_file.sync_all()?;
    drop(snapshot_file);
    let source = hash_file(&snapshot)?;
    let native = NativePdfExtractor::extract_path(&snapshot)?;
    let receipt = write_staged(&native, replacements, &staged.path().join("document.pdf"))?;
    let manifest = build_manifest(&native, source.clone(), receipt)?;
    write_manifest(&staged.path().join("output-manifest.jsonl"), &manifest)?;
    if verify_bundle(staged.path())? != manifest {
        return Err(invalid("manifest readback differs"));
    }
    if hash_file(input)? != source {
        return Err(BundleError::SourceChanged);
    }
    fs::remove_file(snapshot)?;
    File::open(staged.path())?.sync_all()?;
    publish_directory(staged.path(), &target)?;
    if let Err(source) = File::open(&parent).and_then(|f| f.sync_all()) {
        return Err(BundleError::DurabilityUncertain {
            path: target,
            source,
        });
    }
    let published = PublishedBundle {
        pdf_path: target.join("document.pdf"),
        manifest_path: target.join("output-manifest.jsonl"),
        directory: target,
    };
    Ok(published)
}

#[cfg(target_os = "linux")]
pub(crate) fn publish_directory(from: &Path, to: &Path) -> Result<(), BundleError> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(|e| BundleError::Io(e.into()))
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn publish_directory(_from: &Path, _to: &Path) -> Result<(), BundleError> {
    Err(BundleError::UnsupportedPlatform)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::preservation_writer::RasterReplacement;
    use lopdf::{dictionary, Document, Object, Stream};
    use std::io::Write;

    fn fixture(path: &Path) {
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&[1_u8, 2, 3, 4]).unwrap();
        let image = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => 2, "Height" => 2,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8, "Filter" => "FlateDecode",
            },
            encoder.finish().unwrap(),
        ));
        let content = doc.add_object(Stream::new(
            dictionary! {},
            b"q 20 0 0 20 1 2 cm /Scan Do Q".to_vec(),
        ));
        let mut kids = Vec::new();
        for index in 0..3 {
            let mut page = dictionary! { "Type" => "Page", "Parent" => pages,
                "MediaBox" => vec![1.into(),2.into(),21.into(),22.into()],
            };
            if index != 1 {
                page.set(
                    "Resources",
                    dictionary! {"XObject" => dictionary! {"Scan" => image}},
                );
                page.set("Contents", content);
            }
            kids.push(Object::Reference(doc.add_object(page)));
        }
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! {"Type" => "Pages", "Count" => 3, "Kids" => kids}),
        );
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        doc.save(path).unwrap();
    }

    #[test]
    fn bundle_roundtrip_retains_blank_and_shared_pages_and_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.pdf");
        fixture(&source);
        let before = std::fs::read(&source).unwrap();
        let destination = dir.path().join("published");
        let published = write_preservation_bundle(
            &source,
            &[RasterReplacement {
                page_index: 0,
                image: image::DynamicImage::ImageLuma8(
                    image::GrayImage::from_raw(2, 2, vec![4, 3, 2, 1]).unwrap(),
                ),
            }],
            &destination,
        )
        .unwrap();
        let manifest = verify_bundle(&destination).unwrap();
        assert_eq!(manifest.pages.len(), 3);
        assert!(!manifest.pages[0].output.reused);
        assert!(manifest.pages[1].output.reused);
        assert!(manifest.pages[2].output.reused);
        assert_eq!(manifest.pages[1].source_images.len(), 0);
        assert_eq!(
            manifest.pages[0].source_images,
            manifest.pages[2].source_images
        );
        assert_eq!(std::fs::read(&source).unwrap(), before);
        assert_eq!(published.pdf_path, destination.join("document.pdf"));
        assert!(published.manifest_path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn bundle_rechecks_alpha_samples_and_rejects_forged_pixel_hash() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.pdf");
        fixture(&source);
        for (index, image) in [
            image::DynamicImage::new_rgba8(2, 2),
            image::DynamicImage::new_rgba16(2, 2),
        ]
        .into_iter()
        .enumerate()
        {
            let result = write_preservation_bundle(
                &source,
                &[RasterReplacement {
                    page_index: 0,
                    image,
                }],
                &dir.path().join(format!("bundle-{index}")),
            )
            .unwrap();
            let mut manifest = verify_bundle(&result.directory).unwrap();
            manifest.pages[0].output.decoded_pixel_sha256 = Some("0".repeat(64));
            fs::remove_file(&result.manifest_path).unwrap();
            write_manifest(&result.manifest_path, &manifest).unwrap();
            assert!(verify_bundle(&result.directory).is_err());
        }
    }

    #[test]
    fn bundle_never_clobbers_destination_or_publishes_failed_writer() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.pdf");
        fixture(&source);
        let destination = dir.path().join("published");
        std::fs::create_dir(&destination).unwrap();
        assert!(write_preservation_bundle(&source, &[], &destination).is_err());
        assert!(destination.is_dir());
        std::fs::remove_dir(&destination).unwrap();
        let wrong_size = RasterReplacement {
            page_index: 0,
            image: image::DynamicImage::new_luma8(3, 3),
        };
        assert!(write_preservation_bundle(&source, &[wrong_size], &destination).is_err());
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn bundle_reader_rejects_output_and_manifest_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.pdf");
        fixture(&source);
        let destination = dir.path().join("published");
        let result = write_preservation_bundle(&source, &[], &destination).unwrap();
        let original = std::fs::read_to_string(&result.manifest_path).unwrap();
        let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
        lines.swap(1, 2);
        std::fs::write(&result.manifest_path, lines.join("\n") + "\n").unwrap();
        assert!(verify_bundle(&destination).is_err());
        std::fs::write(
            &result.manifest_path,
            original.replace("\"schema_version\":2", "\"schema_version\":1"),
        )
        .unwrap();
        assert!(verify_bundle(&destination).is_err());
        std::fs::write(&result.manifest_path, &original).unwrap();
        let mut records: Vec<serde_json::Value> = original
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        records[1]["source_page_object"][0] = serde_json::json!(99999);
        let forged = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect::<String>();
        std::fs::write(&result.manifest_path, forged).unwrap();
        assert!(verify_bundle(&destination).is_err());
        std::fs::write(&result.manifest_path, &original).unwrap();
        std::fs::write(&result.pdf_path, b"not a PDF").unwrap();
        assert!(verify_bundle(&destination).is_err());
    }
}
