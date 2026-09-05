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

A 180-degree correction is an exact native sample permutation. Approved deskew uses the
existing Lanczos-3 expanded RGBA8 transform. Expanded output is centered on the original
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
