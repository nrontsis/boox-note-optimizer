"""Test SVG export round-trip: load .note -> export SVG -> check size -> re-import -> compare visuals.

Usage:
    uv run test_svg_roundtrip.py          # test all gold files
    uv run test_svg_roundtrip.py taro     # test only matching file(s)
"""
import argparse
import base64
import http.server
import io
import sys
import threading
from pathlib import Path

import numpy as np
from PIL import Image

SCRIPT_DIR = Path(__file__).resolve().parent
WEB_DIR = SCRIPT_DIR / "web"
NOTE_DIR = SCRIPT_DIR / "gold-pairs" / "note-files"


def _start_http_server(directory):
    import mimetypes
    mimetypes.add_type("application/wasm", ".wasm")

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=str(directory), **kw)
        def log_message(self, *a): pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def data_url_to_image(data_url):
    """Convert a data:image/png;base64,... URL to a PIL Image."""
    header, b64data = data_url.split(",", 1)
    return Image.open(io.BytesIO(base64.b64decode(b64data)))


def _mask_template(img, tmpl_img):
    """Set pixels to white where template has non-white content (dot grid etc)."""
    tmpl = np.array(tmpl_img, dtype=np.uint8)
    if tmpl.shape[:2] != img.shape[:2]:
        tmpl = np.array(Image.fromarray(tmpl).resize(
            (img.shape[1], img.shape[0]), Image.LANCZOS), dtype=np.uint8)
    mask = np.abs(tmpl[:, :, :3].astype(np.int16) - 255).max(axis=2) > 10
    for c in range(min(3, img.shape[2])):
        img[:, :, c][mask] = 255


def compare_images(ref_img, imp_img, template_img=None, template_img_rt=None,
                   transform=None):
    """Compare two PIL images with transform-based alignment.

    Uses the SVG import transform (scale + offset) to map the round-trip
    render back to the original coordinate space. Masks out template pixels.

    transform: dict with scaleX, scaleY, offsetX, offsetY from SVG import
    """
    ref = np.array(ref_img, dtype=np.int16)
    imp_raw = np.array(imp_img, dtype=np.int16)

    # Apply inverse transform to align RT image with original
    if transform:
        sx = transform["scaleX"]
        sy = transform["scaleY"]
        ox = transform["offsetX"]
        oy = transform["offsetY"]
        rt_w = transform.get("pdfW", imp_raw.shape[1])
        rt_h = transform.get("pdfH", imp_raw.shape[0])
        orig_h, orig_w = ref.shape[:2]

        # Map original pixel (x,y) -> RT pixel: rx = x*sx + ox, ry = y*sy + oy
        # Build the aligned image by sampling RT at the transformed coordinates
        ys = np.arange(orig_h)
        xs = np.arange(orig_w)
        ry = (ys * sy + oy).astype(int)
        rx = (xs * sx + ox).astype(int)

        # Clip to RT bounds
        ry = np.clip(ry, 0, rt_h - 1)
        rx = np.clip(rx, 0, rt_w - 1)

        # Resample: imp_aligned[y,x] = imp_raw[ry[y], rx[x]]
        imp = imp_raw[np.ix_(ry, rx)]

        # Also resample RT template
        if template_img_rt is not None:
            tmpl_rt_arr = np.array(template_img_rt, dtype=np.uint8)
            template_img_rt = Image.fromarray(tmpl_rt_arr[np.ix_(ry, rx)])
    else:
        imp = imp_raw
        if ref.shape[:2] != imp.shape[:2]:
            imp_pil = Image.fromarray(np.clip(imp, 0, 255).astype(np.uint8))
            imp_pil = imp_pil.resize((ref.shape[1], ref.shape[0]), Image.LANCZOS)
            imp = np.array(imp_pil, dtype=np.int16)

    # Mask template pixels
    if template_img is not None:
        _mask_template(ref, template_img)
    if template_img_rt is not None:
        _mask_template(imp, template_img_rt)

    diff = np.abs(ref[:, :, :3] - imp[:, :, :3])
    mae = float(diff.mean())
    max_diff = int(diff.max())
    total = ref.shape[0] * ref.shape[1]
    sig_diff = int((diff.max(axis=2) > 30).sum())
    return {
        "mae": mae,
        "max_diff": max_diff,
        "sig_diff_pct": sig_diff / total * 100,
        "diff_array": diff,
    }


