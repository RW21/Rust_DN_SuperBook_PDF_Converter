# Geometry execution (Task 6B)

Geometry-only execution connects the previously validated rotation/deskew policies to
the verified native writer. The default remains report; per-stage off/report/apply
remain independent. No legacy renderer, super-resolution, crop/trim, resizing, JPEG
recompression, color correction, or OCR path is invoked.

Analysis uses one immutable source snapshot and exactly one indexed record per physical
page. Unsupported, ambiguous and blank pages remain present. Both stages off skips image
decoding. Eligible decoded pages are analyzed for rotation first. Deskew is measured once
on the actual image after an applied half-turn, or on the unchanged source if rotation
is report/rejected/off. A proposed rotation is never a hypothetical deskew input.
Automatic preparation currently requires /Rotate 0, a positive axis-aligned image
placement, and the complete original scan frame inside the visible page rectangle.
Rotated/sheared placements and even compensating noncanonical combinations remain
unchanged and review-required; raw-raster orientation is never assumed to be display
orientation. Expanded RGBA allocations are bounded before resampling. Retained prepared
rasters share a 512 MiB default budget (or PipelineConfig.max_memory_mb when set); this
is not a cap on parser/codec/transient working memory. Budget exhaustion aborts the new
bundle rather than risking unbounded retention across a whole book.

Native decoding additionally accepts one-bit `/DeviceGray` `/CCITTFaxDecode` images only
when strict explicit `/Columns` and `/Rows` match the image dimensions. `K = 0` Group 3
one-dimensional and `K < 0` Group 4 are supported by the exact-pinned decoder; mixed
Group 3 two-dimensional `K > 0`, damaged-row tolerance, unknown or malformed parameters,
EOL/byte-aligned-row variants, excessive dimensions, and other image semantics fail closed.
The dimension cap bounds decoder transition state independently of the sample budget.
Short output, overflow, and unexplained nonzero trailing whole bytes are rejected; bits
after the exact declared row count in the codec's final consumed byte are terminal padding.
Decoding expands bilevel samples to Gray8 only for
analysis. Unchanged/report output continues to reuse the original encoded CCITT stream
byte-for-byte.

A 180-degree correction is an exact native sample permutation. Automatic 180-degree
application to a one-bit source remains rejected and review-required until the writer can
publish a lossless bilevel replacement without changing bit depth; analysis/reporting is
still allowed. Approved deskew uses the existing Lanczos-3 expanded RGBA8 transform, so a
one-bit source may become RGBA8 only when the manifest records and independently verifies
an actually applied interpolated deskew. Expanded output is centered on the original
scan frame at the original physical pixel scale, without resizing or raster cropping.
The actual f32 PDF placement matrix is checked against MediaBox/CropBox intersection.
A conservative bound around every nonwhite/nontransparent pixel must lie fully inside
that visible rectangle, including pixel-cell edges. White padding alone may extend
outside the unchanged paper frame. If any visible content could clip, deskew is rejected
and flagged for review; any independently approved half-turn is retained. Other decode,
transform, verification or publication failures abort the new bundle.

The geometry manifest uses schema version 3 and a distinct geometry header/page record
format. Published v1 transform and v2 prepared-output readers remain unchanged. Each
geometry page records source identity/geometry, all discovered source/output images,
rotation and deskew evidence/decisions, actual analysis frame, deskew interpolation/fill/
canvas metadata, output placement, review notes, and a verified writer receipt. The
header records actual effective policy and source/output hashes and sizes. Readers
reject unsupported versions, unknown fields, unordered/missing/duplicate records,
contradictory applied flags, policies inconsistent with applied evidence, and mismatches
with final PDF geometry/streams/pixel hashes.

Computed corrections remain Proposed in preparation. After staged PDF graph/sample
verification, final records may be prepared as Applied only for matching non-reused
writer receipts. The PDF and final manifest are synced/read back in a private staging
directory and become visible together via Linux no-replace directory publication.
No returned or published Applied record can exist without its verified PDF. Failure
before rename leaves no published bundle; post-rename sync failure reports durability
uncertainty rather than success. Sources and prior generations are never overwritten.

The CLI returns paths inside `<input-stem>.geometry/` under its requested output root.
Existing bundles cause an error. max-pages truncation is rejected in geometry mode;
qualification samples must be separate input PDFs so manifest cardinality stays honest.
Non-Linux execution remains explicitly unsupported until equivalent atomic publication
is implemented. Real scan qualification follows synthetic/end-to-end verification.
