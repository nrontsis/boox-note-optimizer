# Boox Onyx `.note` file optimizer — debloats and previews .note files

✨ **[Try the Web App Live Here!](https://nrontsis.github.io/boox-note-optimizer)** ✨

## Boox `.note` File Format

Infered from multiple test files and cross-referenced against device-exported PDFs. The following related repos were particularly helpful:
- https://github.com/RobertCsordas/OnyxNoteRenderer
- https://github.com/hhornbacher/boox-note-parser

> [!WARNING]  
> Details might vary across firmware versions - this repo was only tested with Note Air 5c files generated with latest firmware as of 2026/02/18.

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

Each point is 16 bytes, big-endian (`struct.Struct(">ffBBHI")`):

| Offset | Size | Type   | Field      | Range     | Notes |
|--------|------|--------|------------|-----------|-------|
| 0      | 4    | f32 BE | x          | ~0–1860   | Horizontal coordinate (PDF points) |
| 4      | 4    | f32 BE | y          | ~0–2480   | Vertical coordinate (PDF points) |
| 8      | 1    | u8     | tilt_x     | 0–255     | Stylus tilt X component, wraps at 256 |
| 9      | 1    | u8     | tilt_y     | 0–255     | Stylus tilt Y component, wraps at 256 |
| 10     | 2    | u16 BE | pressure   | 0–4095    | Stylus pressure (hardware max 4095) |
| 12     | 4    | u32 BE | time_delta | ms        | Time delta from previous point |

**Coordinate system:** Coordinates are already in PDF points (1:1 mapping to the exported PDF). Despite the 1404×1872 display resolution, the stored coordinates span the full 1860×2480 PDF page — no scaling is needed. Observed ranges: x ≈ 0–1860, y ≈ 0–2480. (The ~1.325 scale factor mentioned in early analysis was incorrect; direct comparison of calibration.note points to calibration.pdf path segments confirmed exact 1:1 correspondence.)

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
| 21    | 2    | string  | Unknown            | Observed: `"[]"` |
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
Not all strokes have pen config — some (observed ~6/52 in one file) lack it entirely. On some devices/firmware versions this field may contain a simpler `displayScale`-only JSON (with `maxPressure`, `revisedDisplayScale`, `source`).

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

Field 12 in the shape protobuf identifies the brush tool. Observed values and their PDF export behavior (determined by pairing `calibration.note` strokes with their `calibration.pdf` counterparts):

| pen_type | Brush Name (approx) | PDF Export | Notes |
|----------|---------------------|------------|-------|
| 2  | Ballpoint / Fineliner | Stroked line segments, constant width | Pressure-agnostic. Width = thickness. Each stroke is one multi-segment path or chained single-segment paths. |
| 5  | Fountain Pen | Stroked line segments, varying width per segment | Pressure-sensitive. Each point becomes a separate line-segment drawing op with width derived from pressure. |
| 15 | Highlighter | Stroked line segments, constant width | Very thick (e.g., 59pt). Constant width = thickness. Fewer segments than points (some are merged). |
| 21 | Marker | Stroked line segments, varying width per segment | Pressure-sensitive, similar to pen_type 5. Width range observed 7–17pt from thickness 7pt. |
| 22 | Charcoal / Calligraphy | Per-stroke raster (Stamp annotation) | Tilt-sensitive. See **Charcoal Raster Rendering** section below. |
| 37 | Fill | Scanline fill rectangles | Points are interleaved scanline pairs: even-indexed = left edge, odd-indexed = right edge. Each pair defines one horizontal fill span. Multiple colors observed. Thickness always 1.0. |
| 60 | Calligraphy Brush A | Filled polygon (no stroke) | Tilt-sensitive. Outline is exported as a closed filled path (~5x more segments than input points). No per-segment widths. |
| 61 | Calligraphy Brush B | Filled polygon (no stroke) | Tilt-sensitive. Same as 60 but different fill tessellation. Also ~5x segment expansion. |

**PDF export summary:**
- **Stroked types** (2, 5, 15, 21): Each note point maps to approximately one PDF line segment. Width is either constant (types 2, 15) or derived from pressure (types 5, 21).
- **Fill type** (37): Points encode a scanline fill — even/odd interleaved pairs define horizontal spans that tile the filled region.
- **Filled types** (60, 61): The stroke outline is tessellated into a closed polygon. Segment count is much larger than point count (~5x). No per-segment width — the shape is filled.
- **Raster types** (22): Each stroke is a separate raster image in a Stamp annotation. See **Charcoal Raster Rendering** below.

### Width Formulas (Pressure-Sensitive Pens)

Fitted from calibration.note ↔ calibration.pdf stroke pairing. Thickness values in `.note` are already in PDF points — no scaling needed.

| pen_type | Formula | Params | RMSE |
|----------|---------|--------|------|
| 2 (Ballpoint) | `w = thickness` (constant) | — | exact |
| 5 (Fountain) | `w = thickness × 1.37 × (pressure/4095)^0.59` | k=1.37, exp=0.59 | 0.063 |
| 15 (Highlighter) | `w = thickness` (constant) | — | exact |
| 21 (Marker) | `w = thickness × 2.35 × (pressure/4095)^0.43` | k=2.35, exp=0.43 | 1.207 |

For variable-width pens (5, 21), each PDF segment uses the average pressure of its two endpoints. Width is clamped to a minimum of 0.5pt.

### Charcoal Raster Rendering (pen_type=22)

Charcoal strokes are **not** rendered as vector paths. In device-exported PDFs, each charcoal stroke is a separate **Stamp annotation** (`/Name /#23ONYX-STROKE`) containing a raster image.

**PDF structure per charcoal stroke:**
```
Stamp annotation (rect = stroke bounding box + margin)
  └── Form XObject (appearance stream, /N)
       └── Form XObject (/FRM)
            └── Image XObject (/Im0) + SMask (alpha mask)
```

**Image properties:**
- The RGB image is a **single solid color** matching the stroke's ARGB color — all visible pixels have the exact same RGB value
- The alpha mask (SMask) is **binary** (only values 0 and 255) — no anti-aliasing or intermediate alpha
- The texture effect (charcoal "grain") comes entirely from **which pixels have alpha=255** in the mask
- Image size matches the annotation bounding box at ~1:1 pixel-to-point (e.g., a 1143×249pt annotation has a 1079×235px image, scale ≈ 1.06)
- Visible pixels are sparse (~7–12% of the bounding box area), scattered in a pattern that follows the stroke path

**Texture characteristics:**
- The mask forms a scattered dot pattern along the stroke path — sparse individual pixels with gaps (mean gap ~6–10 pixels between visible pixels in a row)
- Density varies along the stroke, roughly correlating with pressure
- The pattern resembles charcoal on textured paper — not a solid filled path
- tilt_x wraps near 0/255 (e.g., stroke 0: tilt_x=0–10, stroke 1: tilt_x=250–255 wrapping to 0). tilt_y stays in a narrow range (21–33)
- The exact algorithm mapping (x, y, tilt_x, tilt_y, pressure) → pixel mask is still unknown, but the texture appears to be a deterministic scattered pattern based on position and tilt

**Rendering approach (render_note.py):**
Charcoal is approximated with procedural stippling: random dots are scattered along the stroke path within the pressure-dependent width envelope. Dot density is proportional to segment length × width (0.15 dots per unit area). Dot radius varies randomly between 0.4–1.4pt. The RNG is seeded per-stroke (from UUID) for deterministic output. This produces a visually similar scattered grain effect without reverse-engineering the exact device algorithm.

### Calligraphy Brush Rendering (pen_type=60, 61)

Calligraphy brushes use a **chisel-tip model** where stroke width depends on the angle between the stroke direction and the pen's tilt azimuth (tilt_x). When the stroke is perpendicular to the chisel edge, the line is thick; when parallel, it becomes a thin hairline.

**Tilt data:** `tilt_x` encodes the pen azimuth with 256 units = full circle. Values are small integers (typically 0–25) and may wrap around 0/255. The `unwrap_8bit()` function handles phase continuity.

**Width formula:**
```
tx_rad = tilt_x × (2π / 256)
tilt_vec = (cos(tx_rad), sin(tx_rad))
chisel = |dot(stroke_normal, tilt_vec)|     # 0=hairline, 1=full width
base_w = thickness × 1.37 × (pressure/4095)^0.59
half_width = base_w × (0.07 + 0.93 × chisel)
```

The stroke is rendered as a filled polygon outline built from left/right offset curves at ±half_width along the stroke normals.

### PDF Page Geometry & Coordinate Mapping

Device-exported PDFs use a page size of **1860 x 2480 points** (approximately 25.83 x 34.44 inches at 72 DPI). Coordinates in `.note` files are **already in PDF points** — no scaling is needed:
- `pdf_x = note_x` (1:1)
- `pdf_y = note_y` (1:1)

Verified by direct comparison: calibration.note point coordinates match calibration.pdf path segment coordinates exactly.

### PDF Annotation Structure & Z-Ordering

Device-exported PDFs store **all strokes as PDF annotations**, not in the page content stream. The page content stream only contains the background raster image (1860×2480, DeviceRGB, mostly black).

**Annotation types used:**
- **Stamp** (`/Name /#23ONYX-STROKE`): Used for fountain pen, brush, marker, charcoal, and highlighter strokes. The appearance stream contains the vector paths or raster image.
- **Ink** (`/InkList`): Used for ballpoint/fineliner strokes.

**Z-ordering:** Annotations are rendered bottom-to-top in list order. The annotation order matches the stroke drawing order from the `.note` file. Later strokes render on top of earlier ones.

**Blend modes:** Most annotations use Normal blending. The highlighter uses `/BM /Multiply` with `/CA 0.498039` (≈50% opacity), which creates the see-through highlighting effect where underlying strokes remain visible.

### Stash (Undo History)

`stash/` contains undo history (~46% of total file size in typical files). Safe to drop entirely — the device does not require it for rendering. The debloater strips this directory on export.

### Older Backup Format (different!)

`OnyxNoteRenderer/` handles a **different** format: SQLite-based `.note` backup files where points are `Nx6 float32` (byteswapped) in a `NewShapeModel` table, with coordinates in normalized 0–1 range (scaled by 792 for letter-size PDF). This is the cloud backup/export format, not the same as standalone `.note` ZIP files from the device.

Key differences from the standalone format:
- SQLite database vs ZIP archive
- Normalized 0–1 coordinates vs device-unit coordinates
- 6 floats per point (x, y, pressure, ?, ?, ?) vs 16-byte packed struct
- `shapeType = 5` used for pressure-sensitive rendering in the backup renderer

### Open Questions

1. **Header u32**: Version number or page count? Only value `1` observed.
2. **Bounding boxes**: Must they be updated when points change, or does the device recompute? (Currently we do not update them and the device accepts the file.)
3. **Pen type completeness**: 8 values observed (2, 5, 15, 21, 22, 37, 60, 61). pen_type=37 is a scanline fill tool. Other Boox brush tools (pencil, etc.) likely have additional values.
4. **Pen config absence**: Why do some strokes (~12%) lack pen config JSON (field 11)? Possibly firmware version dependent. On some devices, field 11 contains a simpler render-scale JSON rather than full pen config.
5. **Charcoal texture algorithm**: The exact device algorithm mapping (position, tilt, pressure) → pixel mask is unknown. Our stipple approximation is visually similar but not identical. Tilt_x encodes pen azimuth (256 units = full circle, typically 0–25 range); tilt_y encodes elevation (typically 15–33 range).
6. **Firmware variation**: Format details observed on our device may differ across firmware versions or device models. The [boox-note-parser](https://github.com/hhornbacher/boox-note-parser) project (based on Note Air 4 C, app version 42842) reports some differences in shape protobuf field interpretation — their field 11 contains a simpler `displayScale` JSON and they don't identify fields 4 (color) or 12 (pen_type). These may be firmware-dependent or represent a different interpretation of the same data.
