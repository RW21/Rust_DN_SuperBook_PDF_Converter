//! Copy-on-write, lossless staged PDF preservation writer.
use image::DynamicImage;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub struct RasterReplacement {
    pub page_index: usize,
    pub image: DynamicImage,
}
/// RGBA8 deskew canvas, placed at native pixel scale without changing page boxes.
pub struct ExpandedRasterReplacement {
    pub page_index: usize,
    pub image: DynamicImage,
}
pub struct StagedPdfReceipt {
    pub pdf_sha256: String,
    pub byte_length: u64,
    pub pages: Vec<PageWriteReceipt>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PageWriteReceipt {
    pub page_index: usize,
    pub reused: bool,
    pub source_image_sha256: Option<String>,
    pub output_image_sha256: Option<String>,
    pub decoded_pixel_sha256: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bits_per_component: Option<u8>,
    pub color_space: Option<String>,
}
#[derive(Debug, thiserror::Error)]
pub enum PreservationWriterError {
    #[error("stage target already exists or aliases source")]
    ExistingTarget,
    #[error("encrypted or signed PDF cannot be rewritten")]
    ProtectedSource,
    #[error("unsupported page or replacement: {0}")]
    Unsupported(String),
    #[error("replacement dimensions differ from source; expanded deskew is not supported")]
    DimensionMismatch,
    #[error("expanded canvas would clip visible nonwhite pixels")]
    ExpandedCanvasWouldClip,
    #[error("saved PDF verification failed: {0}")]
    Verification(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Pdf(#[from] lopdf::Error),
    #[error(transparent)]
    Native(#[from] crate::image_extract::NativeExtractError),
}
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read, Seek, SeekFrom, Write},
};
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn unsupported(message: &str) -> PreservationWriterError {
    PreservationWriterError::Unsupported(message.into())
}
fn protected(doc: &Document, obj: &Object, depth: usize) -> bool {
    if depth > 128 {
        return true;
    }
    match obj {
        Object::Dictionary(d) => d.iter().any(|(k, v)| {
            ((k == b"Type" || k == b"FT")
                && doc.dereference(v).ok().and_then(|(_, o)| o.as_name().ok()) == Some(b"Sig"))
                || protected(doc, v, depth + 1)
        }),
        Object::Stream(s) => protected(doc, &Object::Dictionary(s.dict.clone()), depth + 1),
        Object::Array(a) => a.iter().any(|v| protected(doc, v, depth + 1)),
        _ => false,
    }
}
fn effective_resources(
    doc: &Document,
    page: ObjectId,
) -> Result<Dictionary, PreservationWriterError> {
    let mut id = page;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(id) {
            return Err(unsupported("page parent cycle"));
        }
        let d = doc.get_dictionary(id)?;
        if let Ok(r) = d.get(b"Resources") {
            return Ok(doc.dereference(r)?.1.as_dict()?.clone());
        }
        id = d.get(b"Parent")?.as_reference()?;
    }
}
struct Samples {
    color: Vec<u8>,
    alpha: Option<Vec<u8>>,
    canonical: Vec<u8>,
    bits: u8,
    space: &'static str,
}
fn samples(image: &DynamicImage) -> Result<Samples, PreservationWriterError> {
    let (canonical, channels, bits, alpha) = match image {
        DynamicImage::ImageLuma8(i) => (i.as_raw().clone(), 1, 8, false),
        DynamicImage::ImageLumaA8(i) => (i.as_raw().clone(), 2, 8, true),
        DynamicImage::ImageRgb8(i) => (i.as_raw().clone(), 3, 8, false),
        DynamicImage::ImageRgba8(i) => (i.as_raw().clone(), 4, 8, true),
        DynamicImage::ImageLuma16(i) => (
            i.as_raw().iter().flat_map(|x| x.to_be_bytes()).collect(),
            1,
            16,
            false,
        ),
        DynamicImage::ImageLumaA16(i) => (
            i.as_raw().iter().flat_map(|x| x.to_be_bytes()).collect(),
            2,
            16,
            true,
        ),
        DynamicImage::ImageRgb16(i) => (
            i.as_raw().iter().flat_map(|x| x.to_be_bytes()).collect(),
            3,
            16,
            false,
        ),
        DynamicImage::ImageRgba16(i) => (
            i.as_raw().iter().flat_map(|x| x.to_be_bytes()).collect(),
            4,
            16,
            true,
        ),
        _ => return Err(unsupported("floating-point or unknown pixel mode")),
    };
    let step = bits as usize / 8;
    let colors = channels - usize::from(alpha);
    let mut color = Vec::new();
    let mut mask = alpha.then(Vec::new);
    for pixel in canonical.chunks_exact(channels * step) {
        color.extend_from_slice(&pixel[..colors * step]);
        if let Some(a) = mask.as_mut() {
            a.extend_from_slice(&pixel[colors * step..]);
        }
    }
    Ok(Samples {
        color,
        alpha: mask,
        canonical,
        bits,
        space: if colors == 1 {
            "DeviceGray"
        } else {
            "DeviceRGB"
        },
    })
}
fn image_stream(
    bytes: &[u8],
    w: u32,
    h: u32,
    bits: u8,
    space: &str,
) -> Result<Stream, PreservationWriterError> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(bytes)?;
    let stream = Stream::new(
        dictionary! {"Type"=>"XObject", "Subtype"=>"Image", "Width"=>i64::from(w), "Height"=>i64::from(h), "BitsPerComponent"=>i64::from(bits), "ColorSpace"=>space, "Filter"=>"FlateDecode"},
        z.finish()?,
    );
    Ok(stream)
}
/// Prove that the expanded RGBA8 canvas loses no visible nonwhite pixel cells.
/// Returned coefficients are quantized to PDF Real (f32) BEFORE the proof.
/// White/fully transparent padding may extend beyond unchanged page boxes.
pub fn validate_expanded_placement(
    native: &crate::image_extract::NativePdfDocument,
    page_index: usize,
    image: &DynamicImage,
) -> Result<[f64; 6], PreservationWriterError> {
    let source = native.source_document();
    if source.trailer.has(b"Encrypt")
        || source.objects.values().any(|o| protected(source, o, 0))
        || protected(source, &Object::Dictionary(source.trailer.clone()), 0)
    {
        return Err(PreservationWriterError::ProtectedSource);
    }
    let page = native
        .pages()
        .get(page_index)
        .ok_or_else(|| unsupported("out-of-range page index"))?;
    if page.review_required
        || page.kind != crate::image_extract::NativePageKind::SingleImage
        || page.image_invocations.len() != 1
        || !page.physical_page.issues.is_empty()
    {
        return Err(unsupported(
            "expanded replacement needs a supported pure single-image page",
        ));
    }
    let inv = &page.image_invocations[0];
    if !inv.binding.is_direct || inv.binding.resource_path.len() != 1 {
        return Err(unsupported(
            "expanded replacement needs a direct image binding",
        ));
    }
    let [a, b, c, d, e, f] = inv.binding.placement_matrix.coordinates();
    let det = a * d - b * c;
    if ![a, b, c, d, e, f, det].iter().all(|v| v.is_finite()) || det <= 0.0 {
        return Err(unsupported(
            "placement must be finite with positive nonsingular determinant",
        ));
    }
    let (old_w, old_h) = (inv.metadata.width, inv.metadata.height);
    let (w, h) = (image.width(), image.height());
    if old_w == 0 || old_h == 0 || w == 0 || h == 0 || w < old_w || h < old_h {
        return Err(PreservationWriterError::DimensionMismatch);
    }
    let rgba = image
        .as_rgba8()
        .ok_or_else(|| unsupported("expanded replacement requires RGBA8"))?;
    u64::from(w)
        .checked_mul(u64::from(h))
        .and_then(|n| n.checked_mul(4))
        .filter(|n| *n <= 256 * 1024 * 1024)
        .ok_or_else(|| unsupported("expanded replacement exceeds 256 MiB sample budget"))?;
    native.decode_image(&inv.metadata, 256 * 1024 * 1024)?;
    let sx = f64::from(w) / f64::from(old_w);
    let sy = f64::from(h) / f64::from(old_h);
    let tx = -f64::from(w - old_w) / (2.0 * f64::from(old_w));
    let ty = -f64::from(h - old_h) / (2.0 * f64::from(old_h));
    let matrix = [
        a * sx,
        b * sx,
        c * sy,
        d * sy,
        a * tx + c * ty + e,
        b * tx + d * ty + f,
    ]
    .map(|v| f64::from(v as f32));
    let [a, b, c, d, e, f] = matrix;
    let det = a * d - b * c;
    if !matrix.iter().all(|v| v.is_finite()) || !det.is_finite() || det <= 0.0 {
        return Err(unsupported(
            "expanded PDF Real placement is nonfinite or degenerate",
        ));
    }
    let geometry = &page.physical_page;
    let media = geometry
        .media_box
        .as_ref()
        .ok_or_else(|| unsupported("missing valid MediaBox"))?
        .value
        .coordinates();
    let crop = geometry
        .effective_crop_box()
        .ok_or_else(|| unsupported("missing valid CropBox"))?
        .coordinates();
    let visible = [
        media[0].max(crop[0]),
        media[1].max(crop[1]),
        media[2].min(crop[2]),
        media[3].min(crop[3]),
    ];
    if visible[0] >= visible[2] || visible[1] >= visible[3] {
        return Err(unsupported("empty MediaBox/CropBox intersection"));
    }
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, 0, 0);
    for (x, y, pixel) in rgba.enumerate_pixels() {
        if pixel[3] != 0 && pixel.0[..3] != [255, 255, 255] {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x + 1);
            max_y = max_y.max(y + 1);
        }
    }
    if min_x < max_x && min_y < max_y {
        // Include complete pixel cells, not just centers. Forward-map all corners
        // so rotation and skew do not weaken the conservative containment proof.
        for x in [min_x, max_x] {
            for y in [min_y, max_y] {
                let u = f64::from(x) / f64::from(w);
                let v = 1.0 - f64::from(y) / f64::from(h);
                let px = a * u + c * v + e;
                let py = b * u + d * v + f;
                if !px.is_finite()
                    || !py.is_finite()
                    || px < visible[0]
                    || px > visible[2]
                    || py < visible[1]
                    || py > visible[3]
                {
                    return Err(PreservationWriterError::ExpandedCanvasWouldClip);
                }
            }
        }
    }
    Ok(matrix)
}

