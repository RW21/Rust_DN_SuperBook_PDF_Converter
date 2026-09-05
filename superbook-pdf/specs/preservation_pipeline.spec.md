# Preservation Pipeline Contract

## Scope

Task 1 defines the command-line and configuration contract for a future preservation-first geometry pipeline. It does not implement geometry analysis, transform application, manifest generation, or preservation PDF writing.

## CLI

`convert` accepts these typed options:

- `--geometry-only` selects the preservation geometry pipeline.
- `--geometry-action <report|apply>` sets the common geometry action. Its effective default in geometry-only mode is `report`.
- `--rotation-action <off|report|apply>` overrides the common action for 180-degree rotation.
- `--deskew-action <off|report|apply>` overrides the common action for skew correction.

The action options require `--geometry-only`. Rotation and deskew are represented independently in the effective pipeline configuration.

## Safe Effective Configuration

Geometry-only mode forces these legacy stages off regardless of configuration-file defaults:

- RealESRGAN upscaling
- GPU AI processing
- OCR
- margin trimming and content-aware/group cropping
- shadow removal
- deblur
- internal resolution normalization
- color correction
- marker removal
- offset alignment/page-number group crop
- final resizing (`output_height = 0`)

Explicit CLI options that enable these mutating stages are rejected. Redundant disabling options may be accepted. Without `--geometry-only`, all legacy defaults and behavior remain unchanged.

## Dry Run

A geometry-only dry run reports:

1. native page extraction
2. rotation analysis and its effective action
3. deskew analysis and its effective action
4. transform manifest
5. preservation output

It identifies geometry-only mode explicitly and does not present enhancement, cleanup, OCR, crop, normalization, or resizing stages as enabled.

## Fail-Closed Transition

Task 1 does not execute geometry-only conversion. Any non-dry-run geometry-only request fails before output-directory creation, extraction, image mutation, cache handling, or legacy PDF generation, with a clear unsupported/not-yet-implemented error. The library pipeline enforces the same guard so callers cannot bypass the CLI transition.

## Test Cases

- Default geometry-only action resolves independently to `report` for rotation and deskew.
- Per-operation actions override the common action.
- Invalid enum values and action options without geometry-only mode fail argument parsing.
- Explicit mutating legacy options conflict with geometry-only mode.
- Geometry-only dry-run output contains only the preservation stages.
- Non-dry-run geometry-only conversion fails closed without creating output.
- Pipeline/config serialization includes the mode and independent actions.
- Legacy CLI and web-worker defaults remain non-geometry and apply both legacy geometry operations.

## Transform Manifest Contract

Task 2 defines a versioned, deterministic transform manifest independently of the
future geometry algorithms. The artifact path for `document.pdf` is
`document.transforms.jsonl`. `PipelineResult.transform_manifest_path` is optional:
the current destructive legacy pipeline returns `None` because it does not publish
a manifest, while a successful future preservation conversion exposes `Some(path)`
only after publishing the artifact. Path derivation remains available independently.

### JSONL records

The first line is exactly one tagged `header` record. It contains schema version
`1`, the package name and version, and stable source-PDF identity: file name,
complete-file SHA-256 digest, byte length, and physical page count. It contains no
absolute path or timestamp.

Every subsequent line is a tagged `page` record with these ordered fields:

- schema version, zero-based physical `page_index`, and matching one-based
  `source_page_number`;
- optional source-image metadata (PDF object ID, native dimensions, color space,
  bits per component, filter, and decode parameters); `null` preserves pages for
  which no directly usable image XObject is known;
- optional blank classification, where `null` means unavailable or failed;
- rotation evidence and decision: proposed integer angle (`0` or `180`), optional
  score, confidence, snake-case decision, and stable reason;
- deskew evidence and decision: proposed angle, confidence, feature count,
  snake-case decision, and stable reason;
- output flags for pixel change and required review;
- zero or more structured page errors containing stage, code, and message.

Decisions are `unchanged`, `proposed`, `applied`, `rejected`, or `failed`.
Pending page records use physical identity immediately, before extraction or pixel
processing, and explicit pending/disabled reasons.

### Validation

All floating-point values must be finite. Rotation and deskew confidence values
must be in the inclusive range `0.0..=1.0`; invalid values are rejected during
construction, deserialization, and final manifest validation. Rotation proposals
other than `0` or `180` are rejected during construction, direct transform
deserialization, and final validation. Rotation proposals and confidence values
use validated public types so safe public APIs cannot construct invalid values.
Signed zero is normalized to positive zero.

