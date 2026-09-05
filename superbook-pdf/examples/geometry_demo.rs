//! End-to-end synthetic geometry qualification; never reads a scan corpus.
use image::{DynamicImage, GrayImage, Luma};
use lopdf::{dictionary, Document, Object, Stream};
use std::{error::Error, io::Write, path::PathBuf};
use superbook_pdf::geometry_pipeline::verify_geometry_bundle;
use superbook_pdf::{
    cli::GeometryAction,
    pipeline::{PdfPipeline, PipelineConfig},
};
fn text() -> DynamicImage {
    let mut image = GrayImage::from_pixel(600, 800, Luma([255]));
    for (line, y) in [
        45, 56, 67, 78, 89, 100, 111, 122, 133, 144, 190, 220, 250, 270, 650, 700,
    ]
    .into_iter()
    .enumerate()
    {
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
fn edge(angle: f64) -> DynamicImage {
    let mut image = GrayImage::from_pixel(320, 480, Luma([255]));
    for y in 0..480 {
        let x = (25.0 + angle.to_radians().tan() * f64::from(y))
            .round()
            .clamp(6.0, 60.0) as u32;
        for xx in x..320 {
            image.put_pixel(xx, y, Luma([32]));
        }
    }
    DynamicImage::ImageLuma8(image)
}
fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: geometry_demo NEW_DIRECTORY")?,
    );
    std::fs::create_dir(&root)?;
    let source = root.join("synthetic-geometry.pdf");
    let mut pdf = Document::with_version("1.7");
    let pages = pdf.new_object_id();
    let mut kids = Vec::new();
    for (image, placement) in [
        (Some(text()), "600 0 0 800 0 0 cm /Scan Do"),
        (Some(edge(-2.0)), "480 0 0 640 60 80 cm /Scan Do"),
        (Some(edge(2.0)), "480 0 0 640 60 80 cm /Scan Do"),
        (Some(edge(2.0)), "600 0 0 800 0 0 cm /Scan Do"),
        (None, ""),
    ] {
        let mut page = dictionary! {"Type"=>"Page","Parent"=>pages,"MediaBox"=>vec![0.into(),0.into(),600.into(),800.into()]};
        if let Some(image) = image {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(image.as_bytes())?;
            let id=pdf.add_object(Stream::new(dictionary!{"Type"=>"XObject","Subtype"=>"Image","Width"=>i64::from(image.width()),"Height"=>i64::from(image.height()),"BitsPerComponent"=>8,"ColorSpace"=>"DeviceGray","Filter"=>"FlateDecode"},z.finish()?));
            let content =
                pdf.add_object(Stream::new(dictionary! {}, placement.as_bytes().to_vec()));
            page.set(
                "Resources",
                dictionary! {"XObject"=>dictionary!{"Scan"=>id}},
            );
            page.set("Contents", content);
        }
        kids.push(Object::Reference(pdf.add_object(page)));
    }
    pdf.objects.insert(
        pages,
        Object::Dictionary(dictionary! {"Type"=>"Pages","Count"=>kids.len() as i64,"Kids"=>kids}),
    );
    let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages});
    pdf.trailer.set("Root", catalog);
    pdf.save(&source)?;
    let pipeline = PdfPipeline::new(PipelineConfig::geometry_only(
        GeometryAction::Apply,
        GeometryAction::Apply,
    ));
    let result = pipeline.process(&source, &root.join("output"))?;
    let bundle = result
        .output_path
        .parent()
        .ok_or("missing bundle directory")?;
    let manifest = verify_geometry_bundle(bundle)?;
    for page in &manifest.pages {
        println!(
            "page {}: rotation={:?}; deskew={:?}; review={}",
            page.page_index + 1,
            page.evidence.rotation.decision,
            page.evidence.deskew.decision,
            page.evidence.review_required
        );
    }
    println!("PDF: {}", result.output_path.display());
    println!(
        "Transforms: {}",
        result
            .transform_manifest_path
            .ok_or("missing manifest")?
            .display()
    );
    Ok(())
}
