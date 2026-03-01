"""Test suite: compare .note WASM rendering against gold-reference PDFs.

Saves inspectable output per region: gold crop, rendered crop, diff crop.
Output goes to gold-pairs/test-output/<timestamp>/ with a `latest` symlink.

Usage:
    uv run test_rendering.py                 # all test files
    uv run test_rendering.py shapes          # single file
    uv run test_rendering.py --no-cache      # force re-render
    uv run test_rendering.py --threshold 30  # adjust template-subtraction threshold
"""
import argparse
import json
import sys
import time
from datetime import datetime
from pathlib import Path

import fitz  # pymupdf
import numpy as np
from PIL import Image, ImageDraw, ImageFont
from scipy.ndimage import binary_dilation, label

from compare import (
    _data_url_to_image,
    _note_b64,
    ensure_playwright_browsers,
    headless_page,
)

SCRIPT_DIR = Path(__file__).resolve().parent
GOLD_DIR = SCRIPT_DIR / "gold-pairs"
NOTE_DIR = GOLD_DIR / "note-files"
EXPECTED_DIR = GOLD_DIR / "expected-outputs"
CACHE_DIR = GOLD_DIR / ".cache"
OUTPUT_BASE = GOLD_DIR / "test-output"

CANVAS_W, CANVAS_H = 1860, 2480
CROP_PAD = 20  # padding around region crops


# ── Gold image extraction ──

def extract_gold_images(pdf_path: Path) -> list[Image.Image]:
    """Extract page images from a gold-reference PDF using pymupdf."""
    doc = fitz.open(str(pdf_path))
    images = []
    for page in doc:
        rect = page.rect
        zoom_x = CANVAS_W / rect.width
        zoom_y = CANVAS_H / rect.height
        mat = fitz.Matrix(zoom_x, zoom_y)
        pix = page.get_pixmap(matrix=mat, alpha=False)
        img = Image.frombytes("RGB", (pix.width, pix.height), pix.samples)
        images.append(img.convert("RGBA"))
    doc.close()
    return images


# ── Caching ──

def _cache_key(stem: str, page: int, suffix: str) -> Path:
    return CACHE_DIR / f"{stem}_p{page}_{suffix}.png"


def _cache_is_fresh(cache_path: Path, note_path: Path, wasm_path: Path) -> bool:
    if not cache_path.exists():
        return False
    cache_mtime = cache_path.stat().st_mtime
    return (cache_mtime > note_path.stat().st_mtime
            and cache_mtime > wasm_path.stat().st_mtime)


# ── Template subtraction ──

def subtract_template(img: np.ndarray, template: np.ndarray, threshold: int = 25) -> np.ndarray:
    """Set pixels close to the template to white."""
    result = img.copy()
    close = np.all(
        np.abs(img[:, :, :3].astype(np.int16) - template[:, :, :3].astype(np.int16)) <= threshold,
        axis=2)
    result[close] = [255, 255, 255, 255]
    return result


# ── Region detection and scoring ──

def _perimeter(mask: np.ndarray) -> int:
    """Count boundary pixels: mask pixels with at least one non-mask 4-neighbor."""
    padded = np.pad(mask, 1, constant_values=False)
    eroded = (padded[1:-1, 1:-1] &
              padded[:-2, 1:-1] & padded[2:, 1:-1] &
              padded[1:-1, :-2] & padded[1:-1, 2:])
    return int((mask & ~eroded).sum())


def find_regions(img_a: np.ndarray, img_b: np.ndarray, min_area: int = 200,
                 merge_radius: int = 15) -> list[dict]:
    """Find regions of non-white content in the union of two images.

    Dilates the union mask by `merge_radius` before labeling, so nearby
    objects merge into single regions. The final mask/bbox uses the
    original (undilated) pixels within each merged component.
    """
    non_white_a = np.any(img_a[:, :, :3] < 250, axis=2)
    non_white_b = np.any(img_b[:, :, :3] < 250, axis=2)
    union = non_white_a | non_white_b

    # Dilate to merge nearby components, then label on the dilated mask
    struct = np.ones((2 * merge_radius + 1, 2 * merge_radius + 1), dtype=bool)
    dilated = binary_dilation(union, structure=struct)
    labeled_arr, n_features = label(dilated)

    regions = []
    for i in range(1, n_features + 1):
        # Use original pixels within this dilated component
        component = union & (labeled_arr == i)
        area = int(component.sum())
        if area < min_area:
            continue
        ys, xs = np.where(component)
        bbox = (int(ys.min()), int(xs.min()), int(ys.max()) + 1, int(xs.max()) + 1)
        perim = _perimeter(component)
        regions.append({"bbox": bbox, "mask": component, "area": area, "perimeter": perim})
    return regions