Header and page schema versions must equal the pinned version. The number of page
records must equal the header's physical page count. Page indices must be unique,
contiguous, and complete from zero, and each source page number must equal its
index plus one. Thus blank, failed, unsupported, excluded, and unchanged physical
pages cannot disappear. Index-to-page-number conversion is checked for overflow.
The source filename is one nonempty normal path component with no separators, the
SHA-256 is exactly 64 lowercase hexadecimal characters, the tool name matches the
package name, and the tool version is nonempty. Older nonempty tool versions remain
valid. Unknown fields are rejected at the record and every nested schema level.

Source hashing counts the bytes read and compares that count with metadata lengths
captured before and after hashing. An append or truncation is reported as a typed
source-changed error rather than pairing a digest with stale size metadata.

### Deterministic atomic writing

Finalization validates all records, sorts page records by physical page index,
and serializes one compact JSON object per line with LF termination. Fixed struct
field order, header-first ordering, stable reasons/errors, and omission of
timestamps make identical inputs produce identical bytes regardless of worker
completion order.
Objects inside arbitrary decode parameters are recursively key-sorted, and signed
zero in those parameters is normalized before serialization.

The complete payload is written to a named temporary file in the destination
directory, flushed and file-synced, then persisted over the destination. On Unix,
the destination directory is synced after persistence. Validation or
serialization failure occurs before publication and leaves an existing valid
manifest byte-identical. Replacement behavior is covered by a real overwrite
test rather than assumed from the temporary-file API. A directory-sync failure
after persistence is reported distinctly as committed-but-not-confirmed-durable;
callers must not interpret it as preservation of the previous destination.

### Reading and pipeline API

The reader requires one header followed only by page records, rejects duplicate or
misordered headers and unknown/invalid records, and applies the same validation as
the writer. `PipelineError` preserves manifest failures as a typed source.
`PipelineResult` includes `transform_manifest_path: Option<PathBuf>`; the legacy
pipeline returns `None`, and only a future preservation path that successfully
publishes the manifest returns `Some(path)`. Conversion to the existing cache
result remains unchanged.

Task 2 does not add rotation or deskew evidence algorithms. Those stages remain the
responsibility of Tasks 3 and 4; workers must eventually return indexed outcomes
to a coordinator rather than write JSONL concurrently.

## Task 3A: Conservative Rotation Analysis

Task 3A adds analysis and exact in-memory 180-degree rotation only. It does not
integrate rotation evidence into the pipeline, CLI, configuration, manifest, Markdown,
or preservation writer.

`RotationAnalysisOptions` is validated before analysis. Fraction and confidence
values must be finite and in `0.0..=1.0`; dimensions, contrast, and evidence-count
minimums must be nonzero where required. Conservative defaults include a 3% analysis
border crop, a minimum dimension of 64 pixels, and minimum auto-apply confidence of
0.90.

`analyze_rotation(path, options)` and `analyze_rotation_image(image, options)` return
validated `RotationEvidence`. Proposed degrees are limited to 0 or 180, score is
finite in `-1.0..=1.0`, confidence is finite in `0.0..=1.0`, reasons are stable
snake-case values, and an ambiguity guard is a hard auto-apply veto. The default
auto-apply policy requires a 180-degree proposal, the immutable score safety floor of
0.55, confidence at least 0.90, an `upside_down_evidence` reason, and no ambiguity
guard. The compatibility `detect_upside_down` wrapper returns true only when that
policy approves the evidence.

Analysis is deterministic and operates on a grayscale copy. It excludes a symmetric
border, uses robust contrast plus Otsu thresholding, removes speckle components, and
measures ink, glyph-like components, horizontal text-like lines, unsupported vertical
layout, outer-frame density, and broad plus outer mirrored bands. Blank, tiny, sparse,
cover-like, illustration-like, and vertical pages abstain. Missing band support,
band disagreement, weak score, or weak confidence also fail closed.

`rotate_180_exact` is a decoded-pixel coordinate permutation with no interpolation,
canvas expansion, fill, thresholding, color conversion, or alpha manipulation. It
preserves representative 8-bit and 16-bit grayscale, grayscale-alpha, RGB, and RGBA
`DynamicImage` variants; applying it twice is pixel-identical. The legacy path-based
`correct_upside_down` remains compatibility behavior and is not an archival or
encoded-format-preserving operation. In particular, Task 3A makes no JPEG, indexed,
bilevel, metadata, or PDF-filter preservation claim.

### Task 3A test cases

