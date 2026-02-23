#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "playwright",
#     "pillow",
#     "numpy",
# ]
# ///
"""Compare .note rendering against gold reference PNGs.

Usage:
    uv run compare.py check shapes.note shapes.png [--non-overlapping] [-o diff.png]
    uv run compare.py render shapes.note [-o rendered.png] [--page 0]
    uv run compare.py diff gold.png rendered.png [--note shapes.note] [--non-overlapping]
"""
import argparse
import base64
import http.server
import io
import json
import subprocess
import sys
import threading
import zipfile
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

SCRIPT_DIR = Path(__file__).resolve().parent
WEB_DIR = SCRIPT_DIR / "web"


# ── Protobuf parsing for bounding box extraction ──

def decode_varint(data, offset):
    result, shift = 0, 0
    while offset < len(data):
        b = data[offset]; offset += 1
        result |= (b & 0x7F) << shift
        if not (b & 0x80) or shift >= 63: break
        shift += 7
    return result, offset


def parse_shape_entries(data):
    """Parse shape protobuf, extract UUID, pen_type, bounding_rect per shape."""
    entries = []
    off = 0
    while off < len(data):
        tag, off = decode_varint(data, off)
        fn_num, wt = tag >> 3, tag & 0x07
        if fn_num == 1 and wt == 2:
            length, off = decode_varint(data, off)
            if off + length > len(data): break
            msg = data[off:off + length]; off += length
            uuid, pen_type, bbox = "", 2, None
            so = 0
            while so < len(msg):
                stag, so = decode_varint(msg, so)
                sfn, swt = stag >> 3, stag & 0x07
                if sfn == 1 and swt == 2:
                    l, so = decode_varint(msg, so)
                    if so + l > len(msg): break
                    uuid = msg[so:so + l].decode("utf-8", errors="replace"); so += l
                elif sfn == 7 and swt == 2:
                    l, so = decode_varint(msg, so)
                    if so + l > len(msg): break
                    try:
                        j = json.loads(msg[so:so + l])
                        if all(k in j for k in ("top", "left", "bottom", "right")):
                            bbox = [j["top"], j["left"], j["bottom"], j["right"]]
                    except (json.JSONDecodeError, KeyError): pass
                    so += l
                elif sfn == 12 and swt == 0:
                    pen_type, so = decode_varint(msg, so)
                elif swt == 0: _, so = decode_varint(msg, so)
                elif swt == 2:
                    l, so = decode_varint(msg, so)
                    if so + l > len(msg): break
                    so += l
                elif swt == 5: so += 4
                elif swt == 1: so += 8
                else: break
            if uuid and bbox:
                entries.append({"uuid": uuid, "pen_type": pen_type, "bbox": bbox})
        else:
            if wt == 0: _, off = decode_varint(data, off)
            elif wt == 2:
                l, off = decode_varint(data, off); off += l
            elif wt == 5: off += 4
            elif wt == 1: off += 8
            else: break
    return entries


def extract_bboxes(note_path):
    """Extract all shape bounding boxes from a .note file."""
    entries = []
    with zipfile.ZipFile(note_path) as zf:
        for name in zf.namelist():
            if name.endswith(".zip") and "shape" in name and "stash" not in name:
                with zf.open(name) as f:
                    try: inner_zip = zipfile.ZipFile(io.BytesIO(f.read()))
                    except zipfile.BadZipFile: continue
                    for inner_name in inner_zip.namelist():
                        entries.extend(parse_shape_entries(inner_zip.read(inner_name)))
    return entries


def find_non_overlapping(entries):
    """Filter to bounding boxes that don't overlap with any other."""
    def overlaps(a, b):
        return not (a[2] <= b[0] or b[2] <= a[0] or a[3] <= b[1] or b[3] <= a[1])
    return [e for i, e in enumerate(entries)
            if not any(overlaps(e["bbox"], f["bbox"]) for j, f in enumerate(entries) if i != j)]


# ── Headless rendering via Playwright ──

def start_http_server(directory):
    import mimetypes
    mimetypes.add_type("application/wasm", ".wasm")

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=str(directory), **kw)
        def log_message(self, *a): pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def ensure_playwright_browsers():
    """Install Chromium if not already present."""
    try:
        from playwright.sync_api import sync_playwright
        with sync_playwright() as p:
            b = p.chromium.launch(headless=True)
            b.close()
    except Exception:
        print("Installing Chromium for Playwright (one-time)...")
        subprocess.run([sys.executable, "-m", "playwright", "install", "chromium"],
                       check=True, capture_output=True)