def _adaptive_threshold(region: dict) -> float:
    """Compute adaptive per-pixel diff threshold based on outline/area ratio.

    Thin shapes (high perimeter/area) have more aliasing-affected pixels,
    so we raise the threshold to avoid false positives from anti-aliasing.
    A filled circle has ratio ~0.01; a 2px-wide stroke has ratio ~1.0.

    Returns a threshold in [10, 80] for counting "truly different" pixels.
    """
    area = region["area"]
    perim = region.get("perimeter", 0)
    if area == 0:
        return 10.0
    ratio = perim / area  # 0 for solid fills, up to ~1 for thin strokes
    # Linear ramp: ratio=0 -> thresh=10, ratio>=0.5 -> thresh=80
    return min(80.0, 10.0 + 140.0 * ratio)


def _adaptive_mae_limit(region: dict) -> float:
    """Compute adaptive MAE fail threshold based on outline/area ratio.

    Thin shapes are expected to have higher MAE due to aliasing differences,
    so we allow a higher MAE before calling a region "failing".

    Returns a threshold in [5, 40].
    """
    area = region["area"]
    perim = region.get("perimeter", 0)
    if area == 0:
        return 5.0
    ratio = perim / area
    return min(40.0, 5.0 + 70.0 * ratio)


def score_region(gold: np.ndarray, rendered: np.ndarray, region: dict) -> dict:
    """Score a region by comparing gold vs rendered within the region mask."""
    t, l, b, r = region["bbox"]
    mask = region["mask"][t:b, l:r]

    gold_crop = gold[t:b, l:r].astype(np.int16)
    rend_crop = rendered[t:b, l:r].astype(np.int16)
    diff = np.abs(gold_crop[:, :, :3] - rend_crop[:, :, :3])

    masked_diff = diff[mask]
    if len(masked_diff) == 0:
        return {"mae": 0.0, "diff_pct": 0.0, "max_diff": 0, "n_pixels": 0,
                "diff_pixels": 0, "px_thresh": 10, "mae_limit": 5.0, "failing": False}

    mae = float(masked_diff.mean())
    max_diff = int(masked_diff.max())

    px_thresh = _adaptive_threshold(region)
    pixel_diffs = diff.max(axis=2)
    diff_pixels = int((pixel_diffs[mask] > px_thresh).sum())
    n_pixels = int(mask.sum())
    diff_pct = diff_pixels / n_pixels * 100 if n_pixels > 0 else 0.0

    mae_limit = _adaptive_mae_limit(region)
    failing = mae > mae_limit

    return {
        "mae": mae, "diff_pct": diff_pct, "max_diff": max_diff,
        "n_pixels": n_pixels, "diff_pixels": diff_pixels,
        "px_thresh": round(px_thresh, 1), "mae_limit": round(mae_limit, 1),
        "failing": failing,
    }


# ── Crop saving ──