- Options reject non-finite, out-of-range, and structurally invalid values.
- Evidence construction enforces finite bounded score/confidence and 0/180 proposals.
- Auto-apply policy uses inclusive score/confidence boundaries and ambiguity veto.
- Synthetic upright and exactly rotated text pages produce strong opposite evidence.
- Synthetic blank, uniform, tiny, sparse, illustration, cover, and vertical pages
  abstain and are never approved by the compatibility wrapper.
- Repeated analysis is exactly deterministic and all returned numeric evidence is
  finite and bounded.
- Exact rotation preserves dimensions, color type, samples, channels, and alpha for
  representative 8-bit and 16-bit variants; two rotations restore the original.

## Task 3B: Rotation Policy Configuration

`--rotation-min-confidence` is available only with `--geometry-only`. Its value is a
validated finite threshold in `0.0..=1.0`; NaN, infinities, and out-of-range values
are rejected at CLI, TOML, Serde, and library boundaries. The default is 0.90.

`processing.rotation_min_confidence` configures the same threshold. An explicitly
provided CLI value overrides TOML; an omitted CLI value preserves TOML; otherwise the
default applies. Dry-run output shows the effective threshold. Explicit configuration
file read or parse errors are fatal instead of silently reverting to mutation-capable
defaults. Errors from implicitly discovered legacy configuration retain the existing
default-fallback behavior.

## Task 3C: Rotation Policy Integration

The coordinator assigns a zero-based physical page index before analysis. `off`
performs no image read. `report` returns a `proposed` or `rejected` manifest-ready
rotation outcome and never changes pixels. `apply` first returns an approved proposal;
only a successful decoded-pixel transform finalizes its decision as `applied`. The
immutable score floor, configured confidence threshold, reason, and ambiguity
checks all approve the evidence. Upright evidence is `unchanged`; ambiguous or weak
evidence is `rejected` and requires review. Analysis, copy, and transform failures
propagate as typed pipeline failures instead of being silently treated as unchanged.

The legacy pipeline remains gated by its legacy `deskew` switch, but rotation and
deskew actions are checked independently inside that gate. Geometry-only execution
continues to fail closed until the preservation writer can publish a complete
physical-page manifest; Task 3 provides deterministic indexed outcomes and strict
`RotationTransform` mapping without advertising an unwritten manifest.

### Task 3 policy test cases

- `off` returns without reading even a nonexistent image.
- `report` maps approved 180-degree evidence to `proposed`, never `applied`.
- `apply` leaves approved evidence as a proposal until a successful transform finalizes
  it as `applied`; low-confidence or guarded evidence is rejected with
  `review_required = true`.
- Indexed outcomes retain the coordinator's physical page index.

## Task 4: Conservative Deskew Policy

Deskew analysis and application are separate operations. The coordinator assigns a
zero-based physical page index and analyzes each nonblank page exactly once. `off`
performs no image read. Blank pages are `unchanged` without analysis. Report mode
copies the encoded source bytes and emits a manifest-ready outcome; apply mode uses
the already measured angle rather than running detection again.
Page-edge detection and application use the same clockwise-positive image-coordinate
convention; the recorded proposed angle is therefore the angle passed to the raster
transform, not its negation.

The policy has four explicit CLI/TOML/library settings:

- maximum correction angle: default `5.0` degrees, finite and in `0.0..=15.0` with
  zero rejected;
- minimum confidence: default `0.90`, finite and in `0.0..=1.0`;
- minimum feature count: default `100`, with zero rejected;
- no-op angle: default `0.10` degrees, finite and in `0.0..=15.0`, and strictly
  smaller than the maximum correction angle.

An automatic correction requires an absolute measured angle no greater than the
configured maximum, confidence at least the configured minimum, feature count at least
the configured minimum, and an absolute angle greater than the no-op threshold. The
policy evaluates excessive angle, confidence, and feature support before classifying a
well-supported tiny angle as `unchanged`; low-quality evidence therefore cannot hide
behind the no-op threshold. Evidence outside any safety threshold is `rejected`, remains
byte-identical, and requires review. Approved report evidence is `proposed` and requires
review. Approved apply evidence remains `proposed` until a successful output write
changes it to `applied`. Analysis, copy, and transform failures propagate as typed
pipeline failures.

The current decoded-pixel correction is retained in the manifest-ready policy outcome as
Lanczos-3 interpolation, expanded canvas, opaque white RGBA fill, and RGBA8 output.
These properties are not represented as preservation of the source pixel mode. The
strict transform-manifest schema remains version 1; serializing this application object
is deliberately deferred to the preservation coordinator, where it requires a
version-2 schema with explicit version dispatch. Version 1 is not changed in place. Unchanged, rejected, and report-only pages have no application metadata.
Geometry-only execution remains fail-closed until the native-raster writer and complete
manifest coordinator are available. The numerical defaults are provisional conservative
settings and must be reviewed against representative report data before production
application is enabled.

