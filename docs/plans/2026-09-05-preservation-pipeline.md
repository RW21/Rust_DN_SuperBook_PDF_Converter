# Preservation Pipeline Implementation Plan

> **For Hermes:** Implement task-by-task with tests first. Do not process or overwrite source scans while developing.

**Goal:** Add a conservative geometry-only and YomiToku OCR workflow that preserves the original scan data wherever possible, records every proposed/applied transform, and creates searchable derivatives without RealESRGAN or JPEG recompression.

**Architecture:** Keep immutable source PDFs as archival masters. A geometry stage analyzes native embedded page rasters, applies only approved high-confidence rotation/deskew operations, and writes a cleaned derivative plus a JSONL transform manifest. A long-lived YomiToku worker then analyzes cleaned pages on a CUDA-capable worker and writes auditable JSONL; a final Rust stage appends an invisible text layer without rebuilding visible page images.

**Tech Stack:** Rust, `lopdf`, existing `image`/deskew modules, Serde JSONL, Python managed with `uv`, YomiToku `v0.14.0`, and CUDA PyTorch.

**Pinned baseline:** upstream commit `f6032ad602013d7678a2b6df19b0339d44f90c0f`. Review or cherry-pick useful parts of `upstream/fix/ocr-mps-and-analyzer-cache`, but do not rely on its process-local cache: the current Rust integration starts a new Python process per page, so the analyzer still reloads.

---

## Non-negotiable invariants

1. Source segments and archival masters are never modified in place.
2. RealESRGAN, deblur, color correction, shadow removal, marker removal, margin trim, normalization, group crop, and output resizing are all disabled in geometry-only mode.
3. The default geometry-only action is report-only. Applying transforms requires an explicit flag.
4. Deskew and 180-degree rotation are separately configurable.
5. Low-confidence or structurally suspicious pages remain unchanged and are marked for review.
6. Every page receives one manifest record, including unchanged and failed pages.
7. Native page dimensions and PDF page boxes are preserved.
8. Unchanged visible page image streams are reused byte-for-byte when practical.
9. Changed pages are encoded losslessly; never silently convert bilevel or grayscale pages to RGB JPEG.
10. OCR runs after geometry correction so OCR coordinates match final visible pages.
11. YomiToku structured output is retained independently of the PDF.
12. Adding the text layer must not alter hashes of existing visible image streams.

## Current defects this plan addresses

- `PipelineConfig.deskew` currently controls both 180-degree rotation and skew correction.
- Rotation detection returns only a boolean and applies immediately.
- The normal CLI defaults still trim margins, remove shadows, upscale, and resize to `3508` pixels unless several unrelated options are overridden.
- `PrintPdfWriter::add_image_to_layer` converts every non-JPEG image to 8-bit RGB JPEG and ignores `PdfWriterOptions.compression`.
- `PageSizeMode::Original` exists but is not selected by the pipeline.
- The direct extractor handles DCT JPEG well but does not robustly preserve bilevel CCITT pages or pages without a directly reachable image XObject.
- YomiToku is initialized inside `process_image`, while Rust invokes the bridge per page; the analyzer cache in the upstream feature branch therefore cannot persist across a book.
- The built-in OCR/PDF path conflates recognition with PDF recreation.

---

### Task 1: Add a geometry-only CLI contract

**Objective:** Make safe behavior explicit and testable instead of requiring a fragile combination of existing switches.

**Files:**
- Modify: `superbook-pdf/src/cli.rs`
- Modify: `superbook-pdf/src/pipeline.rs`
- Modify: `superbook-pdf/src/config.rs`
- Test: `superbook-pdf/tests/cli_integration.rs`
- Create: `superbook-pdf/specs/preservation_pipeline.spec.md`

**Steps:**