def save_region_crops(out_dir: Path, stem: str, page_idx: int, region_idx: int,
                      gold_arr: np.ndarray, rend_arr: np.ndarray, region: dict, scores: dict):
    """Save gold/ours/diff crop triplet for a region."""
    t, l, b, r = region["bbox"]
    # Pad the crop
    t_pad = max(0, t - CROP_PAD)
    l_pad = max(0, l - CROP_PAD)
    b_pad = min(gold_arr.shape[0], b + CROP_PAD)
    r_pad = min(gold_arr.shape[1], r + CROP_PAD)

    gold_crop = gold_arr[t_pad:b_pad, l_pad:r_pad]
    rend_crop = rend_arr[t_pad:b_pad, l_pad:r_pad]

    # Amplified diff (4x, clamped)
    diff_raw = np.abs(gold_crop.astype(np.int16) - rend_crop.astype(np.int16))
    diff_vis = np.clip(diff_raw.astype(np.float32) * 4, 0, 255).astype(np.uint8)
    if diff_vis.shape[2] == 4:
        diff_vis[:, :, 3] = 255

    prefix = f"{stem}_p{page_idx}_r{region_idx:02d}"
    Image.fromarray(gold_crop).save(out_dir / f"{prefix}_gold.png")
    Image.fromarray(rend_crop).save(out_dir / f"{prefix}_ours.png")
    Image.fromarray(diff_vis).save(out_dir / f"{prefix}_diff.png")


def save_full_page_images(out_dir: Path, stem: str, page_idx: int,
                          gold_arr: np.ndarray, rend_arr: np.ndarray,
                          regions: list[dict], scored: list[dict]):
    """Save full-page gold, rendered, and annotated diff images."""
    prefix = f"{stem}_p{page_idx}"

    Image.fromarray(gold_arr).save(out_dir / f"{prefix}_gold.png")
    Image.fromarray(rend_arr).save(out_dir / f"{prefix}_ours.png")

    # Annotated diff: amplified diff with bbox rectangles
    diff_raw = np.abs(gold_arr.astype(np.int16) - rend_arr.astype(np.int16))
    diff_vis = np.clip(diff_raw.astype(np.float32) * 4, 0, 255).astype(np.uint8)
    if diff_vis.shape[2] == 4:
        diff_vis[:, :, 3] = 255
    diff_img = Image.fromarray(diff_vis)
    draw = ImageDraw.Draw(diff_img)
    for i, (region, score) in enumerate(zip(regions, scored)):
        t, l, b, r = region["bbox"]
        color = (255, 0, 0) if score.get("failing", score["mae"] > 5.0) else (0, 255, 0)
        draw.rectangle([l, t, r, b], outline=color, width=2)
        label_text = f"r{i:02d} MAE={score['mae']:.0f}"
        draw.text((l + 2, max(0, t - 12)), label_text, fill=color)
    diff_img.save(out_dir / f"{prefix}_diff.png")


# ── Main test runner ──

def _find_note_file(stem: str) -> Path | None:
    """Find .note file by stem, handling optional N_ prefix (e.g. '1_animals')."""
    direct = NOTE_DIR / f"{stem}.note"
    if direct.exists():
        return direct
    # Try matching with any numeric prefix
    for p in NOTE_DIR.glob("*.note"):
        # Strip leading digits + underscore from filename
        name = p.stem
        import re
        bare = re.sub(r'^\d+_', '', name)
        if bare == stem:
            return p
    return None