def render_note(note_path, page=0):
    """Render a .note file to a PIL Image using headless Chromium."""
    from playwright.sync_api import sync_playwright

    with open(note_path, "rb") as f:
        b64 = base64.b64encode(f.read()).decode()

    server, port = start_http_server(WEB_DIR)
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            pg = browser.new_context().new_page()
            pg.goto(f"http://127.0.0.1:{port}/headless.html", wait_until="networkidle")
            pg.wait_for_function("window.ready === true", timeout=15000)
            data_url = pg.evaluate(
                "async ([b64, idx]) => await window.renderNote(b64, idx)", [b64, page]
            )
            browser.close()
    finally:
        server.shutdown()

    png_bytes = base64.b64decode(data_url.split(",", 1)[1])
    return Image.open(io.BytesIO(png_bytes)).convert("RGBA")


def render_all_pages(note_path):
    """Render all pages of a .note file. Returns list of PIL Images."""
    from playwright.sync_api import sync_playwright

    with open(note_path, "rb") as f:
        b64 = base64.b64encode(f.read()).decode()

    server, port = start_http_server(WEB_DIR)
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            pg = browser.new_context().new_page()
            pg.goto(f"http://127.0.0.1:{port}/headless.html", wait_until="networkidle")
            pg.wait_for_function("window.ready === true", timeout=15000)
            data_urls = pg.evaluate(
                "async (b64) => await window.renderAllPages(b64)", b64
            )
            browser.close()
    finally:
        server.shutdown()

    images = []
    for url in data_urls:
        png_bytes = base64.b64decode(url.split(",", 1)[1])
        images.append(Image.open(io.BytesIO(png_bytes)).convert("RGBA"))
    return images


def render_debloated(note_path, page=0, threshold=0.5, press_eq=100, tilt_eq=20):
    """Render a .note file after debloating with given parameters."""
    from playwright.sync_api import sync_playwright

    with open(note_path, "rb") as f:
        b64 = base64.b64encode(f.read()).decode()

    server, port = start_http_server(WEB_DIR)
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            pg = browser.new_context().new_page()
            pg.goto(f"http://127.0.0.1:{port}/headless.html", wait_until="networkidle")
            pg.wait_for_function("window.ready === true", timeout=15000)
            data_url = pg.evaluate(
                "async ([b64, idx, th, pe, te]) => await window.renderDebloated(b64, idx, th, pe, te)",
                [b64, page, threshold, press_eq, tilt_eq]
            )
            browser.close()
    finally:
        server.shutdown()

    png_bytes = base64.b64decode(data_url.split(",", 1)[1])
    return Image.open(io.BytesIO(png_bytes)).convert("RGBA")


# ── Image comparison ──

def compare_images(gold_img, rendered_img, entries=None, non_overlapping=False):
    """Compare gold vs rendered. Returns (metrics_dict, diff_image)."""
    gold = np.array(gold_img, dtype=np.int16)
    rendered = np.array(rendered_img, dtype=np.int16)

    if gold.shape != rendered.shape:
        h, w = min(gold.shape[0], rendered.shape[0]), min(gold.shape[1], rendered.shape[1])
        print(f"WARNING: size mismatch gold={gold_img.size} vs rendered={rendered_img.size}, cropping to ({w},{h})")
        gold, rendered = gold[:h, :w], rendered[:h, :w]

    diff = np.abs(gold - rendered).astype(np.uint8)
    diff_max_ch = diff.max(axis=2)  # max across RGBA channels per pixel
    diff_pixels = int((diff_max_ch > 0).sum())
    total = gold.shape[0] * gold.shape[1]

    results = {
        "overall": {
            "mae": float(diff.mean()), "max": int(diff.max()),
            "diff_pixels": diff_pixels, "total_pixels": total,
            "pct": diff_pixels / total * 100,
        },
        "bboxes": [],
    }

    if entries:
        selected = find_non_overlapping(entries) if non_overlapping else entries
        for e in selected:
            t, l, b, r = [int(round(v)) for v in e["bbox"]]
            t, l = max(0, t), max(0, l)
            b, r = min(gold.shape[0], b), min(gold.shape[1], r)
            if t >= b or l >= r: continue
            rd = diff[t:b, l:r]
            rpx = int((rd.max(axis=2) > 0).sum())
            rtot = (b - t) * (r - l)
            results["bboxes"].append({
                "uuid": e["uuid"][:8], "pen_type": e["pen_type"],
                "bbox": [t, l, b, r], "mae": float(rd.mean()), "max": int(rd.max()),
                "diff_pixels": rpx, "total_pixels": rtot,
                "pct": rpx / rtot * 100 if rtot else 0,
            })
        results["bboxes"].sort(key=lambda x: x["mae"], reverse=True)

    # Build diff visualization: amplified diff + bbox rectangles
    diff_vis = np.clip(diff.astype(np.float32) * 4, 0, 255).astype(np.uint8)
    if diff_vis.shape[2] == 4:
        diff_vis[:, :, 3] = 255
    diff_img = Image.fromarray(diff_vis)
    draw = ImageDraw.Draw(diff_img)
    for b_info in results["bboxes"]:
        t, l, b, r = b_info["bbox"]
        color = (0, 255, 0) if b_info["mae"] < 1.0 else (255, 0, 0)
        draw.rectangle([l, t, r, b], outline=color, width=2)

    return results, diff_img