### Task 4 test cases

- CLI, TOML, Serde, and direct-library policy values reject non-finite,
  out-of-range, zero-count, and cross-field-invalid settings.
- Positive and negative synthetic skew evidence retains finite angle, confidence,
  and feature count; blank evidence remains unchanged.
- Off, report, approved apply, no-op, excessive-angle, weak-confidence, and
  weak-feature outcomes map to the required decisions and review flags.
- Report, no-op, rejected, blank, and vertical/bilevel synthetic pages preserve
  encoded bytes and input order.
- Apply uses the supplied detection once, records its lossy decoded-pixel properties,
  and becomes `applied` only after the output write succeeds.
- Analysis, copy, and transform publication errors fail closed.

## Task 5: Native PDF Page Inventory

Native preservation inspection is a separate API from the legacy DPI-controlled render
extractors. It accepts only a source PDF path and emits exactly one `NativePageRecord`
for every physical page returned by the PDF page tree, in that order. It never scans
orphan objects as a fallback, never invokes ImageMagick or Poppler, and never renders at
a configured DPI. The strict native loader rejects broken page trees (missing nodes,
duplicate/cyclic children, mismatched Parent links or Count values) rather than returning
partial inventories. Traversal is bounded to 128 levels and 100,000 tree nodes. Legacy
reader and raster extraction behavior remain separate; native loading never enters the
legacy A4 fallback or recursive geometry helper. Encrypted inputs are rejected.

Each page record carries its zero-based index, one-based source page number, page object
identity, effective inherited `MediaBox`, optional effective inherited `CropBox`, and
raw inherited rotation together with the object on which each inherited value was
defined. The effective rotation is normalized only after verifying that the raw value is
a multiple of 90 degrees. Missing, malformed, non-finite, or degenerate required
geometry is retained as a typed page failure rather than replaced with an arbitrary A4
or zero default.

Image discovery follows the page content's `Do` operations through named XObjects and
nested Form XObjects, with bounded recursion and cycle detection. It records the full
resource-owner/name path, invocation count, and effective six-value placement matrix so
Task 6 can perform page-local copy-on-write replacement without guessing. Merely
appearing in a resource dictionary does not prove that an image is painted. A direct
reusable page has exactly one painted image occurrence and no visible text, vector,
shading, or inline-image painting. No-content pages are `blank`; non-image content,
inline images, malformed content, unresolved XObjects, recursive Forms, repeated image
placement, and multiple image invocations remain present but require review. The
preservation API does not guess that the largest resource is the page scan.

For every identified image object, native metadata includes object identity, encoded
stream length and SHA-256, dimensions, color space, bits per component, the complete
filter chain, and canonical decode parameters. The native document retains access to
the loaded source stream and complete object graph without exposing `lopdf` objects in
the public API. The original object identity and stream hash are the reuse contract for
unchanged pages; Task 6 reuses the cloned source graph rather than shallow-copying an
image that can reference masks, ICC profiles, or indexed color spaces. Transform decode
capability is reported separately; unsupported decoding, including CCITT until a
validated decoder exists, blocks geometry application for that page rather than
triggering a renderer or lower-DPI fallback. Manifest schema version 1 is unchanged;
its existing `SourceImageMetadata` is populated only when a single source image has
been identified unambiguously and every field can be represented losslessly. Complex
metadata that cannot be projected to schema version 1 fails projection explicitly.

Native decoding is an explicit separate request with an output-sample byte budget,
not an effect of inspection. The initial decoder accepts 8-bit DeviceGray/DeviceRGB
Flate without prediction and baseline 8-bit DCT, revalidates the supplied metadata and
encoded hash against the loaded object, and verifies decoded dimensions and pixel mode.
DCT decoding retains the codec's native samples; it cannot recover pre-JPEG originals.
The caller budget is hard-capped at 256 MiB of samples; encoded input is separately
capped at 256 MiB. Codec working memory is not claimed to fit the sample budget.
Masks, custom Decode arrays, predictors, complex color spaces, CCITT and other unsupported
image semantics are never passed to the legacy renderer as a fallback. They remain
available as unchanged source objects for the future writer.

Content decoding accepts unfiltered and strictly validated Flate streams only. Missing
references, malformed/truncated compression, unconsumed parser suffixes, over-limit
content, inline images, clipping, optional content, and unmodeled graphics state produce
page-scoped review issues. Nested Forms retain their image bindings and matrices but
require review because their BBox clipping/group semantics are not modeled yet. There
is no claim that a `SingleImage` classification alone authorizes a future transform.