def run_test(stem: str, pg, use_cache: bool, threshold: int,
             wasm_path: Path, out_dir: Path) -> list[dict]:
    """Run comparison for one .note file. Returns list of per-page results."""
    note_path = _find_note_file(stem)
    pdf_path = EXPECTED_DIR / f"{stem}.pdf"

    if note_path is None:
        print(f"  SKIP {stem}: no matching .note found")
        return []
    if not pdf_path.exists():
        print(f"  SKIP {stem}: {pdf_path} not found")
        return []

    gold_images = extract_gold_images(pdf_path)
    b64 = _note_b64(note_path)

    # Get the note's actual page count (may differ from PDF if pages were deleted)
    note_page_count = pg.evaluate("([b64]) => window.getPageCount(b64)", [b64])
    n_pages = min(len(gold_images), note_page_count)

    all_results = []
    for page_idx in range(n_pages):
        gold_img = gold_images[page_idx]

        # Render or load from cache
        rendered_cache = _cache_key(stem, page_idx, "rendered")
        template_cache = _cache_key(stem, page_idx, "template")

        if use_cache and _cache_is_fresh(rendered_cache, note_path, wasm_path) \
                and _cache_is_fresh(template_cache, note_path, wasm_path):
            rendered_img = Image.open(rendered_cache).convert("RGBA")
            template_img = Image.open(template_cache).convert("RGBA")
        else:
            data_url = pg.evaluate(
                "async ([b64, idx]) => await window.renderNote(b64, idx)",
                [b64, page_idx])
            rendered_img = _data_url_to_image(data_url)

            tmpl_url = pg.evaluate(
                "async ([b64, idx]) => await window.renderTemplateOnly(b64, idx)",
                [b64, page_idx])
            template_img = _data_url_to_image(tmpl_url)

            CACHE_DIR.mkdir(parents=True, exist_ok=True)
            rendered_img.save(rendered_cache)
            template_img.save(template_cache)

        # Ensure sizes match
        gold_arr = np.array(gold_img)
        rend_arr = np.array(rendered_img)
        tmpl_arr = np.array(template_img)

        if gold_arr.shape[:2] != (CANVAS_H, CANVAS_W):
            gold_img = gold_img.resize((CANVAS_W, CANVAS_H), Image.LANCZOS)
            gold_arr = np.array(gold_img)
        if rend_arr.shape[:2] != gold_arr.shape[:2]:
            rendered_img = rendered_img.resize((gold_arr.shape[1], gold_arr.shape[0]), Image.LANCZOS)
            rend_arr = np.array(rendered_img)
        if tmpl_arr.shape[:2] != gold_arr.shape[:2]:
            template_img = template_img.resize((gold_arr.shape[1], gold_arr.shape[0]), Image.LANCZOS)
            tmpl_arr = np.array(template_img)

        # Subtract template from both
        gold_sub = subtract_template(gold_arr, tmpl_arr, threshold=threshold)
        rend_sub = subtract_template(rend_arr, tmpl_arr, threshold=threshold)

        # Find regions and score
        regions = find_regions(gold_sub, rend_sub)

        # Overall page metrics
        diff_rgb = np.abs(gold_sub[:, :, :3].astype(np.int16) - rend_sub[:, :, :3].astype(np.int16))
        overall_mae = float(diff_rgb.mean())
        overall_max = int(diff_rgb.max())
        diff_pixels = int((diff_rgb.max(axis=2) > 10).sum())
        total_pixels = gold_arr.shape[0] * gold_arr.shape[1]

        # Score all regions
        scored = []
        for region in regions:
            scores = score_region(gold_sub, rend_sub, region)
            t, l, b, r = region["bbox"]
            scored.append({
                "bbox": [t, l, b, r],
                "area": region["area"],
                **scores,
            })

        # Sort by MAE descending
        order = sorted(range(len(scored)), key=lambda i: scored[i]["mae"], reverse=True)
        regions = [regions[i] for i in order]
        scored = [scored[i] for i in order]

        # Save full-page images
        save_full_page_images(out_dir, stem, page_idx, gold_sub, rend_sub, regions, scored)

        # Save per-region crops for failing regions (MAE > 5)
        for i, (region, sc) in enumerate(zip(regions, scored)):
            if sc["failing"]:
                save_region_crops(out_dir, stem, page_idx, i, gold_sub, rend_sub, region, sc)

        page_result = {
            "stem": stem,
            "page": page_idx,
            "overall_mae": overall_mae,
            "overall_max": overall_max,
            "diff_pixels": diff_pixels,
            "total_pixels": total_pixels,
            "diff_pct": diff_pixels / total_pixels * 100,
            "regions": scored,
        }
        all_results.append(page_result)

    return all_results


