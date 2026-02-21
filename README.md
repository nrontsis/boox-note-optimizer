# Boox Onyx `.note` file optimizer — debloats and previews .note files

✨ **[Try the Web App Live Here!](https://nrontsis.github.io/boox-note-optimizer)** ✨

This web app is a PWA with offline support. A service worker (`web/sw.js`) caches the app shell (HTML, JS, WASM, icons) on install using a cache-first strategy for local assets and network-first for CDN resources. The `demo.note` file is excluded from caching due to its size. 
> [!IMPORTANT]
> Bump the `CACHE_NAME` version in `sw.js` when deploying changes to force clients to re-fetch.

## Overview of the document

A `.note` file is a ZIP archive produced by Boox/Onyx e-ink tablets. It stores handwritten strokes as sequences of pressure/tilt-sensitive points, plus per-stroke metadata (pen type, color, thickness, transform) in protobuf. The rendering model is pen-type-dependent: some pens produce constant-width line segments, others produce pressure-modulated variable-width strokes, filled polygons, raster textures, or scanline fills.

This document describes the file format and rendering rules inferred from examining `.note` files and comparing against device-exported PDFs.

## Boox `.note` File Format

Inferred from multiple test files and cross-referenced against device-exported PDFs. The following related repos were particularly helpful:
- https://github.com/RobertCsordas/OnyxNoteRenderer
- https://github.com/hhornbacher/boox-note-parser

> [!WARNING]
> Details might vary across firmware versions — this repo was only tested with Note Air 5c files generated with latest firmware as of 2026/02/18.

### ZIP Structure

A `.note` file is a ZIP archive. It can contain either a single note or multiple notes.

**Single-note archive** — the most common format from device export. Contains `note/pb/note_info` with the note's metadata. All paths are rooted at `<noteId>/`:

```
<noteId>/
├── note/pb/note_info                                              # protobuf: note metadata
├── template/json/<pageUUID>.template_json                         # JSON: page template config
├── virtual/doc/pb/<noteId>                                        # protobuf: document model
├── virtual/page/pb/<pageUUID>                                     # protobuf: virtual page model
├── document/<noteId>/template/json/<pageUUID>.template_json       # duplicate template config
├── pageModel/pb/<pageModelUUID>                                   # protobuf: page model entry
├── resource/pb/<resourceUUID>#<timestamp>                         # binary: embedded resources
├── point/<pageUUID>/<pageUUID>#<pointsDocUUID>#points             # BINARY: stroke point data
├── shape/<pageUUID>#<shapeDocUUID>#<timestamp>.zip                # nested ZIP → protobuf: stroke metadata
├── extra/pb/extra                                                 # protobuf: extra metadata
└── stash/                                                         # undo history (safe to drop)
    ├── shape/                                                     # current undo buffer
    └── archivedShape/<timestamp>/<pageUUID>#<shapeUUID>#<ts>.zip  # archived undo entries
```

**Multi-note archive** — contains a `note_tree` file in the root with protobuf metadata for all notes. Each note's files are nested under `<noteId>/` with the same structure as above.

**Key UUID cross-references:**
- `pageUUID` — appears in point path, shape path, template filenames
- `pointsDocUUID` — appears in point path and shape protobuf field 16
- `shapeDocUUID` — appears in shape path and shape protobuf field 18
- `noteId` — root folder name, also in virtual/doc and document paths
- `shapeUUID` — per-stroke ID, appears in both `#points` index and shape protobuf field 1

### `#points` Binary Format

Main stroke data blob. All multi-byte integers are **big-endian**.

```
┌─────────────────────── HEADER (76 bytes) ───────────────────────┐
│ u32       : version or page count (observed: 1)                 │
│ 36B ASCII : pageUUID (may be hyphenated or condensed+space-padded) │
│ 36B ASCII : pointsDocUUID (always hyphenated)                   │
├─────────────────────── STROKE DATA (contiguous) ────────────────┤
│ For each stroke:                                                │
│   4B       : zero padding (always 0x00000000)                   │
│   N × 16B  : points (see Point Format below)                   │
├─────────────────────── INDEX ───────────────────────────────────┤
│ For each stroke (44 bytes):                                     │
│   36B ASCII : shapeUUID (matches shape protobuf field 1)        │
│   u32       : offset (absolute from start of blob, incl header) │
│   u32       : size (4B pad + N × 16B points)                    │
│                                                                 │
│ u32  : index_start_offset (absolute from start of blob)         │
└─────────────────────────────────────────────────────────────────┘
```

The last 4 bytes of the entire blob always point to where the index begins.

**Parsing:** Read the header, then read the last 4 bytes to get the index start offset, parse the index to get stroke UUIDs / offsets / sizes, then parse each stroke's points using those offsets.

### Point Format

Each point is 16 bytes, big-endian (`>ffBBHI` in [struct](https://docs.python.org/3/library/struct.html) notation):

| Offset | Size | Type   | Field      | Range     | Notes |
|--------|------|--------|------------|-----------|-------|
| 0      | 4    | f32 BE | x          | ~0–1860   | Horizontal coordinate (PDF points) |
| 4      | 4    | f32 BE | y          | ~0–2480   | Vertical coordinate (PDF points) |
| 8      | 1    | u8     | tilt_x     | 0–255     | Stylus tilt X component, wraps at 256 |
| 9      | 1    | u8     | tilt_y     | 0–255     | Stylus tilt Y component, wraps at 256 |
| 10     | 2    | u16 BE | pressure   | 0–4095    | Stylus pressure (hardware max 4095) |
| 12     | 4    | u32 BE | time_delta | ms        | Time delta from previous point |

**Coordinate system:** Coordinates are in PDF points (1:1 mapping to device-exported PDFs). Despite the 1404×1872 display resolution, the stored coordinates span the full 1860×2480 page. Observed ranges: x ≈ 0–1860, y ≈ 0–2480. Verified by direct comparison of `.note` point coordinates against device-exported PDF path segments.

**Tilt wrapping:** Tilt values wrap at 256 (e.g., 254 → 0 is a +2 change, not -254). Continuous tilt must be unwrapped with modular arithmetic before interpolation.

**Time delta:** Milliseconds since the previous point within the same stroke. The first point's time_delta appears to be an absolute offset.

### Shape Protobuf (inside nested ZIP)

`shape/*.zip` contains another ZIP with a single protobuf file. The top-level message contains repeated field 1 submessages, one per stroke:

| Field | Wire | Type    | Content            | Notes |
|-------|------|---------|--------------------|-------|
| 1     | 2    | string  | shapeUUID          | Matches `#points` index UUID |
| 2     | 0    | varint  | Created timestamp  | Epoch ms |
| 3     | 0    | varint  | Modified timestamp | Epoch ms |
| 4     | 0    | varint  | Color              | ARGB u32 (see below) |
| 5     | 5    | float32 | Thickness          | Line width |
| 7     | 2    | string  | Bounding box JSON  | `{"bottom","empty","left","right","stability","top"}` |
| 8     | 2    | string  | Transform matrix JSON | `{"values":[a,b,tx,c,d,ty,0,0,1]}` — 3×3 row-major affine. Applied to strokes that were moved/scaled on-device. |
| 11    | 2    | string  | Pen config JSON    | See below |
| 12    | 0    | varint  | Pen type           | Brush tool identifier (see Pen Types) |
| 16    | 2    | string  | pointsDocUUID      | Same as `#points` header/path |
| 17    | 2    | string  | Line style JSON    | `{"lineStyle":{"phase","type"}}` |
| 18    | 2    | string  | shapeDocUUID       | Same as shape ZIP path |
| 20    | 2    | string  | Extra JSON         | Contains `featureCollection` for pen_type=40 geometric shapes. See **Geometric Shapes** section. |
| 21    | 2    | string  | Unknown            | Observed: `"[]"` |
| 22    | 2    | string  | Rich text HTML     | HTML-formatted text content for text boxes (pen_type=6, 16). |
| 25    | 2    | bytes   | Point list         | Binary point data for geometric shapes (see **Point List Format** below) |
| 26    | 2    | string  | Repo JSON          | Observed: `'{"repo":{}}'` |

**Color encoding (ARGB u32):**
```
Alpha = (color >> 24) & 0xFF
Red   = (color >> 16) & 0xFF
Green = (color >>  8) & 0xFF
Blue  = (color      ) & 0xFF
```

**Pen config JSON** (field 11):
```json
{
  "penType": 5,
  "maxPressure": 4095.0,
  "displayScale": 0.9435484,
  "dpi": 320.0,
  "alphaFactor": 1.0,
  "pressureSensitivity": ...
}
```
Not all strokes have pen config — a minority lack it entirely. On some devices/firmware versions this field may contain a simpler `displayScale`-only JSON (with `maxPressure`, `revisedDisplayScale`, `source`).

**Ordering:** Shape protobuf submessage order may differ from the `#points` index order. They are cross-referenced by shapeUUID (protobuf field 1 = index UUID).

### Note Metadata Protobuf (`note/pb/note_info` or `note_tree`)

The note metadata protobuf (field tags from reverse engineering):

| Field | Type    | Content                | Notes |
|-------|---------|------------------------|-------|
| 1     | string  | noteId                 | Note UUID |
| 2     | uint64  | Created timestamp      | Epoch ms |
| 3     | uint64  | Modified timestamp     | Epoch ms |
| 6     | string  | Note name              | |
| 8     | uint32  | Flag                   | |
| 9     | float   | Pen width              | |
| 10    | float   | Scale factor           | |
| 11    | string  | Pen settings JSON      | Detailed pen config with quick pen list |
| 12    | string  | Canvas state JSON      | Page dimensions, zoom info, layer list per page |
| 13    | string  | Background config JSON | Page background settings |
| 14    | string  | Device info JSON       | Device name and screen dimensions |
| 15    | uint32  | Fill color             | |
| 16    | uint32  | Pen type               | |
| 20    | string  | Active pages JSON      | `{"pageNameList": [<pageUUID>, ...]}` |
| 21    | string  | Reserved pages JSON    | Same format as active pages |
| 22    | float   | Canvas width           | |
| 23    | float   | Canvas height          | |
| 24    | string  | Location               | |
| 44    | string  | Detached pages JSON    | Same format as active pages |

Multi-note archives use a `note_tree` file whose protobuf wraps repeated note metadata messages at tag 1.

### Page Model Protobuf (`pageModel/pb/`)

| Field | Type    | Content         | Notes |
|-------|---------|-----------------|-------|
| 1     | string  | pageUUID        | |
| 2     | string  | Layers JSON     | `{"layerList": [{"id": N, "lock": bool, "show": bool}, ...]}` |
| 5     | uint64  | Created timestamp | Epoch ms |
| 6     | uint64  | Modified timestamp | Epoch ms |
| 7     | string  | Dimensions JSON | `{"top", "right", "bottom", "left", "empty", "stability"}` |

### Virtual Doc Protobuf (`virtual/doc/pb/`)

| Field | Type    | Content         | Notes |
|-------|---------|-----------------|-------|
| 1     | string  | virtualDocUUID  | |
| 2     | uint64  | Created timestamp | Epoch ms |
| 3     | uint64  | Modified timestamp | Epoch ms |
| 4     | string  | Template UUID   | References a pageUUID |
| 5     | float   | Stability       | |
| 9     | string  | Content JSON    | Content ID, page ID, page size, relative path, content type |

### Virtual Page Protobuf (`virtual/page/pb/`)

| Field | Type    | Content          | Notes |
|-------|---------|------------------|-------|
| 1     | string  | pageUUID         | |
| 2     | uint64  | Created timestamp | Epoch ms |
| 3     | uint64  | Modified timestamp | Epoch ms |
| 4     | float   | Zoom scale       | |
| 6     | string  | Dimensions JSON  | Same format as page model |
| 7     | string  | Layout JSON      | |
| 8     | string  | Geo JSON         | |
| 10    | string  | Template path    | |
| 12    | string  | Page number      | |

### Pen Types

Field 12 in the shape protobuf identifies the brush tool. Observed values and their rendering behavior (determined by comparing `.note` stroke data against device-exported PDFs):

| pen_type | Brush Name (approx) | Rendering | Notes |
|----------|---------------------|-----------|-------|
| 2  | Ballpoint / Fineliner | Stroked line segments, constant width | Pressure-agnostic. Width = thickness. |
| 5  | Fountain Pen | Stroked line segments, varying width per segment | Pressure-sensitive. Width derived from pressure per point. |
| 15 | Highlighter | Stroked line segments, constant width | Very thick. Constant width = thickness. Multiply blend mode at ~50% opacity. |
| 21 | Marker | Stroked line segments, varying width per segment | Pressure-sensitive, similar to pen_type 5. |
| 22 | Charcoal | Per-stroke raster image | Tilt-sensitive. See **Charcoal Raster Rendering** section below. |
| 37 | Fill | Scanline fill rectangles | Points are interleaved scanline pairs: even-indexed = left edge, odd-indexed = right edge. Each pair defines one horizontal fill span. Thickness always 1.0. |
| 40 | Geometric Shapes | GeoJSON-based rendering | Uses field 20 `extra` JSON with `featureCollection`. See **Geometric Shapes (pen_type=40)** section. |
| 60 | Calligraphy Brush A | Filled polygon (no stroke) | Tilt-sensitive. Closed filled path (~5x more segments than input points). No per-segment widths. |
| 61 | Calligraphy Brush B | Filled polygon (no stroke) | Tilt-sensitive. Same as 60 but different fill tessellation. |

**Rendering summary:**
- **Stroked types** (2, 5, 15, 21): Each point maps to approximately one line segment. Width is either constant (types 2, 15) or derived from pressure (types 5, 21).
- **Fill type** (37): Points encode a scanline fill — even/odd interleaved pairs define horizontal spans that tile the filled region.
- **Filled types** (60, 61): The stroke outline is tessellated into a closed polygon. Segment count is much larger than point count (~5x). No per-segment width — the shape is filled.
- **Raster types** (22): Each stroke is a separate raster image. See **Charcoal Raster Rendering** below.
- **Text types** (6, 16): Text boxes with plain text (field 10) and/or HTML rich text (field 22). See **Text Boxes** section.
- **Geometric shapes** (40): GeoJSON-based vector shapes using field 20. See **Geometric Shapes** section.

### Rendering Pipeline

Each stroke is rendered by:
1. **Looking up metadata** — the shape protobuf provides pen_type, thickness, color (ARGB), and an optional affine transform matrix
2. **Applying the transform** — if the stroke was moved/scaled on-device, the 3×3 affine matrix (`{"values":[a,b,tx,c,d,ty,0,0,1]}`) is applied to each point: `x' = a*x + b*y + tx`, `y' = c*x + d*y + ty`
3. **Unwrapping tilt** — 8-bit tilt values that wrap at 256 are unwrapped with modular arithmetic to produce continuous angles
4. **Computing width** — per-point width is computed from pressure and pen-type-specific formulas (see Width Formulas below)
5. **Drawing** — the pen type determines the drawing primitive (line segments, filled polygon, raster texture, or scanline rectangles)

**Compositing:**
- Most pen types use normal (source-over) blending at full opacity
- Highlighter (pen_type 15) uses **multiply** blend mode at ~50% opacity
- All stroked types use **round** line caps and **round** line joins
- Strokes are rendered bottom-to-top in creation-timestamp order (field 2 in shape protobuf); later strokes occlude earlier ones

**Color:** Stored as ARGB u32 in protobuf field 4. For most pen types, alpha comes from the ARGB value. The highlighter overrides alpha to ~50%.

### Width Formulas (Pressure-Sensitive Pens)

Fitted by comparing `.note` stroke data against device-exported PDF output. Thickness values in `.note` are already in PDF points — no scaling needed.

| pen_type | Formula | Params | RMSE |
|----------|---------|--------|------|
| 2 (Ballpoint) | `w = thickness` (constant) | — | exact |
| 5 (Fountain) | `w = thickness × 1.37 × (pressure/4095)^0.59` | k=1.37, exp=0.59 | 0.063 |
| 15 (Highlighter) | `w = thickness` (constant) | — | exact |
| 21 (Marker) | `w = thickness × 2.35 × (pressure/4095)^0.43` | k=2.35, exp=0.43 | 1.207 |

For variable-width pens (5, 21), each segment uses the average pressure of its two endpoints. Width is clamped to a minimum of 0.5pt.

### Charcoal Raster Rendering (pen_type=22)

Charcoal strokes are **not** rendered as vector paths. On the device, each charcoal stroke is exported as a raster image: a solid-color RGB layer with a binary alpha mask that creates the "grain" texture.

**Width envelope:** The charcoal stroke's outline follows the same pressure-dependent variable-width model as the fountain pen: `w = thickness × 1.37 × (pressure/4095)^0.59`, rendered as a filled polygon (same `fill_stroke_outline` approach as calligraphy).

**Texture characteristics:**
- The alpha mask forms a scattered dot pattern along the stroke path — sparse individual pixels with gaps between them
- Density varies along the stroke, roughly correlating with pressure
- The pattern resembles charcoal on textured paper — not a solid filled path
- `tilt_x` encodes pen azimuth (wraps near 0/255); `tilt_y` encodes elevation (narrow range)
- The exact algorithm mapping (position, tilt, pressure) → pixel mask is unknown, but the texture appears to be a deterministic scattered pattern

**Rendering approach:**
Charcoal is approximated with procedural stippling: a 64×64 tiled grain pattern (solid color with ~30% of pixels erased) is used as a `CanvasPattern` fill inside the stroke's variable-width polygon outline. The pattern's RNG is seeded per-stroke (from UUID hash) for deterministic output. This produces a visually similar scattered grain effect without reverse-engineering the exact device algorithm.

### Calligraphy Brush Rendering (pen_type=60, 61)

See **[calligraphy.md](calligraphy.md)** for the detailed chisel-tip model, width formula, smoothing pipeline, and known limitations.

### Templates & Backgrounds

Pages can have templates (ruled lines, grids, dot grids) and background images.

**Templates** are stored at `template/json/<pageUUID>.template_json`. The JSON contains:
```json
{
  "layoutType": "LayoutResVector",
  "properties": {
    "resourceAttr": { "resName": "template/<templateName>" },
    "spacing": 68.0,
    "shaderRect": { "left": 0, "right": 1860, "top": 0, "bottom": 2480 }
  }
}
```
The template SVG is fetched from the Boox CDN at `https://static.send2boox.com/device/note/template/{templateName}.svg`. Observed template names: `ic_horizontal_line_24` (ruled lines), `new_scribble_back_ground_grid_point` (dot grid).

**Backgrounds** are stored in the `note_info` protobuf field 13 as a JSON string:
```json
{
  "useDocBKGround": true,
  "docBKGround": { "type": 1, "resId": "<resourceUUID>" },
  "pageBKGroundMap": { "<pageUUID>": { "type": 1, "resId": "<resourceUUID>" } }
}
```
Background type `1` = image file. The `resId` references a resource in `resource/pb/`. Per-page overrides are in `pageBKGroundMap`.

### Text Boxes (pen_type=6, 16)

Text boxes are shape entries with pen_type 6 or 16. They may appear in the `#points` index (with 2 points defining the bounding box corners) or only in the shape protobuf.

**Content fields:**
- Field 10: plain text content
- Field 22: HTML rich text (e.g. `<p><span style="...">text</span></p>`)
- Field 9: text style JSON with formatting properties:
  ```json
  { "textSize": 32, "textBold": false, "textItalic": false, "alignType": 0, "textSpacing": 1.2 }
  ```
  `alignType`: 0=left, 1=center, 2=right.

**Positioning:** From bounding_rect (field 7) or from 2 points in the `#points` data defining opposite corners of the text box.

### Geometric Shapes (pen_type=40)

Geometric shapes drawn with the Boox shape tools (lines, arrows, polygons, ovals, curves, brackets, etc.) use pen_type=40 and store their geometry in protobuf field 20 (`extra`) instead of field 25 (`pointList`).

**Field 20 format:** A JSON string containing a `featureCollection` key whose value is itself a JSON string in GeoJSON-like format:
```json
{
  "featureCollection": "{\"type\":\"FeatureCollection\",\"features\":[...]}"
}
```

**Coordinate system:** Coordinates in the geometry are in local space. The matrix (field 8) transforms local → page coordinates: `x' = a*x + b*y + tx`, `y' = c*x + d*y + ty`. The matrix can include Y-flips (negative d), scaling, and rotation.

**Geometry types observed in `.note` files:**

| geometry.type | subType | coords format | Rendering |
|---|---|---|---|
| LineString | "" | `[[x0,y0],[x1,y1]]` | Straight line |
| LineString | WaveLine | `[[x0,y0],[x1,y1]]` | Sine wave between endpoints |
| DirectionLine | "" | `[[x0,y0],[x1,y1]]` | Line with arrowhead at end |
| BidirectionalLine | "" | `[[x0,y0],[x1,y1]]` | Line with arrowheads at both ends |
| MultiLineString | "" | `[[[x0,y0],[x1,y1]], ...]` | Multiple line segments (used for arrow head lines) |
| Polygon | "" | `[[[start,end], ...]]` | Pairs of segment endpoints forming a closed ring |
| MultiPoint | Oval | `[[x0,y0],[x1,y1]]` | Bounding box → ellipse |
| MultiPoint | Curve | `[[start],[control],[end]]` | Quadratic Bezier curve |
| MultiPoint | Arc | `[[bboxMin],[bboxMax],[angleCtrl]]` | Elliptical arc within bounding box. 3rd point x=0 → upper half, x=180 → lower half |
| MultiPoint | Bracket | `[[tip],[topEnd],[bottomEnd]]` | Bracket/brace: tip is the apex, topEnd/bottomEnd are the open ends |
| FeatureCollection | Surface | nested `features[]` | Recurse into sub-features. Used for compound shapes: 3D solids (cube, pyramid, cylinder), shapes with hidden edges |

**Polygon coordinate format:** Unlike standard GeoJSON, polygon coordinates are stored as pairs of `[start_point, end_point]` for each edge segment, not as a simple vertex list. The first point of each pair forms the polygon vertex.

**SubType location:** The geometry subtype (Oval, Arc, Curve, Bracket, WaveLine, Surface) is stored in `feature.properties.subType`, not on the geometry object itself.

**Styling:** Color from field 4, thickness from field 5. Individual features may have a `strokeAttr` object with `lineWidth` and `color` overrides.

**Dashed lines:** Features can specify `lineStyle: {"dashLineIntervals": [8.0, 5.0], "phase": 0.0, "type": 1}` for dashed rendering. Used for hidden edges in 3D shapes.

**WaveLine properties:** WaveLine features include `waveAttr` in `feature.properties`: `{"wavyLength": 24.0, "wavyPeak": 6.0, "wavyOffset": 0.0}` controlling wavelength, amplitude, and phase offset.

### Point List Format (field 25)

Used by geometric shapes with pen_type 0 (ellipse), 1 (rectangle), 7 (line), 8/10–12/17/18/24/26/27 (polygons), 28 (arrow), 31 (polyline). Binary format: 4-byte header followed by 16-byte records (same layout as stroke points: x:f32 BE, y:f32 BE, then 8 bytes of size/pressure/event_time). Only x,y are used for shape geometry.

### Page Geometry & Coordinate Mapping

The page size is **1860 x 2480 points** (approximately 25.83 x 34.44 inches at 72 DPI). Coordinates in `.note` files map 1:1 to device-exported PDF coordinates — no scaling is needed.

### Stroke Z-Ordering

Strokes are rendered bottom-to-top sorted by creation timestamp (shape protobuf field 2). Within a page, the `#points` index order may differ from creation order; the shape metadata's timestamp is the authoritative sort key. Later strokes render on top of earlier ones.

**Fill-first ordering:** Fill strokes (pen_type 37) are rendered before all other strokes, regardless of timestamp. This ensures fills appear behind the outline strokes that border them, matching the device's observed rendering behavior.

### Stash (Undo History)

`stash/` contains undo history (~46% of total file size in typical files). Safe to drop entirely — the device does not require it for rendering. The debloater strips this directory on export.

### Decimation (Optimization) Algorithm

The optimizer reduces file size by removing redundant points from strokes using a modified **Ramer-Douglas-Peucker** algorithm that operates in 5 dimensions: spatial (x, y) plus attribute (pressure, tilt_x, tilt_y). The cost of removing a point is the maximum of:

- **Spatial deviation**: perpendicular distance from the point to the line segment connecting its neighbors
- **Attribute deviation**: maximum interpolation error across pressure and tilt channels (scaled by user-configurable equivalence factors)

Points at sharp turns (>30° angle change, i.e. `cos(angle) < 0.866`) are never removed. First and last points of each stroke are always preserved. A priority queue processes points in ascending cost order, updating neighbor costs after each removal. The threshold parameter controls the quality/size tradeoff.

### Older Backup Format (different!)

The [OnyxNoteRenderer](https://github.com/RobertCsordas/OnyxNoteRenderer) project handles a **different** format: SQLite-based `.note` backup files where points are `Nx6 float32` (byteswapped) in a `NewShapeModel` table, with coordinates in normalized 0–1 range. This is the cloud backup/export format, not the same as standalone `.note` ZIP files from the device.

Key differences from the standalone format:
- SQLite database vs ZIP archive
- Normalized 0–1 coordinates vs device-unit coordinates
- 6 floats per point (x, y, pressure, ?, ?, ?) vs 16-byte packed struct
- `shapeType = 5` used for pressure-sensitive rendering in the backup renderer

### Rendering Comparison Tool (`compare.py`)

A CLI tool for testing rendering accuracy against gold reference PNGs (device-exported screenshots or PDF rasterizations). Requires [uv](https://docs.astral.sh/uv/) — dependencies are declared inline via PEP 723.

**Subcommands:**

```bash
# Render a .note file to PNG via headless Chromium
uv run compare.py render shapes.note [-o rendered.png] [--page 0]

# Compare two existing PNGs (gold vs rendered)
uv run compare.py diff gold.png rendered.png [--note shapes.note] [--non-overlapping] [-o diff.png]

# Render + compare in one step (most common)
uv run compare.py check shapes.note shapes.png [--non-overlapping] [-o diff.png] [--page 0]
```

**How it works:**
1. Spins up a local HTTP server serving the `web/` directory
2. Uses Playwright (headless Chromium) to load `headless.html`, which initializes the WASM renderer
3. Passes the `.note` file as base64 to `window.renderNote()`, captures the canvas as a PNG
4. Compares rendered vs gold pixel-by-pixel, computing MAE (mean absolute error), max error, and percentage of differing pixels
5. If a `.note` file is provided, extracts per-shape bounding boxes from the protobuf and reports per-region metrics
6. Outputs a diff visualization with errors amplified 4× and bounding boxes outlined in green (MAE < 1) or red

**Options:**
- `--non-overlapping`: Only report metrics for shapes whose bounding boxes don't overlap with any other, avoiding ambiguous attribution of errors
- `--page N`: Render page N (0-indexed, default 0)
- `-o FILE`: Output path for the diff image

**Output example:**
```
Overall: MAE=1.64  Max=255  Diff pixels: 42,139/4,612,800 (0.91%)

Per bounding box (15 regions):
  [ 841, 310 → 1174, 615] pen=40  MAE=  7.40  Max=255  diff=5.2%  DIFF
  [ 100, 200 →  400, 500] pen=40  MAE=  0.45  Max= 12  diff=0.3%  OK
```

### Open Questions

1. **Header u32**: Version number or page count? Only value `1` observed.
2. **Bounding boxes**: Must they be updated when points change, or does the device recompute? (Currently we do not update them and the device accepts the file.)
3. **Pen type completeness**: 10 values observed (2, 5, 6, 15, 16, 21, 22, 37, 40, 60, 61). The Boox app offers additional brush tools (pencil, etc.) whose pen_type integers have not yet been observed in test files.
4. **Pen config absence**: Why do some strokes (~12%) lack pen config JSON (field 11)? Possibly firmware version dependent. On some devices/firmware versions this field may contain a simpler `displayScale`-only JSON (with `maxPressure`, `revisedDisplayScale`, `source`).
5. **Charcoal texture algorithm**: The exact device algorithm for (position, tilt, pressure) → pixel mask is unknown. Our procedural stipple approximation is visually similar but not pixel-identical. Tilt_x encodes pen azimuth (256 units = full circle, typically 0–25 range); tilt_y encodes elevation (typically 15–33 range).
6. **Velocity effects on width**: Velocity is not stored in `.note` point data (only `time_delta` is stored, from which velocity could theoretically be recomputed using inter-point distances). The marker pen's higher RMSE (1.207 vs 0.063 for fountain) may partly reflect unmodeled velocity effects in the width formula.
7. **Firmware variation**: Format details observed on our device may differ across firmware versions or device models. The [boox-note-parser](https://github.com/hhornbacher/boox-note-parser) project (based on Note Air 4 C, app version 42842) reports some differences in shape protobuf field interpretation — their field 11 contains a simpler `displayScale` JSON and they don't identify fields 4 (color) or 12 (pen_type). These may be firmware-dependent or represent a different interpretation of the same data.
