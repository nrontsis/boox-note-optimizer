"""Test SVG import: download W3C sample SVGs, import via WASM, compare against native render.

Downloads SVGs from https://dev.w3.org/SVG/tools/svgweb/samples/svg-files/
Renders each natively in the browser (reference) and via the import pipeline (ours).
Compares the two and reports differences.

Usage:
    uv run test_svg_import.py                  # all default test SVGs
    uv run test_svg_import.py heart star bozo  # specific files
    uv run test_svg_import.py --list           # list available SVGs
"""
import argparse
import base64
import http.server
import io
import json
import sys
import threading
import time
import urllib.request
from datetime import datetime
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

SCRIPT_DIR = Path(__file__).resolve().parent
WEB_DIR = SCRIPT_DIR / "web"
SVG_CACHE_DIR = SCRIPT_DIR / "gold-pairs" / ".svg-cache"
OUTPUT_BASE = SCRIPT_DIR / "gold-pairs" / "svg-test-output"

SVG_BASE_URL = "https://dev.w3.org/SVG/tools/svgweb/samples/svg-files"

# Curated test set: simple → medium → complex
# Each tuple: (name, expected_difficulty)
#   "simple" = basic shapes/paths only
#   "medium" = transforms, groups, multiple shapes
#   "complex" = gradients, filters, text, CSS (expected partial/skip)
TEST_SVGS = [
    # Simple: basic paths and shapes
    ("heart", "simple"),
    ("star", "simple"),
    ("check", "simple"),
    ("yinyang", "simple"),
    # smile uses XML entities (<!ENTITY>) which aren't supported by innerHTML
    # circles1 uses CSS styling + no viewBox, breaking the reference renderer
    ("italian-flag", "simple"),
    ("rectangles", "simple"),
    # Medium: transforms, groups, compound shapes
    ("bozo", "medium"),
    ("android", "medium"),
    ("caution", "medium"),
    ("atom", "medium"),
    ("snake", "medium"),
    ("shapes-polygon-01-t", "medium"),
    ("shapes-polyline-01-t", "medium"),
    ("paths-data-08-t", "medium"),
    ("paths-data-09-t", "medium"),
    # Complex: gradients, filters, text, CSS
    ("cartman", "complex"),
    ("car", "complex"),
    ("duck", "complex"),
    ("compass", "complex"),
    ("helloworld", "complex"),
    ("tiger", "complex"),
]


def download_svg(name: str) -> str:
    """Download an SVG file, using cache."""
    SVG_CACHE_DIR.mkdir(parents=True, exist_ok=True)
    cache_path = SVG_CACHE_DIR / f"{name}.svg"

    if cache_path.exists():
        return cache_path.read_text()

    url = f"{SVG_BASE_URL}/{name}.svg"
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
        with urllib.request.urlopen(req, timeout=10) as resp:
            svg_text = resp.read().decode("utf-8")
        cache_path.write_text(svg_text)
        return svg_text
    except Exception as e:
        print(f"  SKIP {name}: download failed ({e})")
        return ""


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


def data_url_to_image(data_url: str) -> Image.Image:
    png_bytes = base64.b64decode(data_url.split(",", 1)[1])
    return Image.open(io.BytesIO(png_bytes)).convert("RGBA")


def compare_images(ref_img: Image.Image, imp_img: Image.Image) -> dict:
    """Compare reference vs imported render. Returns metrics."""
    ref = np.array(ref_img, dtype=np.int16)
    imp = np.array(imp_img, dtype=np.int16)

    # Resize if needed
    if ref.shape != imp.shape:
        imp_img = imp_img.resize((ref.shape[1], ref.shape[0]), Image.LANCZOS)
        imp = np.array(imp_img, dtype=np.int16)

    diff = np.abs(ref[:, :, :3] - imp[:, :, :3])
    mae = float(diff.mean())
    max_diff = int(diff.max())

    # Non-white pixels in each (content coverage)
    ref_content = np.any(ref[:, :, :3] < 250, axis=2).sum()
    imp_content = np.any(imp[:, :, :3] < 250, axis=2).sum()

    # Pixels where diff > 30 (significant difference)
    sig_diff = int((diff.max(axis=2) > 30).sum())
    total = ref.shape[0] * ref.shape[1]

    return {
        "mae": mae,
        "max_diff": max_diff,
        "sig_diff_pixels": sig_diff,
        "sig_diff_pct": sig_diff / total * 100,
        "ref_content_pixels": int(ref_content),
        "imp_content_pixels": int(imp_content),
        "content_ratio": int(imp_content) / max(1, int(ref_content)),
    }