def print_results(all_results: list[dict]):
    """Print a summary table of results."""
    print()
    print(f"{'File':<25} {'Page':>4} {'MAE':>7} {'Max':>5} {'Diff%':>7} {'Regions':>8}")
    print("-" * 65)

    total_pages = 0
    total_diff_pct = 0.0

    for pr in all_results:
        n_bad = sum(1 for r in pr["regions"] if r.get("failing", r["mae"] > 5.0))
        status = "PASS" if n_bad == 0 else f"FAIL({n_bad})"
        print(f"{pr['stem']:<25} {pr['page']:>4} {pr['overall_mae']:>7.2f} "
              f"{pr['overall_max']:>5} {pr['diff_pct']:>6.2f}% {len(pr['regions']):>4} {status}")

        total_pages += 1
        total_diff_pct += pr["diff_pct"]

        # Print top 5 worst regions
        bad_regions = [r for r in pr["regions"] if r.get("failing", r["mae"] > 5.0)]
        for r in bad_regions[:5]:
            t, l, b, r_coord = r["bbox"]
            lim = r.get("mae_limit", 5.0)
            print(f"  {'':>25} [{l:4d},{t:4d} -> {r_coord:4d},{b:4d}] "
                  f"MAE={r['mae']:6.1f}/{lim:<5.1f}  diff={r['diff_pct']:5.1f}%  "
                  f"area={r['area']:,}")

    print("-" * 65)
    if total_pages > 0:
        print(f"{'TOTAL':<25} {total_pages:>4} pages  "
              f"avg diff={total_diff_pct / total_pages:.2f}%")
    print()


def save_summary_json(out_dir: Path, all_results: list[dict]):
    """Save machine-readable summary."""
    # Strip numpy types for JSON serialization
    clean = []
    for pr in all_results:
        clean.append({
            "stem": pr["stem"], "page": pr["page"],
            "overall_mae": round(pr["overall_mae"], 3),
            "diff_pct": round(pr["diff_pct"], 3),
            "n_regions": len(pr["regions"]),
            "n_failing": sum(1 for r in pr["regions"] if r.get("failing", r["mae"] > 5.0)),
            "regions": [
                {k: (round(v, 2) if isinstance(v, float) else v)
                 for k, v in r.items() if k != "mask"}
                for r in pr["regions"]
            ],
        })
    (out_dir / "summary.json").write_text(json.dumps(clean, indent=2))


def main():
    parser = argparse.ArgumentParser(description="Test .note rendering against gold PDFs")
    parser.add_argument("files", nargs="*", help="Stems to test (default: all)")
    parser.add_argument("--no-cache", action="store_true", help="Force re-render")
    parser.add_argument("--threshold", type=int, default=25,
                        help="Template subtraction threshold (default: 25)")
    args = parser.parse_args()

    wasm_path = SCRIPT_DIR / "web" / "boox_optimizer_bg.wasm"
    if not wasm_path.exists():
        print(f"ERROR: WASM not found at {wasm_path}. Run wasm-pack build first.")
        sys.exit(1)

    # Discover test files
    if args.files:
        stems = args.files
    else:
        import re
        stems = sorted(set(
            re.sub(r'^\d+_', '', p.stem) for p in NOTE_DIR.glob("*.note")
        ))

    if not stems:
        print("No test files found.")
        sys.exit(1)

    # Create timestamped output directory
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    out_dir = OUTPUT_BASE / ts
    out_dir.mkdir(parents=True, exist_ok=True)

    # Update `latest` symlink
    latest = OUTPUT_BASE / "latest"
    if latest.is_symlink() or latest.exists():
        latest.unlink()
    latest.symlink_to(ts)

    print(f"Testing {len(stems)} file(s): {', '.join(stems)}")
    print(f"Cache: {'disabled' if args.no_cache else 'enabled'}, "
          f"template threshold: {args.threshold}")
    print(f"Output: {out_dir}")

    ensure_playwright_browsers()

    use_cache = not args.no_cache
    all_results = []

    t0 = time.time()
    with headless_page() as pg:
        for stem in stems:
            print(f"  {stem}...", end="", flush=True)
            results = run_test(stem, pg, use_cache, args.threshold, wasm_path, out_dir)
            all_results.extend(results)
            print(f" {len(results)} page(s)")

    elapsed = time.time() - t0
    print(f"\nCompleted in {elapsed:.1f}s")

    print_results(all_results)
    save_summary_json(out_dir, all_results)
    print(f"Output saved to: {out_dir}")
    print(f"  Symlink: {latest}")

    # Exit code: non-zero if any page has high diff
    worst = max((r["diff_pct"] for r in all_results), default=0.0)
    if worst > 20.0:
        print(f"FAIL: worst page diff={worst:.1f}%")
        sys.exit(1)


if __name__ == "__main__":
    main()