### Task 5 test cases

- Reverse object registration cannot alter physical page-tree order.
- Direct-image, truly blank, nested-Form image, multi-image, and non-image pages each
  produce one record with the required classification and review flag.
- Inherited `MediaBox`, `CropBox`, rotation, and resources are resolved without A4
  substitution; invalid required geometry is recorded as an issue.
- DCT RGB, Flate grayscale, and CCITT bilevel streams retain dimensions, color space,
  bit depth, filter chain, decode parameters, encoded length, and encoded SHA-256.
- Repeated image invocations, unresolved names, inline images, malformed content, and
  Form cycles fail closed at page scope without changing record cardinality.
- Native inspection has no DPI argument and creates no raster files.

### Task 6 handoff contract

The first preservation writer consumes the Task 5 inventory and a clone of the same
loaded source document. Unchanged pages are not mutated. A transformed page is eligible
only when its inventory has one direct image binding with a verified placement matrix
and a supported lossless decoder. The writer adds a new losslessly encoded image object
and performs page-local copy-on-write resource rebinding; it never overwrites a shared
source image or shared inherited resource dictionary. Nested-Form rewrites remain
unchanged and review-required until copy-on-write along the complete Form/resource path
is implemented and tested. The writer must reload staged output and verify page order,
page geometry, unchanged object IDs and encoded hashes, and changed decoded-raster hashes
before any transform can become `applied` or any PDF/manifest pair can be published.

## Task 6A: Verified Native-Raster Writer and Output Bundle

The writer is separate from the legacy JPEG/PDF renderer. It clones the complete
parsed native source document; it never globally recompresses, prunes, renumbers,
or rewrites unchanged page contents. Explicit prepared raster replacements are
accepted only for unambiguous supported direct-image pages. Their dimensions must
match the source raster exactly. Shared image objects and resources are not modified;
only a page-local resource binding points to a new losslessly encoded image.

Gray/RGB and alpha variants at 8/16 bits are encoded with explicit Flate dictionaries;
16-bit samples use PDF big-endian order and alpha uses a lossless SMask. The PDF
header version is promoted when necessary: at least 1.2 for escaped names, 1.4 for
soft masks, and 1.5 for 16-bit images. Unchanged
bilevel/CCITT streams remain encoded byte-identical. There is no JPEG fallback and
no implicit resize, crop, DPI setting, or color conversion. Expanded deskew rasters
are explicitly rejected by this initial writer rather than stretched or clipped.
Signed/encrypted inputs are rejected. Source files and existing outputs are never
overwritten. Output is reloaded and checked against the source object graph and
expected decoded samples before a write receipt is returned.

The output-bundle coordinator stages `document.pdf` and `output-manifest.jsonl` in
one same-parent temporary directory. Its version-2 preservation-output audit format
has a header and exactly one ordered page record per physical source page. It records
source/output hashes, source page identities/geometry and all discovered image hashes,
review issues, and the writer's verified pixel receipts. It does not alter the strict
version-1 transform-manifest reader or claim that a prepared raster replacement is an
automatically approved rotation or deskew. The output reader explicitly rejects other
versions and validates page cardinality, indexes, hash syntax, and output file hashes.
Both staged files are synced and read back before publication. On Linux, publication
uses an atomic no-replace directory rename; other platforms fail closed until an
equivalent reviewed primitive is implemented. A parent-directory sync failure after
rename is reported as a durability uncertainty with the published path, not success.

Task 6A provides a working writer/output audit API, not the automatic geometry dispatcher.
CLI geometry application remains fail-closed until policy outcomes are bound to these
verified receipts and the expanded-deskew placement contract is implemented. No real
scan qualification or source modifications are part of the synthetic writer tests.

### Transform manifest test cases

- Serde round-trip preserves every schema field and snake-case decisions.
- NaN and both infinities are rejected for angles, scores, and confidences.
- Confidence below zero or above one is rejected.
- Ten scrambled page records are written as exactly ten ordered page records after
  the header.
- Blank, low-confidence, corrected, failed, untouched, and no-image pages all
  round-trip without losing physical page identity.
- Duplicate, missing, noncontiguous, misnumbered, and wrong-count pages fail
  validation.
- Atomic writing replaces an existing valid destination without leftover temporary
  files; pre-publication failure preserves the old destination bytes.
- Source identity hashes the complete source PDF.
- Pipeline manifest-path derivation and result propagation are stable.
- Legacy pipeline results do not advertise a nonexistent manifest.