def save_comparison(out_dir: Path, name: str, ref_img: Image.Image,
                    imp_img: Image.Image, metrics: dict):
    """Save side-by-side comparison images."""
    # Resize imported to match reference
    if ref_img.size != imp_img.size:
        imp_img = imp_img.resize(ref_img.size, Image.LANCZOS)

    ref_img.save(out_dir / f"{name}_ref.png")
    imp_img.save(out_dir / f"{name}_imp.png")

    # Diff visualization
    ref = np.array(ref_img, dtype=np.int16)
    imp = np.array(imp_img, dtype=np.int16)
    diff = np.abs(ref[:, :, :3] - imp[:, :, :3])
    diff_vis = np.clip(diff.astype(np.float32) * 4, 0, 255).astype(np.uint8)
    # Add alpha channel
    alpha = np.full((*diff_vis.shape[:2], 1), 255, dtype=np.uint8)
    diff_rgba = np.concatenate([diff_vis, alpha], axis=2)
    Image.fromarray(diff_rgba).save(out_dir / f"{name}_diff.png")


def render_svg_reference(page, svg_text: str, width: int, height: int, bbox: dict | None) -> str:
    """Render SVG natively in the browser as a reference image.
    Uses bbox-based scaling to match the import pipeline's positioning."""
    return page.evaluate("""([svgText, width, height, bbox]) => {
        return new Promise((resolve, reject) => {
            const c = document.createElement('canvas');
            c.width = width; c.height = height;
            const ctx = c.getContext('2d');
            ctx.fillStyle = 'white';
            ctx.fillRect(0, 0, width, height);
            const parser = new DOMParser();
            const doc = parser.parseFromString(svgText, 'image/svg+xml');
            const svg = doc.documentElement;
            let drawX = 0, drawY = 0;
            if (bbox) {
                const padding = 100;
                const scale = Math.min((width - padding*2) / bbox.width, (height - padding*2) / bbox.height);
                const drawW = bbox.width * scale, drawH = bbox.height * scale;
                svg.setAttribute('viewBox', `${bbox.x} ${bbox.y} ${bbox.width} ${bbox.height}`);
                svg.setAttribute('width', drawW);
                svg.setAttribute('height', drawH);
                svg.setAttribute('preserveAspectRatio', 'none');
                drawX = padding; drawY = padding;
            } else {
                svg.setAttribute('width', width);
                svg.setAttribute('height', height);
            }
            const modified = new XMLSerializer().serializeToString(svg);
            const blob = new Blob([modified], { type: 'image/svg+xml' });
            const url = URL.createObjectURL(blob);
            const img = new Image();
            img.onload = () => { ctx.drawImage(img, drawX, drawY); URL.revokeObjectURL(url); resolve(c.toDataURL('image/png')); };
            img.onerror = () => { URL.revokeObjectURL(url); reject('Failed to load SVG as image'); };
            img.src = url;
        });
    }""", [svg_text, width, height, bbox])