1. Add failing CLI tests for `--geometry-only`, `--geometry-action report|apply`, `--rotation-action off|report|apply`, and `--deskew-action off|report|apply`.
2. Add typed Clap enums rather than boolean combinations.
3. In `PipelineConfig`, represent rotation and deskew independently.
4. Make `--geometry-only` force all unrelated mutating stages off and set output resizing to disabled.
5. Reject incompatible explicit options such as `--geometry-only --upscale true` with a clear error rather than silently choosing one.
6. Keep existing CLI behavior unchanged when `--geometry-only` is absent.
7. Run `cargo test --features web cli` and the full CLI integration test.

**Acceptance:** `--geometry-only` dry-run prints exactly the extraction, rotation-analysis, deskew-analysis, manifest, and output stages; no enhancement or resizing stage appears.

---

### Task 2: Define the transform manifest

**Objective:** Record evidence and decisions for every physical page before altering pixels.

**Files:**
- Create: `superbook-pdf/src/transform_manifest.rs`
- Modify: `superbook-pdf/src/lib.rs`
- Modify: `superbook-pdf/src/pipeline.rs`
- Test: module tests in `superbook-pdf/src/transform_manifest.rs`

**Schema per JSONL record:**

```json
{
  "schema_version": 1,
  "page_index": 0,
  "source_page_number": 1,
  "source_image": {"width": 2500, "height": 4200, "color_space": "gray", "bits_per_component": 1, "filter": "CCITTFaxDecode"},
  "blank": false,
  "rotation": {"proposed_degrees": 0, "confidence": 0.0, "decision": "unchanged", "reason": "disabled"},
  "deskew": {"proposed_degrees": 0.0, "confidence": 0.0, "feature_count": 0, "decision": "unchanged", "reason": "below_threshold"},
  "output": {"pixel_changed": false, "review_required": false},
  "errors": []
}
```

**Steps:**

1. Add Serde round-trip tests and reject non-finite angles/confidences.
2. Add deterministic one-record-per-page JSONL writing through a temporary file followed by atomic rename.
3. Preserve page order even when detection runs in parallel.
4. Include tool version, source PDF identity, and pinned schema version in a separate manifest header record or companion metadata file.
5. Add tests for blank, low-confidence, corrected, failed, and untouched pages.

**Acceptance:** A ten-page input always emits ten page records in physical page order, including on worker failure.

---

### Task 3: Separate rotation analysis from application

**Objective:** Stop the top-versus-bottom ink heuristic from silently flipping unusual pages.

**Files:**
- Modify: `superbook-pdf/src/deskew/algorithm.rs`
- Modify: `superbook-pdf/src/deskew/types.rs`
- Modify: `superbook-pdf/src/pipeline.rs`
- Add fixtures/tests under: `superbook-pdf/tests/fixtures/` and deskew module tests

**Steps:**

1. Replace the boolean-only rotation result at the pipeline boundary with evidence: score, confidence, reason, and proposed angle.
2. Add tests for normal text, upside-down text, blank pages, illustrations, sparse chapter endings, and covers.
3. In report mode, copy/reuse the input and only populate the manifest.
4. In apply mode, rotate only when confidence clears a configurable threshold and no ambiguity guard triggers.
5. Mark ambiguous pages for human review.
6. Preserve lossless format and dimensions for exact 180-degree rotation.

**Acceptance:** Sparse and illustrated fixtures are not auto-flipped; known upside-down fixtures are proposed correctly and change only in apply mode.

---

### Task 4: Separate deskew analysis from application

**Objective:** Apply conservative skew correction while retaining the measured angle and confidence.

**Files:**
- Modify: `superbook-pdf/src/deskew/algorithm.rs`
- Modify: `superbook-pdf/src/deskew/types.rs`
- Modify: `superbook-pdf/src/pipeline.rs`
- Test: existing skew fixtures plus new bilevel/vertical-Japanese fixtures

**Steps:**