/// Write a new, private stage only. Decoded hashes use row-major interleaved
/// channels (including alpha), with 16-bit samples in PDF big-endian order.
/// Encoded image hashes cover the color stream; alpha is separately verified.
pub fn write_staged(
    native: &crate::image_extract::NativePdfDocument,
    replacements: &[RasterReplacement],
    output: &Path,
) -> Result<StagedPdfReceipt, PreservationWriterError> {
    write_staged_geometry(native, replacements, &[], output)
}

/// Write same-size and expanded replacements atomically to a private new stage.
/// Expanded pages get private content streams; original geometry is unchanged.
pub fn write_staged_geometry(
    native: &crate::image_extract::NativePdfDocument,
    replacements: &[RasterReplacement],
    expanded: &[ExpandedRasterReplacement],
    output: &Path,
) -> Result<StagedPdfReceipt, PreservationWriterError> {
    match std::fs::symlink_metadata(output) {
        Ok(_) => return Err(PreservationWriterError::ExistingTarget),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let target = parent.canonicalize()?.join(
        output
            .file_name()
            .ok_or_else(|| unsupported("invalid stage path"))?,
    );
    if target == native.source_path().canonicalize()? {
        return Err(PreservationWriterError::ExistingTarget);
    }
    let source = native.source_document();
    if source.trailer.has(b"Encrypt")
        || source.objects.values().any(|o| protected(source, o, 0))
        || protected(source, &Object::Dictionary(source.trailer.clone()), 0)
    {
        return Err(PreservationWriterError::ProtectedSource);
    }
    let additional = replacements
        .len()
        .checked_mul(2)
        .and_then(|n| expanded.len().checked_mul(3).and_then(|m| n.checked_add(m)))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| unsupported("replacement object count overflow"))?;
    source
        .max_id
        .checked_add(additional)
        .ok_or_else(|| unsupported("PDF object identifier capacity exhausted"))?;
    let mut doc = source.clone();
    // Hex-escaped PDF names are part of PDF 1.2, even in no-op output.
    let (major, minor) = doc
        .version
        .split_once('.')
        .ok_or_else(|| unsupported("invalid PDF version"))?;
    let major: u32 = major
        .parse()
        .map_err(|_| unsupported("invalid PDF version"))?;
    let minor: u32 = minor
        .parse()
        .map_err(|_| unsupported("invalid PDF version"))?;
    if (major, minor) < (1, 2) {
        doc.version = "1.2".into();
    }
    let page_ids = source.get_pages();
    if page_ids.len() != native.pages().len() {
        return Err(unsupported("page inventory mismatch"));
    }
    let mut selected = BTreeMap::new();
    for r in replacements {
        if r.page_index >= native.pages().len()
            || selected.insert(r.page_index, (&r.image, None)).is_some()
        {
            return Err(unsupported("duplicate or out-of-range page index"));
        }
    }
    for r in expanded {
        if r.page_index >= native.pages().len() || selected.contains_key(&r.page_index) {
            return Err(unsupported("duplicate or out-of-range page index"));
        }
        let matrix = validate_expanded_placement(native, r.page_index, &r.image)?;
        selected.insert(r.page_index, (&r.image, Some(matrix)));
    }
    let mut pages = Vec::new();
    let mut expected_streams = Vec::new();
    let mut changed = HashSet::new();
    let mut changed_contents = HashSet::new();
    for (index, page) in native.pages().iter().enumerate() {
        let direct =
            if page.image_invocations.len() == 1 && page.image_invocations[0].binding.is_direct {
                Some(&page.image_invocations[0])
            } else {
                None
            };
        let mut receipt = PageWriteReceipt {
            page_index: index,
            reused: true,
            source_image_sha256: None,
            output_image_sha256: None,
            decoded_pixel_sha256: None,
            width: None,
            height: None,
            bits_per_component: None,
            color_space: None,
        };
        if let Some(inv) = direct {
            let bytes = native.encoded_image_bytes(&inv.metadata)?;
            receipt.source_image_sha256 = Some(hash(bytes));
            receipt.output_image_sha256 = Some(hash(bytes));
            receipt.width = Some(inv.metadata.width);
            receipt.height = Some(inv.metadata.height);
            receipt.bits_per_component = inv.metadata.bits_per_component;
            receipt.color_space = inv
                .metadata
                .color_space
                .as_ref()
                .and_then(|c| c.as_str().map(str::to_owned));
        }
        if let Some((image, placement)) = selected.get(&index) {
            let inv = direct.ok_or_else(|| {
                unsupported("replacement needs exactly one direct image occurrence")
            })?;
            if page.review_required || inv.binding.resource_path.len() != 1 {
                return Err(unsupported(
                    "page requires review or has non-direct resources",
                ));
            }
            let [a, b, c, d, e, f] = inv.binding.placement_matrix.coordinates();
            let det = a * d - b * c;
            if ![a, b, c, d, e, f, det].iter().all(|x| x.is_finite()) || det <= 0.0 {
                return Err(unsupported(
                    "placement must be finite with positive nonsingular determinant",
                ));
            }
            if placement.is_none()
                && (image.width() != inv.metadata.width
                    || image.height() != inv.metadata.height
                    || image.width() == 0
                    || image.height() == 0)
            {
                return Err(PreservationWriterError::DimensionMismatch);
            }
            // Revalidate source decode semantics rather than reinterpreting masks,
            // Decode arrays, predictors, or unsupported source color spaces.
            if placement.is_none() {
                native.decode_image(&inv.metadata, 256 * 1024 * 1024)?;
            }
            let s = samples(image)?;
            let minimum_minor = if s.bits == 16 {
                5
            } else if s.alpha.is_some() {
                4
            } else {
                2
            };
            let (major, minor) = doc
                .version
                .split_once('.')
                .ok_or_else(|| unsupported("invalid PDF version"))?;
            let major: u32 = major
                .parse()
                .map_err(|_| unsupported("invalid PDF version"))?;
            let minor: u32 = minor
                .parse()
                .map_err(|_| unsupported("invalid PDF version"))?;
            if (major, minor) < (1, minimum_minor) {
                doc.version = format!("1.{minimum_minor}");
            }
            let mut stream =
                image_stream(&s.color, image.width(), image.height(), s.bits, s.space)?;
            if let Some(alpha) = &s.alpha {
                let mask =
                    image_stream(alpha, image.width(), image.height(), s.bits, "DeviceGray")?;
                let id = doc.add_object(mask);
                expected_streams.push((id, alpha.clone()));
                stream.dict.set("SMask", id);
            }
            receipt.output_image_sha256 = Some(hash(&stream.content));
            receipt.decoded_pixel_sha256 = Some(hash(&s.canonical));
            receipt.reused = false;
            receipt.width = Some(image.width());
            receipt.height = Some(image.height());
            receipt.bits_per_component = Some(s.bits);
            receipt.color_space = Some(format!("/{}", s.space));
            let new_id = doc.add_object(stream);
            expected_streams.push((new_id, s.color));
            let page_id = *page_ids
                .values()
                .nth(index)
                .ok_or_else(|| unsupported("missing page"))?;
            let mut resources = effective_resources(source, page_id)?;
            let mut xo = source
                .dereference(resources.get(b"XObject")?)?
                .1
                .as_dict()?
                .clone();
            let name = &inv.binding.resource_path[0].resource_name;
            let original = xo.get(name)?.as_reference()?;
            if original
                != (
                    inv.metadata.object_id.object_number,
                    inv.metadata.object_id.generation,
                )
            {
                return Err(unsupported("source binding mismatch"));
            }
            xo.set(name.clone(), new_id);
            resources.set("XObject", xo);
            doc.get_object_mut(page_id)?
                .as_dict_mut()?
                .set("Resources", resources);
            if let Some(matrix) = placement {
                // Use decimal PDF Reals and escape every name byte, including
                // whitespace/delimiters, independently of lopdf's content writer.
                let mut bytes = b"q\n".to_vec();
                for value in matrix {
                    let text = (*value as f32).to_string();
                    write!(
                        &mut bytes,
                        "{text}{} ",
                        if text.contains('.') { "" } else { ".0" }
                    )?;
                }
                bytes.extend_from_slice(b"cm\n/");
                for byte in name {
                    write!(&mut bytes, "#{byte:02X}")?;
                }
                bytes.extend_from_slice(b" Do\nQ\n");
                let content_id = doc.add_object(Stream::new(dictionary! {}, bytes));
                doc.get_object_mut(page_id)?
                    .as_dict_mut()?
                    .set("Contents", content_id);
                changed_contents.insert(page_id);
            }
            changed.insert(page_id);
        }
        pages.push(receipt);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(output).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            PreservationWriterError::ExistingTarget
        } else {
            e.into()
        }
    })?;
    let result = (|| {
        save_graph(&doc, &mut file)?;
        file.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let loaded = Document::load_mem(&bytes)?;
        if loaded.get_pages() != page_ids {
            return Err(PreservationWriterError::Verification(
                "page IDs/order changed".into(),
            ));
        }
        for (id, original) in &source.objects {
            let actual = loaded.objects.get(id).ok_or_else(|| {
                PreservationWriterError::Verification("source object missing".into())
            })?;
            let equal = if changed.contains(id) {
                let mut before = original.as_dict()?.clone();
                let mut after = actual.as_dict()?.clone();
                before.remove(b"Resources");
                after.remove(b"Resources");
                if changed_contents.contains(id) {
                    before.remove(b"Contents");
                    after.remove(b"Contents");
                }
                before == after
            } else {
                original == actual
            };
            if !equal {
                return Err(PreservationWriterError::Verification(format!(
                    "source object {id:?} changed"
                )));
            }
        }
        // Compare the complete expected graph as well, including local resource
        // bindings and every newly added image/mask dictionary and encoded stream.
        for (id, expected) in &doc.objects {
            if loaded.objects.get(id) != Some(expected) {
                return Err(PreservationWriterError::Verification(format!(
                    "saved object {id:?} differs"
                )));
            }
        }
        for (id, expected) in &expected_streams {
            let stream = loaded.get_object(*id)?.as_stream()?;
            let mut decoder = flate2::read::ZlibDecoder::new(stream.content.as_slice());
            let mut decoded = Vec::new();
            decoder
                .by_ref()
                .take(expected.len() as u64 + 1)
                .read_to_end(&mut decoded)?;
            if &decoded != expected {
                return Err(PreservationWriterError::Verification(
                    "decoded image/mask samples differ".into(),
                ));
            }
        }
        for (key, value) in source.trailer.iter() {
            if ![
                b"Size".as_slice(),
                b"Prev",
                b"XRefStm",
                b"Type",
                b"W",
                b"Index",
                b"Length",
                b"Filter",
                b"DecodeParms",
            ]
            .contains(&key.as_slice())
                && loaded.trailer.get(key).ok() != Some(value)
            {
                return Err(PreservationWriterError::Verification(
                    "trailer metadata changed".into(),
                ));
            }
        }
        Ok(StagedPdfReceipt {
            pdf_sha256: hash(&bytes),
            byte_length: bytes.len() as u64,
            pages,
        })
    })();
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result
}