def test_one_note(note_path, page_headless, page_index, port):
    """Run round-trip test for a single .note file. Returns (ok, mae_or_none)."""
    stem = note_path.stem
    note_b64 = base64.b64encode(note_path.read_bytes()).decode()
    note_size = note_path.stat().st_size
    ok = True
    mae = None

    # Phase 1: Render original + template + export SVG via headless.html
    orig_url = page_headless.evaluate(
        "async (b64) => await window.renderNote(b64, 0)", note_b64)
    orig_img = data_url_to_image(orig_url)

    tmpl_url = page_headless.evaluate(
        "async (b64) => await window.renderTemplateOnly(b64, 0)", note_b64)
    tmpl_img = data_url_to_image(tmpl_url)

    svg_str = page_headless.evaluate(
        "async (b64) => await window.exportSVG(b64, 0)", note_b64)

    svg_size = len(svg_str.encode('utf-8'))
    svg_status = "PASS"
    if svg_size > 2_000_000:
        svg_status = "WARN(>2MB)"
    elif svg_size > 1_000_000:
        svg_status = "WARN(>1MB)"

    # Phase 2: Round-trip import via index.html
    # Reset the app state for each file
    page_index.evaluate("() => { if (window._app) { try { window._app.free(); } catch(e) {} } }")
    result = page_index.evaluate("(svg) => window._testImportSVG(svg)", svg_str)

    if isinstance(result, dict) and result.get("error"):
        print(f"  {stem}: FAIL import — {result['error']}")
        return False, None

    transform = result.get("transform") if isinstance(result, dict) else None

    # Export round-trip .note
    note_b64_out = None
    try:
        note_b64_out = page_index.evaluate("window._testExportNote()")
    except Exception:
        pass

    if not note_b64_out:
        print(f"  {stem}: FAIL export")
        return False, None

    out_size = len(base64.b64decode(note_b64_out))
    note_ratio = out_size / note_size

    # Phase 3: Re-render round-trip .note via headless for fair comparison
    rt_url = page_headless.evaluate(
        "async (b64) => await window.renderNote(b64, 0)", note_b64_out)
    rt_img = data_url_to_image(rt_url)
    rt_tmpl_url = page_headless.evaluate(
        "async (b64) => await window.renderTemplateOnly(b64, 0)", note_b64_out)
    rt_tmpl_img = data_url_to_image(rt_tmpl_url)

    metrics = compare_images(orig_img, rt_img,
                             template_img=tmpl_img,
                             template_img_rt=rt_tmpl_img,
                             transform=transform)
    mae = metrics["mae"]
    sig_pct = metrics["sig_diff_pct"]

    # Save debug images
    out_dir = Path("/tmp/claude")
    out_dir.mkdir(exist_ok=True)
    orig_img.save(out_dir / f"{stem}_orig.png")
    rt_img.save(out_dir / f"{stem}_rt.png")
    diff = metrics["diff_array"]
    diff_img = Image.fromarray(np.clip(diff * 3, 0, 255).astype(np.uint8))
    diff_img.save(out_dir / f"{stem}_diff.png")

    if mae > 20:
        status = "FAIL"
        ok = False
    elif mae > 10:
        status = "WARN"
    else:
        status = "PASS"

    print(f"  {stem:30s}  MAE={mae:5.1f}  sig={sig_pct:4.1f}%  "
          f"SVG={svg_size/1024:5.0f}KB({svg_status})  "
          f".note={note_ratio:.1f}x  {status}")

    return ok, mae


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("note", nargs="?", default=None,
                        help="Note file stem filter (e.g. taro). Omit to test all.")
    args = parser.parse_args()

    if args.note:
        candidates = sorted(NOTE_DIR.glob(f"*{args.note}*.note"))
        if not candidates:
            print(f"No note file matching '{args.note}' in {NOTE_DIR}")
            sys.exit(1)
    else:
        candidates = sorted(NOTE_DIR.glob("*.note"))

    print(f"Testing {len(candidates)} file(s): {', '.join(p.stem for p in candidates)}")

    from playwright.sync_api import sync_playwright
    from compare import ensure_playwright_browsers
    ensure_playwright_browsers()

    server, port = _start_http_server(WEB_DIR)
    all_ok = True
    results = []

    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)

            # Shared headless page for rendering
            page_headless = browser.new_context().new_page()
            page_headless.goto(f"http://127.0.0.1:{port}/headless.html",
                               wait_until="networkidle")
            page_headless.wait_for_function("window.ready === true", timeout=15000)

            # Shared index page for import
            page_index = browser.new_context().new_page()
            console_msgs = []
            page_index.on("console",
                          lambda msg: console_msgs.append(f"[{msg.type}] {msg.text}"))
            page_index.on("pageerror",
                          lambda err: console_msgs.append(f"[pageerror] {err}"))
            page_index.goto(f"http://127.0.0.1:{port}/index.html",
                            wait_until="networkidle")
            page_index.wait_for_function(
                "typeof window._testImportSVG === 'function'", timeout=15000)

            for note_path in candidates:
                try:
                    ok, mae = test_one_note(note_path, page_headless, page_index, port)
                    results.append((note_path.stem, ok, mae))
                    if not ok:
                        all_ok = False
                except Exception as e:
                    print(f"  {note_path.stem}: ERROR — {e}")
                    results.append((note_path.stem, False, None))
                    all_ok = False

            # Print console errors
            for m in console_msgs:
                if m.startswith("[error]") or m.startswith("[pageerror]"):
                    print(f"    {m}")

            page_headless.close()
            page_index.close()
            browser.close()
    finally:
        server.shutdown()

    # Summary
    print(f"\n{'='*60}")
    passed = sum(1 for _, ok, _ in results if ok)
    maes = [m for _, _, m in results if m is not None]
    avg_mae = sum(maes) / len(maes) if maes else 0
    print(f"  {passed}/{len(results)} passed  avg MAE={avg_mae:.1f}")
    if all_ok:
        print("  All checks passed.")
    else:
        print("  Some checks FAILED.")
        sys.exit(1)


if __name__ == "__main__":
    main()