def get_svg_bbox(page, svg_text: str) -> dict | None:
    """Get getBBox of SVG content (matches import pipeline's scaling basis)."""
    return page.evaluate("""(svgText) => {
        const div = document.createElement('div');
        div.innerHTML = svgText.replace(/<script\\b[^<]*(?:(?!<\\/script>)<[^<]*)*<\\/script>/gi, '');
        const svgEl = div.querySelector('svg');
        if (!svgEl) return null;
        document.body.appendChild(div);
        div.style.cssText = 'position:absolute;visibility:hidden;top:-9999px';
        try {
            const bbox = svgEl.getBBox();
            document.body.removeChild(div);
            if (bbox.width > 0 && bbox.height > 0)
                return { x: bbox.x, y: bbox.y, width: bbox.width, height: bbox.height };
        } catch(e) {}
        document.body.removeChild(div);
        return null;
    }""", svg_text)


def run_tests(names: list[tuple[str, str]], out_dir: Path) -> list[dict]:
    """Run SVG import tests via the actual app (index.html)."""
    from playwright.sync_api import sync_playwright
    from compare import ensure_playwright_browsers

    ensure_playwright_browsers()

    svgs = {}
    for name, difficulty in names:
        svg_text = download_svg(name)
        if svg_text:
            svgs[name] = (svg_text, difficulty)

    if not svgs:
        print("No SVGs to test.")
        return []

    server, port = _start_http_server(WEB_DIR)
    results = []

    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_context().new_page()
            # Load the actual app — tests exercise the real import pipeline
            page.goto(f"http://127.0.0.1:{port}/index.html", wait_until="networkidle")
            page.wait_for_function("typeof window._testImportSVG === 'function'", timeout=15000)

            for name, (svg_text, difficulty) in svgs.items():
                print(f"  {name} ({difficulty})...", end="", flush=True)

                try:
                    imp_result = page.evaluate(
                        "(svg) => window._testImportSVG(svg)", svg_text)
                except Exception as e:
                    print(f" ERROR: {e}")
                    results.append({
                        "name": name, "difficulty": difficulty,
                        "status": "error", "error": str(e), "stroke_count": 0,
                    })
                    continue

                if isinstance(imp_result, dict) and imp_result.get("error"):
                    print(f" ERROR: {imp_result['error']}")
                    results.append({
                        "name": name, "difficulty": difficulty,
                        "status": "error", "error": imp_result["error"],
                        "stroke_count": 0,
                    })
                    continue

                imp_img = data_url_to_image(imp_result["url"])
                W, H = imp_result["width"], imp_result["height"]

                # Canvas dims round-trip check: export .note, reload, verify dims match
                dims_ok = True
                try:
                    note_b64 = page.evaluate("() => window._testExportNote()")
                    if note_b64:
                        rt_dims = page.evaluate(
                            "async (b64) => await window._testReloadNote(b64)",
                            note_b64)
                        if abs(rt_dims["width"] - W) > 1 or abs(rt_dims["height"] - H) > 1:
                            print(f" DIMS MISMATCH: import={W}x{H}, .note={rt_dims['width']}x{rt_dims['height']}", end="")
                            dims_ok = False
                except Exception as e:
                    print(f" dims check error: {e}", end="")
                    dims_ok = False

                # Use the same bbox that the import used
                import_bbox = page.evaluate("() => window._lastSvgBBox")
                bbox = get_svg_bbox(page, svg_text)
                if import_bbox and bbox and (abs(import_bbox['width'] - bbox['width']) > 1 or abs(import_bbox['height'] - bbox['height']) > 1):
                    print(f" BBOX MISMATCH: import={import_bbox}, ref={bbox}", end="")
                    bbox = import_bbox

                try:
                    ref_url = render_svg_reference(page, svg_text, W, H, bbox)
                    ref_img = data_url_to_image(ref_url)
                except Exception as e:
                    print(f" ref render failed: {e}")
                    imp_img.save(out_dir / f"{name}_imp.png")
                    results.append({
                        "name": name, "difficulty": difficulty,
                        "status": "ref_failed", "stroke_count": 0,
                    })
                    continue

                metrics = compare_images(ref_img, imp_img)
                metrics["name"] = name
                metrics["difficulty"] = difficulty
                metrics["status"] = "ok"
                metrics["dims_ok"] = dims_ok

                save_comparison(out_dir, name, ref_img, imp_img, metrics)

                status = "PASS" if metrics["mae"] < 15 and dims_ok else "WARN" if metrics["mae"] < 30 else "FAIL"
                if not dims_ok:
                    status = "FAIL(dims)"
                print(f" MAE={metrics['mae']:.1f}, "
                      f"content={metrics['content_ratio']:.2f}x → {status}")

                results.append(metrics)

            browser.close()
    finally:
        server.shutdown()

    return results