// lopdf's save_to drops original XRef/ObjStm objects. Serialize the complete
// graph explicitly instead, retaining those objects as inert historical data.
fn save_graph(doc: &Document, file: &mut std::fs::File) -> Result<(), PreservationWriterError> {
    fn object(w: &mut dyn Write, o: &Object) -> std::io::Result<()> {
        match o {
            Object::Null => write!(w, "null"),
            Object::Boolean(v) => write!(w, "{v}"),
            Object::Integer(v) => write!(w, "{v}"),
            Object::Real(v) => {
                if !v.is_finite() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "nonfinite PDF number",
                    ));
                }
                let text = v.to_string();
                if text.contains('.') {
                    write!(w, "{text}")
                } else {
                    write!(w, "{text}.0")
                }
            }
            Object::Name(n) => {
                write!(w, "/")?;
                for b in n {
                    write!(w, "#{b:02X}")?;
                }
                Ok(())
            }
            Object::String(s, lopdf::StringFormat::Hexadecimal) => {
                write!(w, "<")?;
                for b in s {
                    write!(w, "{b:02X}")?;
                }
                write!(w, ">")
            }
            Object::String(s, lopdf::StringFormat::Literal) => {
                write!(w, "(")?;
                for b in s {
                    write!(w, "\\{b:03o}")?;
                }
                write!(w, ")")
            }
            Object::Reference((id, g)) => write!(w, "{id} {g} R"),
            Object::Array(a) => {
                write!(w, "[")?;
                for v in a {
                    object(w, v)?;
                    write!(w, " ")?;
                }
                write!(w, "]")
            }
            Object::Dictionary(d) => {
                write!(w, "<<")?;
                for (k, v) in d.iter() {
                    object(w, &Object::Name(k.clone()))?;
                    write!(w, " ")?;
                    object(w, v)?;
                    write!(w, " ")?;
                }
                write!(w, ">>")
            }
            Object::Stream(s) => {
                object(w, &Object::Dictionary(s.dict.clone()))?;
                write!(w, "\nstream\n")?;
                w.write_all(&s.content)?;
                write!(w, "\nendstream")
            }
        }
    }
    writeln!(file, "%PDF-{}", doc.version)?;
    let mut offsets = Vec::new();
    for (&(id, g), o) in &doc.objects {
        offsets.push((id, g, file.stream_position()?));
        writeln!(file, "{id} {g} obj")?;
        object(file, o)?;
        writeln!(file, "\nendobj")?;
    }
    let start = file.stream_position()?;
    writeln!(file, "xref\n0 1\n0000000000 65535 f ")?;
    for (id, g, offset) in offsets {
        if offset > 9_999_999_999 {
            return Err(unsupported("PDF exceeds classic xref offset capacity"));
        }
        writeln!(file, "{id} 1\n{offset:010} {g:05} n ")?;
    }
    let mut trailer = doc.trailer.clone();
    for key in [
        b"Prev".as_slice(),
        b"XRefStm",
        b"Type",
        b"W",
        b"Index",
        b"Length",
        b"Filter",
        b"DecodeParms",
    ] {
        trailer.remove(key);
    }
    trailer.set("Size", i64::from(doc.max_id) + 1);
    writeln!(file, "trailer")?;
    object(file, &Object::Dictionary(trailer))?;
    write!(file, "\nstartxref\n{start}\n%%EOF\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_extract::{NativePdfDocument, NativePdfExtractor};
    use lopdf::{dictionary, Document, Object, Stream};
    use std::io::Write;
    fn fixture(contents: &[&[u8]], signature: bool) -> (tempfile::TempDir, NativePdfDocument) {
        let dir = tempfile::tempdir().unwrap();
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&[1, 2, 3, 4]).unwrap();
        let img = doc.add_object(Stream::new(dictionary! {"Type"=>"XObject", "Subtype"=>"Image", "Width"=>2, "Height"=>2, "BitsPerComponent"=>8, "ColorSpace"=>"DeviceGray", "Filter"=>"FlateDecode"}, z.finish().unwrap()));
        let xo = doc.add_object(dictionary! {"Scan"=>img});
        let resources = doc.add_object(dictionary! {"XObject"=>xo});
        let mut kids = Vec::new();
        for bytes in contents {
            let content = doc.add_object(Stream::new(dictionary! {}, bytes.to_vec()));
            kids.push(Object::Reference(doc.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages, "Contents"=>content, "MediaBox"=>vec![0.into(),0.into(),20.into(),20.into()], "Rotate"=>90})));
        }
        doc.objects.insert(pages, Object::Dictionary(dictionary! {"Type"=>"Pages", "Count"=>kids.len() as i64, "Kids"=>kids, "Resources"=>resources}));
        let root = doc.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
        if signature {
            doc.add_object(dictionary! {"FT"=>"Sig"});
        }
        doc.trailer.set("Root", root);
        let path = dir.path().join("source.pdf");
        doc.save(&path).unwrap();
        let native = NativePdfExtractor::extract_path(&path).unwrap();
        (dir, native)
    }
    fn padded_image() -> DynamicImage {
        let mut image = image::RgbaImage::from_pixel(4, 4, image::Rgba([255; 4]));
        for y in 1..3 {
            for x in 1..3 {
                image.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        DynamicImage::ImageRgba8(image)
    }
    #[test]
    fn expanded_center_scale_rotations_and_shared_graph() {
        for (content, expected) in [
            (
                b"20 0 0 20 0 0 cm /Scan Do".as_slice(),
                [40., 0., 0., 40., -10., -10.],
            ),
            (
                b"0 20 -20 0 20 0 cm /Scan Do",
                [0., 40., -40., 0., 30., -10.],
            ),
            (
                b"0 -20 20 0 0 20 cm /Scan Do",
                [0., -40., 40., 0., -10., 30.],
            ),
            (
                b"-20 0 0 -20 20 20 cm /Scan Do",
                [-40., 0., 0., -40., 30., 30.],
            ),
            (b"10 2 3 10 3 3 cm /Scan Do", [20., 4., 6., 20., -3.5, -3.]),
        ] {
            let (dir, native) = fixture(&[content, content], false);
            let matrix = validate_expanded_placement(&native, 0, &padded_image()).unwrap();
            assert_eq!(matrix, expected);
            let out = dir.path().join("expanded.pdf");
            let receipt = write_staged_geometry(
                &native,
                &[],
                &[ExpandedRasterReplacement {
                    page_index: 0,
                    image: padded_image(),
                }],
                &out,
            )
            .unwrap();
            assert_eq!(
                (receipt.pages[0].width, receipt.pages[0].height),
                (Some(4), Some(4))
            );
            assert!(receipt.pages[1].reused);
            let after = Document::load(&out).unwrap();
            let page_id = native.source_document().get_pages()[&1];
            for (id, obj) in &native.source_document().objects {
                if *id != page_id {
                    assert_eq!(after.objects.get(id), Some(obj));
                }
            }
            let before = native.source_document().get_dictionary(page_id).unwrap();
            let page = after.get_dictionary(page_id).unwrap();
            for key in [b"MediaBox".as_slice(), b"CropBox", b"Rotate"] {
                assert_eq!(before.get(key).ok(), page.get(key).ok());
            }
            assert_ne!(
                before.get(b"Contents").unwrap(),
                page.get(b"Contents").unwrap()
            );
            let content =
                lopdf::content::Content::decode(&after.get_page_content(page_id).unwrap()).unwrap();
            assert_eq!(
                content
                    .operations
                    .iter()
                    .map(|op| op.operator.as_str())
                    .collect::<Vec<_>>(),
                vec!["q", "cm", "Do", "Q"]
            );
            assert_eq!(
                content.operations[1].operands,
                expected.map(|v| Object::Real(v as f32)).to_vec()
            );
        }
    }
    #[test]
    fn expanded_black_margin_rejected_without_stage_white_or_transparent_allowed() {
        let (dir, native) = fixture(&[b"20 0 0 20 0 0 cm /Scan Do"], false);
        let mut image = padded_image().into_rgba8();
        image.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
        let out = dir.path().join("clip.pdf");
        assert!(matches!(
            write_staged_geometry(
                &native,
                &[],
                &[ExpandedRasterReplacement {
                    page_index: 0,
                    image: DynamicImage::ImageRgba8(image.clone()),
                }],
                &out
            ),
            Err(PreservationWriterError::ExpandedCanvasWouldClip)
        ));
        assert!(!out.exists());
        image.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        assert!(validate_expanded_placement(&native, 0, &DynamicImage::ImageRgba8(image)).is_ok());
        assert!(validate_expanded_placement(&native, 0, &padded_image()).is_ok());
    }
    #[test]
    fn expanded_rejects_invalid_modes_dimensions_and_cross_list_duplicates() {
        let (dir, native) = fixture(&[b"20 0 0 20 0 0 cm /Scan Do"], false);
        for image in [
            DynamicImage::new_rgb8(4, 4),
            DynamicImage::new_rgba16(4, 4),
            DynamicImage::new_rgba8(1, 4),
            DynamicImage::new_rgba8(0, 4),
        ] {
            assert!(validate_expanded_placement(&native, 0, &image).is_err());
        }
        let out = dir.path().join("duplicate.pdf");
        assert!(write_staged_geometry(
            &native,
            &[RasterReplacement {
                page_index: 0,
                image: DynamicImage::new_luma8(2, 2)
            }],
            &[ExpandedRasterReplacement {
                page_index: 0,
                image: padded_image()
            }],
            &out
        )
        .is_err());
        assert!(!out.exists());
    }
    #[test]
    fn expanded_quantizes_before_proving_pixel_cell_containment() {
        let (dir, native) = fixture(&[b"20 0 0 20 0 0 cm /Scan Do"], false);
        let mut source = native.source_document().clone();
        let id = &native.pages()[0].image_invocations[0].metadata.object_id;
        source.objects.insert(
            (id.object_number, id.generation),
            Object::Stream(image_stream(&[1, 2, 3, 4, 5, 6], 3, 2, 8, "DeviceGray").unwrap()),
        );
        let path = dir.path().join("three-wide.pdf");
        save_graph(&source, &mut std::fs::File::create(&path).unwrap()).unwrap();
        let native = NativePdfExtractor::extract_path(&path).unwrap();
        let mut rgba = image::RgbaImage::from_pixel(5, 4, image::Rgba([255; 4]));
        let matrix =
            validate_expanded_placement(&native, 0, &DynamicImage::ImageRgba8(rgba.clone()))
                .unwrap();
        assert_eq!(matrix[0], f64::from((20.0_f64 * (5.0 / 3.0)) as f32));
        assert_eq!(matrix[4], f64::from((-20.0_f64 / 3.0) as f32));
        assert!(matrix[0] / 5.0 + matrix[4] < 0.0);
        // Mathematically centered source starts at zero; quantized cm moves its
        // left cell boundary slightly outside. Do not hide this with an epsilon.
        for y in 1..3 {
            for x in 1..4 {
                rgba.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        assert!(matches!(
            validate_expanded_placement(&native, 0, &DynamicImage::ImageRgba8(rgba)),
            Err(PreservationWriterError::ExpandedCanvasWouldClip)
        ));
    }
    #[test]
    fn expanded_intersects_crop_media_and_preserves_shared_contents() {
        let content = b"20 0 0 20 0 0 cm /Scan Do".as_slice();
        let (dir, native) = fixture(&[content, content], false);
        let mut source = native.source_document().clone();
        let ids = source.get_pages();
        let shared = source
            .get_dictionary(ids[&1])
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .clone();
        source
            .get_object_mut(ids[&2])
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", shared);
        source
            .get_object_mut(ids[&1])
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set(
                "CropBox",
                vec![(-10).into(), (-10).into(), 10.into(), 10.into()],
            );
        let path = dir.path().join("crop-shared.pdf");
        save_graph(&source, &mut std::fs::File::create(&path).unwrap()).unwrap();
        let native = NativePdfExtractor::extract_path(&path).unwrap();
        let mut rgba = image::RgbaImage::from_pixel(4, 4, image::Rgba([255; 4]));
        // Top-down pixel (1,2) covers PDF [0,0,10,10], the intersection.
        rgba.put_pixel(1, 2, image::Rgba([0, 0, 0, 1]));
        let out = dir.path().join("stage.pdf");
        write_staged_geometry(
            &native,
            &[],
            &[ExpandedRasterReplacement {
                page_index: 0,
                image: DynamicImage::ImageRgba8(rgba.clone()),
            }],
            &out,
        )
        .unwrap();
        let after = Document::load(out).unwrap();
        for (id, object) in &native.source_document().objects {
            if *id != ids[&1] {
                assert_eq!(after.objects.get(id), Some(object));
            }
        }
        let page = after.get_dictionary(ids[&1]).unwrap();
        for key in [b"MediaBox".as_slice(), b"CropBox", b"Rotate"] {
            assert_eq!(
                page.get(key).unwrap(),
                native
                    .source_document()
                    .get_dictionary(ids[&1])
                    .unwrap()
                    .get(key)
                    .unwrap()
            );
        }
        rgba.put_pixel(0, 3, image::Rgba([0, 0, 0, 255])); // inside CropBox but outside MediaBox
        assert!(matches!(
            validate_expanded_placement(&native, 0, &DynamicImage::ImageRgba8(rgba)),
            Err(PreservationWriterError::ExpandedCanvasWouldClip)
        ));
        assert!(matches!(
            validate_expanded_placement(&native, 0, &padded_image()),
            Err(PreservationWriterError::ExpandedCanvasWouldClip)
        )); // inside MediaBox but outside CropBox
    }
    #[test]
    fn expanded_rejects_review_singular_reflection_and_protected_sources() {
        for content in [
            b"/Scan Do /Scan Do".as_slice(),
            b"0 0 0 20 0 0 cm /Scan Do",
            b"-20 0 0 20 20 0 cm /Scan Do",
            b"0 0 10 10 re f /Scan Do",
        ] {
            let (dir, native) = fixture(&[content], false);
            let out = dir.path().join("stage.pdf");
            assert!(write_staged_geometry(
                &native,
                &[],
                &[ExpandedRasterReplacement {
                    page_index: 0,
                    image: padded_image()
                }],
                &out
            )
            .is_err());
            assert!(!out.exists());
        }
        let (dir, native) = fixture(&[b"20 0 0 20 0 0 cm /Scan Do"], true);
        assert!(matches!(
            validate_expanded_placement(&native, 0, &padded_image()),
            Err(PreservationWriterError::ProtectedSource)
        ));
        let out = dir.path().join("stage.pdf");
        assert!(matches!(
            write_staged_geometry(
                &native,
                &[],
                &[ExpandedRasterReplacement {
                    page_index: 0,
                    image: padded_image()
                }],
                &out
            ),
            Err(PreservationWriterError::ProtectedSource)
        ));
        assert!(!out.exists());
    }
    #[test]
    fn unchanged_mixed_blank_graph_and_shared_image() {
        let (dir, native) = fixture(&[b"/Scan Do", b"", b"/Scan Do /Scan Do"], false);
        let out = dir.path().join("stage.pdf");
        let receipt = write_staged(&native, &[], &out).unwrap();
        assert_eq!(receipt.pages.len(), 3);
        assert!(receipt.pages.iter().all(|p| p.reused));
        assert!(receipt.pages[0].source_image_sha256.is_some());
        assert!(receipt.pages[1].source_image_sha256.is_none());
        assert!(receipt.pages[2].source_image_sha256.is_none());
        let reloaded = Document::load(&out).unwrap();
        for (id, obj) in &native.source_document().objects {
            assert_eq!(reloaded.objects.get(id), Some(obj));
        }
    }
    #[test]
    fn replacements_all_integer_pixel_modes_copy_on_write() {
        let modes = vec![
            DynamicImage::new_luma8(2, 2),
            DynamicImage::new_rgb8(2, 2),
            DynamicImage::new_luma_a8(2, 2),
            DynamicImage::new_rgba8(2, 2),
            DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(2, 2, image::Luma([0x1234]))),
            DynamicImage::ImageLumaA16(image::ImageBuffer::from_pixel(
                2,
                2,
                image::LumaA([0x1234, 0xabcd]),
            )),
            DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
                2,
                2,
                image::Rgb([0x1234, 0x5678, 0x9abc]),
            )),
            DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
                2,
                2,
                image::Rgba([0x1234, 0x5678, 0x9abc, 0xdef0]),
            )),
        ];
        for image in modes {
            let (dir, native) = fixture(&[b"20 0 0 20 0 0 cm /Scan Do", b"/Scan Do"], false);
            let bits =
                if image.color().bits_per_pixel() / image.color().channel_count() as u16 == 16 {
                    16
                } else {
                    8
                };
            let out = dir.path().join("stage.pdf");
            let receipt = write_staged(
                &native,
                &[RasterReplacement {
                    page_index: 0,
                    image,
                }],
                &out,
            )
            .unwrap();
            assert!(!receipt.pages[0].reused);
            assert!(receipt.pages[1].reused);
            assert_eq!(receipt.pages[0].bits_per_component, Some(bits));
            assert!(receipt.pages[0].decoded_pixel_sha256.is_some());
            let after = Document::load(&out).unwrap();
            let ids = native.source_document().get_pages();
            for (id, obj) in &native.source_document().objects {
                if *id != ids[&1] {
                    assert_eq!(after.objects.get(id), Some(obj));
                }
            }
        }
    }
    #[test]
    fn noop_legacy_pdf_promotes_version_for_escaped_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("legacy.pdf");
        let mut bytes = b"%PDF-1.1\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Count 1 /Kids [3 0 R] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 20 20] >>",
        ]
        .iter()
        .enumerate()
        {
            offsets.push(bytes.len());
            writeln!(&mut bytes, "{} 0 obj\n{body}\nendobj", index + 1).unwrap();
        }
        let xref = bytes.len();
        writeln!(&mut bytes, "xref\n0 4\n0000000000 65535 f ").unwrap();
        for offset in offsets {
            writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
        }
        write!(
            &mut bytes,
            "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .unwrap();
        std::fs::write(&source, bytes).unwrap();
        let native = NativePdfExtractor::extract_path(&source).unwrap();
        let output = dir.path().join("stage.pdf");
        write_staged(&native, &[], &output).unwrap();
        assert_eq!(Document::load(output).unwrap().version, "1.2");
    }

    #[test]
    fn replacement_promotes_pdf_version_for_alpha_and_sixteen_bit() {
        for (image, expected) in [
            (DynamicImage::new_rgba8(2, 2), "1.4"),
            (DynamicImage::new_luma16(2, 2), "1.5"),
        ] {
            let (dir, native) = fixture(&[b"/Scan Do"], false);
            let mut source = native.source_document().clone();
            source.version = "1.2".into();
            let path = dir.path().join("older.pdf");
            save_graph(&source, &mut std::fs::File::create(&path).unwrap()).unwrap();
            let native = NativePdfExtractor::extract_path(&path).unwrap();
            let output = dir.path().join("stage.pdf");
            write_staged(
                &native,
                &[RasterReplacement {
                    page_index: 0,
                    image,
                }],
                &output,
            )
            .unwrap();
            assert_eq!(Document::load(&output).unwrap().version, expected);
        }
    }

    #[test]
    fn reject_dimensions_float_complex_singular_duplicate_and_signature() {
        for (content, image, duplicate) in [
            (b"/Scan Do".as_slice(), DynamicImage::new_luma8(3, 2), false),
            (b"/Scan Do", DynamicImage::new_rgb32f(2, 2), false),
            (b"/Scan Do /Scan Do", DynamicImage::new_luma8(2, 2), false),
            (
                b"0 0 0 0 0 0 cm /Scan Do",
                DynamicImage::new_luma8(2, 2),
                false,
            ),
            (b"/Scan Do", DynamicImage::new_luma8(2, 2), true),
        ] {
            let (dir, native) = fixture(&[content], false);
            let out = dir.path().join("stage.pdf");
            let mut r = vec![RasterReplacement {
                page_index: 0,
                image,
            }];
            if duplicate {
                r.push(RasterReplacement {
                    page_index: 0,
                    image: DynamicImage::new_luma8(2, 2),
                });
            }
            assert!(write_staged(&native, &r, &out).is_err());
            assert!(!out.exists());
        }
        let (dir, native) = fixture(&[b"/Scan Do"], true);
        assert!(write_staged(&native, &[], &dir.path().join("stage.pdf")).is_err());
    }
    #[test]
    fn never_clobber_existing_source_or_symlink() {
        let (dir, native) = fixture(&[b"/Scan Do"], false);
        let out = dir.path().join("stage.pdf");
        std::fs::write(&out, b"keep").unwrap();
        assert!(write_staged(&native, &[], &out).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"keep");
        assert!(write_staged(&native, &[], native.source_path()).is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("dangling.pdf");
            std::os::unix::fs::symlink(dir.path().join("absent"), &link).unwrap();
            assert!(write_staged(&native, &[], &link).is_err());
            assert!(std::fs::symlink_metadata(link).is_ok());
        }
    }
}