1. Test exact angle, confidence, feature count, threshold behavior, and blank-page behavior.
2. Preserve `SkewDetection` in the manifest rather than discarding it after correction.
3. Make maximum angle, minimum confidence, minimum feature count, and no-op threshold explicit CLI/config values.
4. Never apply a correction outside the configured maximum or below confidence/evidence thresholds.
5. Use report mode first on representative scans and choose thresholds from observed results, not fixtures alone.
6. Record interpolation, canvas, fill-color, and post-transform pixel mode for applied pages.

**Acceptance:** Existing ±skew fixtures are corrected within the test tolerance; angles below the no-op threshold and low-confidence pages are byte-preserved or object-reused.

---

### Task 5: Preserve native extraction and page identity

**Objective:** Map exactly one output record to each physical PDF page and avoid accidental rasterization/downsampling.

**Files:**
- Modify: `superbook-pdf/src/image_extract.rs`
- Modify: `superbook-pdf/src/pdf_reader.rs`
- Modify: `superbook-pdf/src/pipeline.rs`
- Add tests/fixtures for DCT, Flate grayscale, and CCITT bilevel pages

**Steps:**

1. Add tests proving page-tree order and one-to-one page mapping, including blank pages and Form XObjects.
2. Expose source image metadata: object ID, dimensions, color space, bits per component, filter, decode parameters, and page boxes.
3. Directly reuse supported native image streams for unchanged pages.
4. Decode unsupported streams losslessly only when geometry must change; never fall back silently to a lower-DPI renderer.
5. Treat multi-image or non-image pages as review-required unless an explicit rasterization mode is selected.
6. Remove DPI as a native-extraction control; use it only when an explicit render fallback is requested.

**Acceptance:** Representative source scans retain native pixel dimensions; all pages stay in source page-tree order; no physical page disappears because it lacked a simple XObject.

---

### Task 6: Replace the destructive PDF writer path

**Objective:** Create cleaned derivatives without JPEG-converting all processed pages.

**Files:**
- Modify or replace preservation path in: `superbook-pdf/src/pdf_writer.rs`
- Modify: `superbook-pdf/src/pipeline.rs`
- Add integration tests using `lopdf` object inspection

**Steps:**

1. Write failing tests showing that `ImageCompression::Flate`, `None`, and `PageSizeMode::Original` currently do not control the emitted image objects correctly.
2. Add a preservation writer separate from compact/JPEG output rather than changing legacy defaults unexpectedly.
3. Reuse original page/image objects for unchanged pages.
4. Encode changed grayscale/RGB pages losslessly with correct color space and bit depth; use a documented lossless bilevel strategy when output remains bilevel.
5. Preserve each page's MediaBox/CropBox and orientation.
6. Add source-versus-output object tests for image filter, dimensions, bit depth, color space, page count, and page order.

**Acceptance:** Unchanged page image-stream hashes match the cleaned PDF; changed pages are lossless and native-resolution; no interior page becomes RGB JPEG.

---

### Task 7: Add a long-lived YomiToku book worker

**Objective:** Load `DocumentAnalyzer` once per process/book and retain structured OCR data.

**Files:**
- Modify: `superbook-pdf/ai_bridge/yomitoku_bridge.py`
- Modify: `superbook-pdf/src/yomitoku.rs`
- Modify: `superbook-pdf/src/pipeline.rs`
- Add Python tests under: `superbook-pdf/ai_bridge/tests/`
- Add Rust protocol tests near: `superbook-pdf/src/yomitoku.rs`

**Steps:**

1. Add a JSON-lines stdin/stdout worker protocol with explicit startup-ready, page-result, page-error, and shutdown messages.
2. Initialize one `DocumentAnalyzer(device="cuda:0")` before the page loop.
3. Return image width/height, paragraphs, words, boxes, direction, role, order, confidence when available, model/version metadata, and timing.
4. Write `*.yomitoku.jsonl` atomically as pages complete while preserving deterministic page ordering at finalization.
5. Make worker restart/retry bounded and preserve partial diagnostic output without claiming the book is complete.
6. Add a mock analyzer test proving one initialization for multiple pages.
7. Pin YomiToku and CUDA/PyTorch versions in `uv.lock`; do not use unpinned `pip install` instructions.