def print_summary(results: list[dict]):
    """Print results table."""
    print()
    print(f"{'Name':<25} {'Diff':>8} {'MAE':>7} {'SigDiff%':>8} {'Content':>8} {'Status':>6}")
    print("-" * 68)

    for r in results:
        if r["status"] in ("error", "ref_failed"):
            print(f"{r['name']:<25} {r['difficulty']:>8} "
                  f"{'':>7} {'':>8} {'':>8} {r['status'].upper():>6}")
            if r.get("error"):
                print(f"  → {r['error']}")
            continue

        status = "PASS" if r["mae"] < 15 else "WARN" if r["mae"] < 30 else "FAIL"
        print(f"{r['name']:<25} {r['difficulty']:>8} "
              f"{r['mae']:>7.1f} {r['sig_diff_pct']:>7.2f}% "
              f"{r['content_ratio']:>7.2f}x {status:>6}")

    print("-" * 75)
    ok = [r for r in results if r["status"] == "ok"]
    if ok:
        avg_mae = np.mean([r["mae"] for r in ok])
        n_pass = sum(1 for r in ok if r["mae"] < 15)
        n_warn = sum(1 for r in ok if 15 <= r["mae"] < 30)
        n_fail = sum(1 for r in ok if r["mae"] >= 30)
        print(f"  {len(ok)} tested: {n_pass} PASS, {n_warn} WARN, {n_fail} FAIL  "
              f"(avg MAE={avg_mae:.1f})")
    n_err = sum(1 for r in results if r["status"] in ("error", "ref_failed"))
    if n_err:
        print(f"  {n_err} error/skipped")
    print()


def main():
    parser = argparse.ArgumentParser(description="Test SVG import pipeline")
    parser.add_argument("files", nargs="*", help="SVG names to test (without .svg)")
    parser.add_argument("--list", action="store_true", help="List available test SVGs")
    args = parser.parse_args()

    if args.list:
        for name, diff in TEST_SVGS:
            print(f"  {name:<30} {diff}")
        return

    wasm_path = WEB_DIR / "pkg" / "boox_optimizer_bg.wasm"
    if not wasm_path.exists():
        print(f"ERROR: WASM not found at {wasm_path}. Run wasm-pack build first.")
        sys.exit(1)

    if args.files:
        names = [(f, "unknown") for f in args.files]
    else:
        names = TEST_SVGS

    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    out_dir = OUTPUT_BASE / ts
    out_dir.mkdir(parents=True, exist_ok=True)

    latest = OUTPUT_BASE / "latest"
    if latest.is_symlink() or latest.exists():
        latest.unlink()
    latest.symlink_to(ts)

    print(f"Testing {len(names)} SVG(s)")
    print(f"Output: {out_dir}")

    t0 = time.time()
    results = run_tests(names, out_dir)
    elapsed = time.time() - t0

    print(f"\nCompleted in {elapsed:.1f}s")
    print_summary(results)

    # Save JSON summary
    clean = [{k: v for k, v in r.items() if not isinstance(v, np.floating)}
             for r in results]
    for r in clean:
        for k, v in list(r.items()):
            if isinstance(v, (np.floating, np.integer)):
                r[k] = float(v) if isinstance(v, np.floating) else int(v)
    (out_dir / "summary.json").write_text(json.dumps(clean, indent=2, default=str))
    print(f"Output: {out_dir}")


if __name__ == "__main__":
    main()
