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