def print_results(results):
    o = results["overall"]
    print(f"\nOverall: MAE={o['mae']:.2f}  Max={o['max']}  "
          f"Diff pixels: {o['diff_pixels']:,}/{o['total_pixels']:,} ({o['pct']:.2f}%)")
    if results["bboxes"]:
        print(f"\nPer bounding box ({len(results['bboxes'])} regions):")
        for b in results["bboxes"]:
            ok = "OK" if b["mae"] < 1.0 else "DIFF"
            print(f"  [{b['bbox'][1]:4d},{b['bbox'][0]:4d} → {b['bbox'][3]:4d},{b['bbox'][2]:4d}] "
                  f"pen={b['pen_type']:2d}  MAE={b['mae']:6.2f}  Max={b['max']:3d}  "
                  f"diff={b['pct']:.1f}%  {ok}")


# ── Commands ──

def cmd_render(args):
    if args.page == 'all':
        print(f"Rendering all pages of {args.note}...")
        images = render_all_pages(args.note)
        stem = Path(args.note).stem
        for i, img in enumerate(images):
            out = f"{stem}_p{i}.png" if not args.output else f"{Path(args.output).stem}_p{i}.png"
            img.save(out)
            print(f"  Page {i}: {out} ({img.size[0]}x{img.size[1]})")
        print(f"Rendered {len(images)} pages")
    else:
        page = int(args.page)
        print(f"Rendering {args.note} (page {page})...")
        img = render_note(args.note, page=page)
        out = args.output or Path(args.note).stem + "_rendered.png"
        img.save(out)
        print(f"Saved {out} ({img.size[0]}x{img.size[1]})")


def cmd_diff(args):
    gold = Image.open(args.gold).convert("RGBA")
    rendered = Image.open(args.rendered).convert("RGBA")
    entries = extract_bboxes(args.note) if args.note else None
    results, diff_img = compare_images(gold, rendered, entries, args.non_overlapping)
    out = args.output or Path(args.gold).stem + "_diff.png"
    diff_img.save(out)
    print_results(results)
    print(f"\nDiff image: {out}")


def cmd_check(args):
    page = int(args.page)
    print(f"Rendering {args.note} (page {page})...")
    rendered = render_note(args.note, page=page)
    gold = Image.open(args.gold).convert("RGBA")
    entries = extract_bboxes(args.note)
    print(f"Comparing against {args.gold}...")
    results, diff_img = compare_images(gold, rendered, entries, args.non_overlapping)
    stem = Path(args.note).stem
    out = args.output or f"{stem}_diff.png"
    diff_img.save(out)
    rendered.save(f"{stem}_rendered.png")
    print_results(results)
    print(f"\nDiff image:  {out}")
    print(f"Rendered:    {stem}_rendered.png")
    return results


def main():
    parser = argparse.ArgumentParser(description="Compare .note rendering against gold PNGs")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("render", help="Render .note → PNG")
    p.add_argument("note"); p.add_argument("-o", "--output"); p.add_argument("--page", default="0", help="Page index or 'all'")

    p = sub.add_parser("diff", help="Compare two PNGs")
    p.add_argument("gold"); p.add_argument("rendered")
    p.add_argument("--note", help=".note file for bbox extraction")
    p.add_argument("--non-overlapping", action="store_true"); p.add_argument("-o", "--output")

    p = sub.add_parser("check", help="Render .note + compare against gold PNG")
    p.add_argument("note"); p.add_argument("gold")
    p.add_argument("--page", type=int, default=0)
    p.add_argument("--non-overlapping", action="store_true"); p.add_argument("-o", "--output")

    args = parser.parse_args()
    if args.cmd in ("render", "check"):
        ensure_playwright_browsers()
    {"render": cmd_render, "diff": cmd_diff, "check": cmd_check}[args.cmd](args)


if __name__ == "__main__":
    main()
