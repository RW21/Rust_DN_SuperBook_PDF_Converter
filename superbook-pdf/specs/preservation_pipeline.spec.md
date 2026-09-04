# Preservation Pipeline: Geometry-Only CLI Contract

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
