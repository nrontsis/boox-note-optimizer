# Calligraphy Brush Rendering (pen_type 60 & 61)

## Physical Model

Both pens simulate a **chisel-nib** (broad-edge) calligraphy pen. The nib is a flat edge held at an angle; stroke width varies with the angle between the stroke direction and the nib orientation.

**Key difference between the two pens:**

| Property | Pen 60 (Brush A) | Pen 61 (Brush B) |
|----------|-----------------|-----------------|
| Thick when | Stroke **parallel** to tilt azimuth | Stroke **perpendicular** to tilt azimuth |
| Chisel formula | `\|cos(stroke_dir - nib_angle)\|` | `\|sin(stroke_dir - nib_angle)\|` |
| Min width fraction | 18% of max | 35% of max |
| Visual character | Sharp hairline transitions | Broader, softer variation |

This corresponds to two nib orientations rotated 90 degrees from each other: pen 60's nib edge is aligned with the tilt direction, pen 61's is perpendicular to it.

## Tilt Data

- `tilt_x`: Pen azimuth, 256 units = full circle. Raw values wrap at 0/255; must be unwrapped with `unwrap_8bit()` before use.
- `tilt_y`: Pen elevation, 256 units. Typical range 15-34. Not currently used in the width model (minimal measured effect on stroke width).
- Nib angle: `nib_angle = tilt_x * (2*pi / 256)`

## Width Formula

```
nib_w     = thickness * 0.95 * (pressure / 4095) ^ 0.5
chisel    = |cos(stroke_dir - nib_angle)|    # pen 60
          = |sin(stroke_dir - nib_angle)|    # pen 61
min_frac  = 0.18 (pen 60) or 0.35 (pen 61)
half_width = nib_w * (min_frac + (1 - min_frac) * chisel) / 2
```

Fitted by comparing `.note` stroke data against device-exported output. Typical RMSE on straight sections is ~0.3–0.6px.

## Rendering Method

**Device export:** Filled polygon (closed path with ~5x more segments than input points). No per-segment strokes. The outline naturally expands at turns/corners.

**Our approach:** `fill_stroke_outline()` (in `src/lib.rs`) builds a variable-width polygon from per-segment quadrilaterals with round end caps. This reproduces the device behavior:
- No circle artifacts (unlike per-segment `drawLine` with round caps)
- Natural width expansion at turns
- Start-of-line taper (quadratic ramp over first 8 points)

## Smoothing

Three EMA (exponential moving average) filters ensure smooth output:
1. **Nib angle** (alpha=0.15): Removes sensor jitter in tilt_x
2. **Stroke direction** (alpha=0.3): Stabilizes tangent direction, especially at near-zero velocity
3. **Width** (alpha=0.25): Prevents abrupt width jumps between consecutive points

Stroke direction uses wider central differences (window=3 points each side) before EMA for additional stability.

## Known Limitations

- Gold width measurements at turns are inflated by polygon fill geometry, making precise model fitting difficult at those locations
- The `tilt_y` (elevation) component is not modeled; it may contribute a small width scaling effect
- Our polygon outline doesn't exactly match the device's tessellation algorithm (5x segment expansion), but is visually close
