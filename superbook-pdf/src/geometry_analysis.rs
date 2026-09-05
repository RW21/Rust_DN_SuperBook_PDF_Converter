//! Native-raster geometry preparation; publication alone may mark actions Applied.

use crate::cli::GeometryAction;
use crate::image_extract::{NativePageKind, NativePdfDocument, NativeTransformDecodeCapability};
use crate::pipeline::{DeskewApplicationMetadata, PdfPipeline, PipelineError};
use crate::preservation_writer::{
    validate_expanded_placement, ExpandedRasterReplacement, PreservationWriterError,
    RasterReplacement,
};
use crate::transform_manifest::{DeskewTransform, RotationTransform, TransformDecision};
use crate::ImageProcDeskewer;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(crate) struct PreparedGeometry {
    pub same_size: Vec<RasterReplacement>,
    pub expanded: Vec<ExpandedRasterReplacement>,
    pub pages: Vec<GeometryEvidenceRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GeometryEvidenceRecord {
    pub page_index: usize,
    pub rotation: RotationTransform,
    pub deskew: DeskewTransform,
    pub deskew_application: Option<DeskewApplicationMetadata>,
    pub review_required: bool,
    pub notes: Vec<String>,
    pub deskew_input_after_rotation: bool,
    pub rotation_pixels_changed: bool,
    pub deskew_pixels_changed: bool,
    pub output_matrix: Option<[f64; 6]>,
}

fn skipped_rotation(
    action: GeometryAction,
    reason: &str,
    unsupported: bool,
) -> Result<RotationTransform, PipelineError> {
    let (decision, reason) = skipped_decision(action, reason, unsupported);
    Ok(RotationTransform::new(0, None, 0.0, decision, reason)?)
}
fn skipped_deskew(
    action: GeometryAction,
    reason: &str,
    unsupported: bool,
) -> Result<DeskewTransform, PipelineError> {
    let (decision, reason) = skipped_decision(action, reason, unsupported);
    Ok(DeskewTransform::new(0.0, 0.0, 0, decision, reason)?)
}
fn skipped_decision(
    action: GeometryAction,
    reason: &str,
    unsupported: bool,
) -> (TransformDecision, &str) {
    if action == GeometryAction::Off {
        (TransformDecision::Unchanged, "disabled")
    } else if unsupported {
        (TransformDecision::Rejected, "native_page_unsupported")
    } else {
        (TransformDecision::Unchanged, reason)
    }
}
fn image_error(error: image::ImageError) -> PipelineError {
    PipelineError::ImageProcessingFailed(error.to_string())
}

/// Prepare exact native PNG intermediates, never rendered PDF pages. Decisions
/// remain proposals until the coordinator verifies and publishes the final PDF.
pub(crate) fn prepare(
    native: &NativePdfDocument,
    pipeline: &PdfPipeline,
    work: &Path,
) -> Result<PreparedGeometry, PipelineError> {
    let config = pipeline.config();
    // This bounds retained prepared rasters, not the parser/codec working set.
    let raster_budget = (if config.max_memory_mb == 0 {
        512u64
    } else {
        u64::try_from(config.max_memory_mb).map_err(|_| {
            PipelineError::ImageProcessingFailed("invalid prepared raster budget".into())
        })?
    })
    .checked_mul(1024 * 1024)
    .ok_or_else(|| {
        PipelineError::ImageProcessingFailed("prepared raster budget overflow".into())
    })?;
    let mut retained_bytes = 0u64;
    let mut account = |image: &image::DynamicImage| -> Result<(), PipelineError> {
        retained_bytes = retained_bytes
            .checked_add(image.as_bytes().len() as u64)
            .ok_or_else(|| {
                PipelineError::ImageProcessingFailed("prepared raster size overflow".into())
            })?;
        if retained_bytes > raster_budget {
            return Err(PipelineError::ImageProcessingFailed("prepared raster budget exceeded; process separate source segments or raise PipelineConfig.max_memory_mb".into()));
        }
        Ok(())
    };
    let mut prepared = PreparedGeometry {
        same_size: Vec::new(),
        expanded: Vec::new(),
        pages: Vec::with_capacity(native.pages().len()),
    };
    for page in native.pages() {
        let index = page.physical_page.page_index;
        let invocation = match page.image_invocations.as_slice() {
            [one] => Some(one),
            _ => None,
        };
        let matrix = invocation.map(|i| i.binding.placement_matrix.coordinates());
        let matrix_supported = matrix.is_some_and(|[a, b, c, d, e, f]| {
            let determinant = a * d - b * c;
            [a, b, c, d, e, f, determinant]
                .iter()
                .all(|n| n.is_finite())
                && determinant > 0.0
        });
        let frame_supported = matrix
            .zip(page.physical_page.media_box.as_ref())
            .is_some_and(|(m, media)| {
                let mb = media.value.coordinates();
                let cb = page
                    .physical_page
                    .crop_box
                    .as_ref()
                    .map(|b| b.value.coordinates())
                    .unwrap_or(mb);
                let visible = [
                    mb[0].max(cb[0]),
                    mb[1].max(cb[1]),
                    mb[2].min(cb[2]),
                    mb[3].min(cb[3]),
                ];
                [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]
                    .iter()
                    .all(|[x, y]| {
                        let px = m[0] * x + m[2] * y + m[4];
                        let py = m[1] * x + m[3] * y + m[5];
                        px >= visible[0] && px <= visible[2] && py >= visible[1] && py <= visible[3]
                    })
            });
        let display_supported = page.physical_page.normalized_rotation() == Some(0)
            && matrix.is_some_and(|[a, b, c, d, _, _]| a > 0.0 && d > 0.0 && b == 0.0 && c == 0.0);
        let blank = page.kind == NativePageKind::Blank;
        let unsupported = page.review_required
            || !page.physical_page.issues.is_empty()
            || (!blank
                && (page.kind != NativePageKind::SingleImage
                    || !matrix_supported
                    || !frame_supported
                    || !display_supported
                    || invocation.is_none_or(|i| {
                        !i.binding.is_direct
                            || i.binding.resource_path.len() != 1
                            || i.metadata.transform_decode
                                == NativeTransformDecodeCapability::Unsupported
                    })));
        let mut record = GeometryEvidenceRecord {
            page_index: index,
            rotation: skipped_rotation(
                config.rotation_action,
                if blank { "blank_page" } else { "disabled" },
                unsupported,
            )?,
            deskew: skipped_deskew(
                config.deskew_action,
                if blank { "blank_page" } else { "disabled" },
                unsupported,
            )?,
            deskew_application: None,
            review_required: unsupported,
            notes: if unsupported {
                vec![
                    "native_page_unsupported: source page retained without geometry changes".into(),
                ]
            } else {
                Vec::new()
            },
            deskew_input_after_rotation: false,
            rotation_pixels_changed: false,
            deskew_pixels_changed: false,
            output_matrix: matrix,
        };
        record.notes.extend(
            page.issues
                .iter()
                .map(|issue| format!("{}: {}", issue.code.as_str(), issue.detail)),
        );
        if !blank && !display_supported {
            record.notes.push("noncanonical_display_geometry: page rotation, image rotation/shear, or compensating transforms require manual review".into());
        }
        if !blank && !frame_supported {
            record.notes.push("source_frame_crosses_page_box: automatic geometry requires the original scan frame inside the visible page".into());
        }
        if blank
            || unsupported
            || (config.rotation_action == GeometryAction::Off
                && config.deskew_action == GeometryAction::Off)
        {
            prepared.pages.push(record);
            continue;
        }
        let invocation = invocation.expect("eligible single-image page");
        let mut image = native.decode_image(&invocation.metadata, 256 * 1024 * 1024)?;
        // Native decode currently guarantees Gray8/RGB8; no color conversion or
        // thresholding is involved in the all-white fast path.
        if image.as_bytes().iter().all(|sample| *sample == 255) {
            record.rotation = skipped_rotation(config.rotation_action, "blank_page", false)?;
            record.deskew = skipped_deskew(config.deskew_action, "blank_page", false)?;
            record
                .notes
                .push("blank_page: decoded native samples are all white".into());
            prepared.pages.push(record);
            continue;
        }
        std::fs::create_dir_all(work)?;
        let mut input = work.join(format!("page-{index}-native.png"));
        image
            .save_with_format(&input, image::ImageFormat::Png)
            .map_err(image_error)?;
        if let Some(outcome) = pipeline.analyze_rotation(index, &input)? {
            record.rotation = outcome.transform;
            record.review_required |= outcome.review_required;
            if outcome.should_apply {
                image = ImageProcDeskewer::rotate_180_exact(&image);
                input = work.join(format!("page-{index}-rotated.png"));
                image
                    .save_with_format(&input, image::ImageFormat::Png)
                    .map_err(image_error)?;
                record.rotation.decision = TransformDecision::Proposed;
                record.rotation_pixels_changed = true;
            }
        }
        // This is the actual frame, not a hypothetical report-only rotation.
        record.deskew_input_after_rotation =
            record.rotation_pixels_changed && config.deskew_action != GeometryAction::Off;
        let outcome = pipeline.analyze_deskew(index, &input, false)?;
        record.deskew = outcome.transform.clone();
        record.review_required |= outcome.review_required;
        let mut expanded = false;
        if outcome.should_apply {
            // Bound the actual expanded RGBA allocation before the resampler runs.
            let radians = outcome.transform.proposed_degrees.get().to_radians();
            let width = (f64::from(image.width()) * radians.cos().abs()
                + f64::from(image.height()) * radians.sin().abs())
            .ceil();
            let height = (f64::from(image.width()) * radians.sin().abs()
                + f64::from(image.height()) * radians.cos().abs())
            .ceil();
            if !width.is_finite()
                || !height.is_finite()
                || width * height * 4.0 > 256.0 * 1024.0 * 1024.0
            {
                return Err(PipelineError::ImageProcessingFailed(
                    "expanded deskew exceeds the RGBA sample budget".into(),
                ));
            }
            let output = work.join(format!("page-{index}-deskew.png"));
            let applied = pipeline.apply_deskew_outcome(index, &input, &output, outcome)?;
            let transformed = image::open(&output).map_err(image_error)?;
            match validate_expanded_placement(native, index, &transformed) {
                Ok(matrix) => {
                    record.deskew = applied.transform;
                    record.deskew.decision = TransformDecision::Proposed;
                    record.deskew_application = applied.application;
                    record.deskew_pixels_changed = true;
                    record.output_matrix = Some(matrix);
                    account(&transformed)?;
                    prepared.expanded.push(ExpandedRasterReplacement {
                        page_index: index,
                        image: transformed,
                    });
                    expanded = true;
                }
                Err(PreservationWriterError::ExpandedCanvasWouldClip) => {
                    record.deskew.decision = TransformDecision::Rejected;
                    record.deskew.reason = "expanded_canvas_would_clip".into();
                    record.review_required = true;
                    record.notes.push(if record.rotation_pixels_changed {
                        "expanded_canvas_would_clip: deskew discarded; approved rotation retained"
                            .into()
                    } else {
                        "expanded_canvas_would_clip: deskew discarded; source raster retained"
                            .into()
                    });
                }
                Err(PreservationWriterError::Unsupported(reason)) => {
                    record.deskew.decision = TransformDecision::Rejected;
                    record.deskew.reason = "expanded_placement_unsupported".into();
                    record.review_required = true;
                    record.notes.push(reason);
                }
                Err(error) => return Err(PipelineError::PdfGenerationFailed(error.to_string())),
            }
        }
        if record.rotation_pixels_changed && !expanded {
            account(&image)?;
            prepared.same_size.push(RasterReplacement {
                page_index: index,
                image,
            });
        }
        prepared.pages.push(record);
    }
    Ok(prepared)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{image_extract::NativePdfExtractor, pipeline::PipelineConfig};
    use image::{DynamicImage, GrayImage, Luma};
    use lopdf::{dictionary, Document, Object, Stream};
    use std::io::Write;

    pub(crate) fn fixture(
        contents: &[&[u8]],
        image: &DynamicImage,
        corrupt: bool,
    ) -> (tempfile::TempDir, NativePdfDocument) {
        let dir = tempfile::tempdir().unwrap();
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(image.as_bytes()).unwrap();
        let bytes = if corrupt {
            vec![0, 1, 2]
        } else {
            z.finish().unwrap()
        };
        let img = doc.add_object(Stream::new(dictionary! {"Type"=>"XObject", "Subtype"=>"Image", "Width"=>i64::from(image.width()), "Height"=>i64::from(image.height()), "BitsPerComponent"=>8, "ColorSpace"=>"DeviceGray", "Filter"=>"FlateDecode"}, bytes));
        let resources = doc.add_object(dictionary! {"XObject"=>dictionary! {"Scan"=>img}});
        let mut kids = Vec::new();
        for bytes in contents {
            let content = doc.add_object(Stream::new(dictionary! {}, bytes.to_vec()));
            kids.push(Object::Reference(doc.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages, "Contents"=>content, "MediaBox"=>vec![0.into(),0.into(),600.into(),800.into()]})));
        }
        doc.objects.insert(pages, Object::Dictionary(dictionary! {"Type"=>"Pages", "Count"=>kids.len() as i64, "Kids"=>kids, "Resources"=>resources}));
        let root = doc.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
        doc.trailer.set("Root", root);
        let path = dir.path().join("source.pdf");
        doc.save(&path).unwrap();
        let native = NativePdfExtractor::extract_path(&path).unwrap();
        (dir, native)
    }
    fn pipeline(rotation: GeometryAction, deskew: GeometryAction) -> PdfPipeline {
        PdfPipeline::new(PipelineConfig {
            geometry_only: true,
            rotation_action: rotation,
            deskew_action: deskew,
            rotation_action_configured: true,
            deskew_action_configured: true,
            max_pages: Some(1),
            ..PipelineConfig::default()
        })
    }
    const SCAN: &[u8] = b"600 0 0 800 0 0 cm /Scan Do";

    #[test]
    fn off_skips_corrupt_decode_and_preserves_all_physical_indexes() {
        let image = DynamicImage::ImageLuma8(GrayImage::from_pixel(2, 2, Luma([0])));
        let (dir, native) = fixture(&[SCAN, SCAN, SCAN], &image, true);
        let work = dir.path().join("work");
        let result = prepare(
            &native,
            &pipeline(GeometryAction::Off, GeometryAction::Off),
            &work,
        )
        .unwrap();
        assert_eq!(
            result
                .pages
                .iter()
                .map(|p| p.page_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(result.same_size.is_empty() && result.expanded.is_empty());
        assert!(!work.exists());
        for page in result.pages {
            assert_eq!(page.rotation.reason, "disabled");
            assert_eq!(page.deskew.reason, "disabled");
            assert!(!page.rotation_pixels_changed && !page.deskew_pixels_changed);
            assert_eq!(page.output_matrix, Some([600., 0., 0., 800., 0., 0.]));
        }
    }
    #[test]
    fn blanks_and_unsupported_are_complete_and_unchanged() {
        let image = DynamicImage::ImageLuma8(GrayImage::from_pixel(2, 2, Luma([255])));
        let (dir, native) = fixture(
            &[
                b"",
                SCAN,
                b"0 0 10 10 re f",
                b"0 0 0 800 0 0 cm /Scan Do",
                b"-600 0 0 800 600 0 cm /Scan Do",
            ],
            &image,
            false,
        );
        let result = prepare(
            &native,
            &pipeline(GeometryAction::Apply, GeometryAction::Apply),
            &dir.path().join("work"),
        )
        .unwrap();
        assert_eq!(result.pages.len(), 5);
        assert!(result.same_size.is_empty() && result.expanded.is_empty());
        for page in &result.pages[..2] {
            assert_eq!(page.rotation.reason, "blank_page");
            assert_eq!(page.deskew.reason, "blank_page");
            assert!(!page.rotation_pixels_changed && !page.deskew_pixels_changed);
        }
        for page in &result.pages[2..] {
            assert!(page.review_required);
            assert_eq!(page.rotation.decision, TransformDecision::Rejected);
            assert_eq!(page.deskew.reason, "native_page_unsupported");
        }
    }
    pub(crate) fn upside_down_text() -> DynamicImage {
        // Same synthetic glyph fixture as the conservative rotation analyzer.
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
        DynamicImage::ImageLuma8(image).rotate180()
    }
    #[test]
    fn report_keeps_bytes_and_uses_original_frame_for_deskew() {
        let image = upside_down_text();
        let (dir, native) = fixture(&[SCAN], &image, false);
        let before = std::fs::read(native.source_path()).unwrap();
        let work = dir.path().join("work");
        let pipeline = pipeline(GeometryAction::Report, GeometryAction::Report);
        let result = prepare(&native, &pipeline, &work).unwrap();
        assert!(result.same_size.is_empty() && result.expanded.is_empty());
        let page = &result.pages[0];
        assert_eq!(page.rotation.proposed_degrees.get(), 180);
        assert_eq!(page.rotation.decision, TransformDecision::Proposed);
        assert!(!page.rotation_pixels_changed && !page.deskew_pixels_changed);
        assert!(!page.deskew_input_after_rotation);
        assert!(page.deskew_application.is_none());
        assert_eq!(
            image::open(work.join("page-0-native.png"))
                .unwrap()
                .as_bytes(),
            image.as_bytes()
        );
        let expected = pipeline
            .analyze_deskew(0, &work.join("page-0-native.png"), false)
            .unwrap();
        assert_eq!(page.deskew, expected.transform);
        assert_eq!(std::fs::read(native.source_path()).unwrap(), before);
        assert!(!work.join("page-0-rotated.png").exists());
        let encoded = serde_json::to_value(page).unwrap();
        assert_eq!(
            serde_json::from_value::<GeometryEvidenceRecord>(encoded.clone()).unwrap(),
            *page
        );
        let mut extra = encoded;
        extra
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), true.into());
        assert!(serde_json::from_value::<GeometryEvidenceRecord>(extra).is_err());
    }
    #[test]
    fn approved_rotation_is_exact_and_proposed_until_publication() {
        let image = upside_down_text();
        let (dir, native) = fixture(&[SCAN], &image, false);
        let work = dir.path().join("work");
        let pipeline = pipeline(GeometryAction::Apply, GeometryAction::Report);
        let result = prepare(&native, &pipeline, &work).unwrap();
        assert_eq!(result.same_size.len(), 1);
        assert!(result.expanded.is_empty());
        let page = &result.pages[0];
        assert_eq!(page.rotation.decision, TransformDecision::Proposed);
        assert!(page.rotation_pixels_changed && page.deskew_input_after_rotation);
        assert!(!page.deskew_pixels_changed);
        assert_eq!(
            result.same_size[0].image.as_bytes(),
            image.rotate180().as_bytes()
        );
        let expected = pipeline
            .analyze_deskew(0, &work.join("page-0-rotated.png"), false)
            .unwrap();
        assert_eq!(page.deskew, expected.transform);
    }
    pub(crate) fn skewed_edge(angle: f64) -> DynamicImage {
        let mut image = GrayImage::from_pixel(320, 480, Luma([255]));
        for y in 0..480 {
            let edge = (25.0 + angle.to_radians().tan() * f64::from(y))
                .round()
                .clamp(6.0, 60.0) as u32;
            for x in edge..320 {
                image.put_pixel(x, y, Luma([32]));
            }
        }
        DynamicImage::ImageLuma8(image)
    }
    #[test]
    fn both_signed_deskews_expand_at_native_scale_and_remain_proposed() {
        for angle in [-2.0, 2.0] {
            let image = skewed_edge(angle);
            let (dir, native) = fixture(&[b"480 0 0 640 60 80 cm /Scan Do"], &image, false);
            let result = prepare(
                &native,
                &pipeline(GeometryAction::Off, GeometryAction::Apply),
                &dir.path().join("work"),
            )
            .unwrap();
            assert!(result.same_size.is_empty());
            assert_eq!(result.expanded.len(), 1, "{:?}", result.pages);
            let page = &result.pages[0];
            assert_eq!(page.deskew.decision, TransformDecision::Proposed);
            assert!(page.deskew_pixels_changed);
            assert!(!page.rotation_pixels_changed || page.deskew_input_after_rotation);
            assert_eq!(
                page.deskew_application,
                Some(DeskewApplicationMetadata::current())
            );
            assert!((page.deskew.proposed_degrees.get() - angle).abs() < 0.25);
            let expanded = &result.expanded[0].image;
            assert!(expanded.width() > image.width() && expanded.height() > image.height());
            assert_eq!(
                page.output_matrix,
                Some(validate_expanded_placement(&native, 0, expanded).unwrap())
            );
        }
    }
    #[test]
    fn clipping_rejects_deskew_without_claiming_pixels_or_application() {
        for angle in [-2.0, 2.0] {
            let image = skewed_edge(angle);
            let (dir, native) = fixture(&[SCAN], &image, false);
            let result = prepare(
                &native,
                &pipeline(GeometryAction::Off, GeometryAction::Apply),
                &dir.path().join("work"),
            )
            .unwrap();
            assert!(result.same_size.is_empty() && result.expanded.is_empty());
            let page = &result.pages[0];
            assert_eq!(page.deskew.decision, TransformDecision::Rejected);
            assert_eq!(page.deskew.reason, "expanded_canvas_would_clip");
            assert!(!page.deskew_pixels_changed && !page.rotation_pixels_changed);
            assert!(page.deskew_application.is_none());
            assert!(page.review_required);
            assert_eq!(page.output_matrix, Some([600., 0., 0., 800., 0., 0.]));
        }
    }
    #[test]
    fn intermediate_write_errors_abort() {
        let image = upside_down_text();
        let (dir, native) = fixture(&[SCAN], &image, false);
        let work = dir.path().join("not-a-directory");
        std::fs::write(&work, b"blocked").unwrap();
        assert!(prepare(
            &native,
            &pipeline(GeometryAction::Report, GeometryAction::Off),
            &work
        )
        .is_err());
    }
    #[test]
    fn prepared_raster_budget_is_shared_across_pages() {
        let image = upside_down_text();
        let (dir, native) = fixture(&[SCAN, SCAN, SCAN], &image, false);
        let mut config = PipelineConfig::geometry_only(GeometryAction::Apply, GeometryAction::Off);
        config.max_memory_mb = 1;
        let result = prepare(&native, &PdfPipeline::new(config), &dir.path().join("work"));
        assert!(
            matches!(result,Err(PipelineError::ImageProcessingFailed(ref message)) if message.contains("prepared raster budget exceeded"))
        );
    }

    #[test]
    fn real_decode_errors_abort_instead_of_becoming_review() {
        let image = DynamicImage::ImageLuma8(GrayImage::from_pixel(2, 2, Luma([0])));
        let (dir, native) = fixture(&[SCAN], &image, true);
        assert!(prepare(
            &native,
            &pipeline(GeometryAction::Report, GeometryAction::Off),
            &dir.path().join("work")
        )
        .is_err());
    }
}