**Acceptance:** A multi-page test invokes analyzer construction once, emits one auditable result per page, and clearly marks failed pages.

---

### Task 8: Add the invisible searchable text layer

**Objective:** Add searchable Japanese text while leaving visible page objects untouched.

**Files:**
- Modify or create a preservation overlay module beside: `superbook-pdf/src/pdf_writer.rs`
- Modify: `superbook-pdf/src/yomitoku.rs`
- Add integration fixtures/tests for horizontal, vertical, and mixed Japanese

**Steps:**

1. Define one tested conversion from YomiToku top-left pixel coordinates to PDF bottom-left points using each page's actual dimensions and boxes.
2. Embed a pinned redistributable Japanese font subset with a valid Unicode map.
3. Append invisible text rendering operations to existing pages rather than recreating image pages.
4. Preserve reading order from YomiToku output and record any fallback ordering.
5. Test text extraction, search hits, Unicode glyphs, vertical/mixed positioning, and image-stream hash stability.

**Acceptance:** Japanese search/extraction works on sample pages, all visible image stream hashes remain unchanged, and OCR boxes align at high zoom.

---

### Task 9: Build a reproducible GPU OCR environment

**Objective:** Run GPU OCR without disturbing other GPU workloads except during an explicitly approved processing window.

**Target:** A CUDA-capable Linux environment with enough VRAM for the selected YomiToku model.

**Prerequisites:** Docker or a user-local `uv` environment, a compatible CUDA runtime, and sufficient free VRAM. Competing GPU workloads must not be stopped automatically.

**Steps:**

1. Add a pinned container or user-local `uv` environment; do not install Python or Node packages globally.
2. Verify CUDA from inside the exact OCR runtime before installing models.
3. Add an explicit operator procedure to pause competing GPU workloads, verify VRAM is released, run OCR, then restore and health-check those workloads.
4. Do not automate stopping production inference without per-run approval.
5. Transfer only a 5–10-page test corpus first.
6. Use fast local storage while processing, then copy verified artifacts back to archival storage.
7. Record versions, model identifiers, image digest/lockfile, and GPU result in the run manifest.

**Acceptance:** The representative sample completes on CUDA, reports the expected GPU, restores competing services successfully afterward, and leaves no source files modified.

---

### Task 10: Representative-book qualification

**Objective:** Prove the complete workflow before any 1,320-page batch.

**Sample:** 5–10 pages covering an RGB cover, clean bilevel vertical text, horizontal title/colophon, visible skew, illustration, sparse page, and blank page.

**Checks:**

1. Programmatically verify page count and order.
2. Compare source, cleaned, and searchable PDFs at first/middle/last pages.
3. Inspect image dimensions, filters, bit depth, color spaces, page boxes, and image-stream hashes.
4. Review every proposed transform in the JSONL manifest.
5. Confirm no unapproved rotation/crop/resize/color operation occurred.
6. Search/extract known Japanese phrases and verify vertical reading order.
7. Overlay OCR bounding boxes in a temporary debug derivative and inspect alignment.
8. Compare file sizes and explain any material growth.
9. Preserve the qualification report and exact commands.

**Promotion gate:** Do not process all four books until the user approves the sample's transform review and visible/searchable results.

---

## Intended outputs per book

- `Title.master.pdf` — losslessly concatenated archival master; no pixel changes.
- `Title.transforms.jsonl` — geometry evidence and applied/rejected decisions.
- `Title.cleaned.pdf` — conservative geometry derivative.
- `Title.yomitoku.jsonl` — structured OCR/layout output.
- `Title.searchable.pdf` — cleaned PDF plus invisible searchable layer.
- `Title.review/` — only pages that need human review and temporary overlays/contact sheets.

## Initial development order

Implement Tasks 1–6 locally first, then qualify geometry on the representative sample. Implement Tasks 7–9 and run GPU OCR only after geometry is stable. Task 10 is the release gate before processing whole books.
