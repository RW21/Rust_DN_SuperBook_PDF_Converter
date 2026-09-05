//! Synthetic-only end-to-end demonstration; never reads a user's scan corpus.
use lopdf::{dictionary, Document, Object, Stream};
use std::{error::Error, io::Write, path::PathBuf};
use superbook_pdf::preservation_bundle::{verify_bundle, write_preservation_bundle};
use superbook_pdf::preservation_writer::RasterReplacement;

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: preservation_demo NEW_DIRECTORY")?,
    );
    std::fs::create_dir(&root)?;
    let source = root.join("synthetic-source.pdf");
    let mut pdf = Document::with_version("1.7");
    let pages = pdf.new_object_id();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255])?;
    let image = pdf.add_object(Stream::new(
        dictionary! {
            "Type"=>"XObject", "Subtype"=>"Image", "Width"=>2, "Height"=>2,
            "ColorSpace"=>"DeviceRGB", "BitsPerComponent"=>8, "Filter"=>"FlateDecode",
        },
        z.finish()?,
    ));
    let content = pdf.add_object(Stream::new(
        dictionary! {},
        b"q 144 0 0 144 0 0 cm /Scan Do Q".to_vec(),
    ));
    let mut kids = Vec::new();
    for index in 0..3 {
        let mut page = dictionary! {"Type"=>"Page", "Parent"=>pages,
        "MediaBox"=>vec![0.into(),0.into(),144.into(),144.into()]};
        if index != 1 {
            page.set("Contents", content);
            page.set(
                "Resources",
                dictionary! {"XObject"=>dictionary! {"Scan"=>image}},
            );
        }
        kids.push(Object::Reference(pdf.add_object(page)));
    }
    pdf.objects.insert(
        pages,
        Object::Dictionary(dictionary! {"Type"=>"Pages", "Count"=>3, "Kids"=>kids}),
    );
    let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
    pdf.trailer.set("Root", catalog);
    pdf.save(&source)?;
    let replacement = image::DynamicImage::ImageRgb8(
        image::RgbImage::from_raw(2, 2, vec![255, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0])
            .ok_or("invalid demo samples")?,
    );
    let published = write_preservation_bundle(
        &source,
        &[RasterReplacement {
            page_index: 0,
            image: replacement,
        }],
        &root.join("bundle"),
    )?;
    let verified = verify_bundle(&published.directory)?;
    println!("Synthetic pages verified: {}", verified.pages.len());
    println!("PDF: {}", published.pdf_path.display());
    println!("Audit: {}", published.manifest_path.display());
    println!("SHA-256: {}", verified.header.pdf_sha256);
    Ok(())
}
