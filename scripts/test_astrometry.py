#!/usr/bin/env python3
"""
AstroPi astrometry pipeline smoke test.

Tests every layer of the plate-solving stack so you can verify the setup
is correct after deploying to the Pi.

Usage:
    # Basic (synthetic star field — no real image needed):
    python3 scripts/test_astrometry.py

    # Full end-to-end with a real RAW image:
    python3 scripts/test_astrometry.py --image path/to/image.cr3 \
                                        --ra 83.82 --dec 22.01 \
                                        --tolerance 0.5

Exit code 0 = all tests passed.
Exit code 1 = one or more tests failed.
"""

import sys
import json
import math
import argparse
import subprocess
import tempfile
import pathlib
import time

PASS = "\033[92m[PASS]\033[0m"
FAIL = "\033[91m[FAIL]\033[0m"
SKIP = "\033[93m[SKIP]\033[0m"
INFO = "\033[94m[INFO]\033[0m"

failures = []

def report(label, ok, detail=""):
    tag = PASS if ok else FAIL
    suffix = f"  {detail}" if detail else ""
    print(f"  {tag} {label}{suffix}")
    if not ok:
        failures.append(label)

# ---------------------------------------------------------------------------
# 1. Python dependency checks
# ---------------------------------------------------------------------------
def test_imports():
    print("\n[1/5] Python dependency checks")
    deps = ["numpy", "rawpy", "scipy.ndimage", "astrometry"]
    for dep in deps:
        try:
            __import__(dep)
            report(f"import {dep}", True)
        except ImportError as e:
            report(f"import {dep}", False, str(e))

# ---------------------------------------------------------------------------
# 2. Index file check (existence only — does NOT download)
# ---------------------------------------------------------------------------
def test_index_files():
    print("\n[2/5] Astrometry.net index files  (scales 5, 6, 7 of series_4200)")
    cache = pathlib.Path.home() / ".cache" / "astrometry" / "4200"
    # Expect at least one .fits file per scale group (12 files × 3 scales = 36 total)
    fits_files = list(cache.glob("*.fits")) if cache.exists() else []
    if not fits_files:
        print(f"  {SKIP} no index files in {cache}")
        print(f"  {INFO} run the download first:")
        print(f"  {INFO}   python3 - <<'EOF'")
        print(f"  {INFO}   import astrometry, pathlib")
        print(f"  {INFO}   astrometry.series_4200.index_files(pathlib.Path.home()/'.cache'/'astrometry', scales={{5,6,7}})")
        print(f"  {INFO}   EOF")
        return

    scale_counts = {}
    for f in fits_files:
        # filenames look like index-4205-01.fits  →  scale = int(name[8:10])
        try:
            scale = int(f.stem[8:10])
            scale_counts[scale] = scale_counts.get(scale, 0) + 1
        except (ValueError, IndexError):
            pass

    for scale in [5, 6, 7]:
        n = scale_counts.get(scale, 0)
        report(
            f"scale {scale}: {n}/12 index files present",
            n == 12,
            f"({'OK' if n == 12 else f'expected 12, found {n}'})",
        )

# ---------------------------------------------------------------------------
# 3. Star extraction on a synthetic image
# ---------------------------------------------------------------------------
def test_star_extraction_synthetic():
    print("\n[3/5] Star extraction — synthetic image")
    try:
        import numpy as np
        from scipy import ndimage

        # Build a 512×512 blank image with 20 point-source stars
        img = np.zeros((512, 512), dtype=np.float32)
        expected_centroids = set()
        rng = np.random.default_rng(42)
        for _ in range(20):
            cx = int(rng.integers(20, 492))
            cy = int(rng.integers(20, 492))
            img[cy-1:cy+2, cx-1:cx+2] += 0.5
            img[cy, cx] += 0.5  # brighter centre pixel
            expected_centroids.add((cx, cy))

        # Run the same logic as _extract_stars_from_raw
        vmin, vmax = np.percentile(img, [1.0, 99.9])
        if vmax > vmin:
            norm = np.clip((img - vmin) / (vmax - vmin), 0.0, 1.0)
        else:
            norm = img

        threshold = 0.15
        labeled, nf = ndimage.label(norm > threshold)
        centroids = ndimage.center_of_mass(norm, labeled, range(1, nf + 1))
        sizes = ndimage.sum(norm > threshold, labeled, range(1, nf + 1))
        peaks  = ndimage.maximum(norm, labeled, range(1, nf + 1))

        stars = [
            (float(cx), float(cy))
            for (cy, cx), sz, pk in zip(centroids, sizes, peaks)
            if sz >= 5 and pk > threshold
        ]

        report(
            f"detected {len(stars)} stars in synthetic image",
            len(stars) >= 15,
            f"(expected ≥15 of 20 injected)",
        )
    except Exception as e:
        report("synthetic star extraction", False, str(e))

# ---------------------------------------------------------------------------
# 4. Star extraction on a real RAW image (optional)
# ---------------------------------------------------------------------------
def test_star_extraction_real(image_path: str):
    print(f"\n[4/5] Star extraction — real image  ({image_path})")
    if not pathlib.Path(image_path).exists():
        print(f"  {SKIP} image not found, skipping")
        return

    try:
        import rawpy
        import numpy as np
        from scipy import ndimage

        with rawpy.imread(image_path) as raw:
            gray = raw.raw_image_visible.astype(np.float32)  # full Bayer image
            threshold_value = 0.15 * float(raw.white_level)

        labeled, nf = ndimage.label(gray > threshold_value)
        sizes = ndimage.sum((gray > threshold_value).astype(np.uint8), labeled, range(1, nf + 1))

        stars = [i for i, sz in enumerate(sizes) if sz >= 5]
        n = min(len(stars), 200)

        report(
            f"detected ≥10 stars in real image",
            n >= 10,
            f"({n} stars detected, image {gray.shape[1]}×{gray.shape[0]} px)",
        )
    except Exception as e:
        report("real image star extraction", False, str(e))

# ---------------------------------------------------------------------------
# 5. Full end-to-end solve via raw_tools.py
# ---------------------------------------------------------------------------
def test_full_solve(image_path: str, ra_deg: float, dec_deg: float, tolerance_deg: float):
    print(f"\n[5/5] Full astrometry solve  ({pathlib.Path(image_path).name})")
    if not pathlib.Path(image_path).exists():
        print(f"  {SKIP} image not found, skipping full solve")
        return

    # Pixel scale: 1.733 arcsec/pix nominal for 714 mm focal length + 6 µm pixels
    scale_low  = 0.7
    scale_high = 3.5
    radius_deg = 15.0

    script = pathlib.Path(__file__).parent / "raw_tools.py"
    cmd = [
        sys.executable, str(script),
        "solve",
        image_path,
        f"{ra_deg:.4f}",
        f"{dec_deg:.4f}",
        f"{scale_low:.2f}",
        f"{scale_high:.2f}",
        f"{radius_deg:.1f}",
    ]

    print(f"  {INFO} running: {' '.join(cmd)}")
    t0 = time.time()
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
    except subprocess.TimeoutExpired:
        report("solve completed within 5 minutes", False, "timeout")
        return
    elapsed = time.time() - t0

    if result.returncode != 0:
        report("solve subprocess exit code", False, f"rc={result.returncode}\n{result.stderr[:400]}")
        return

    stdout = result.stdout.strip()
    try:
        data = json.loads(stdout)
    except json.JSONDecodeError:
        report("solve JSON output", False, f"could not parse: {stdout[:200]}")
        return

    report("solve returned JSON", True, f"({elapsed:.1f}s)")

    if not data.get("success"):
        report("solve found a solution", False, data.get("error", "no error field"))
        return

    report("solve found a solution", True)

    solved_ra  = data["ra_deg"]
    solved_dec = data["dec_deg"]
    scale      = data["scale_arcsec_per_pixel"]
    logodds    = data.get("logodds", float("nan"))

    # Angular separation (haversine)
    dra  = math.radians(solved_ra  - ra_deg)
    ddec = math.radians(solved_dec - dec_deg)
    a = math.sin(ddec / 2) ** 2 + math.cos(math.radians(dec_deg)) * math.cos(math.radians(solved_dec)) * math.sin(dra / 2) ** 2
    separation_deg = math.degrees(2 * math.asin(math.sqrt(a)))

    print(f"  {INFO} solved  RA={solved_ra:.4f}°  Dec={solved_dec:.4f}°")
    print(f"  {INFO} expected RA={ra_deg:.4f}°  Dec={dec_deg:.4f}°")
    print(f"  {INFO} separation={separation_deg*60:.1f}'  scale={scale:.3f} arcsec/pix  logodds={logodds:.1f}")

    report(
        f"position within {tolerance_deg*60:.0f}' of expected",
        separation_deg <= tolerance_deg,
        f"(actual separation {separation_deg*60:.1f}')",
    )
    report(
        "pixel scale plausible (0.5–5 arcsec/pix)",
        0.5 <= scale <= 5.0,
        f"({scale:.3f} arcsec/pix)",
    )

# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------
def main():
    parser = argparse.ArgumentParser(description="AstroPi astrometry pipeline test")
    parser.add_argument("--image",
        default="AstroPI_PlateSolving/test_img/M101.CR3",
        help="Path to a RAW image to use for the full solve test")
    parser.add_argument("--ra",   type=float, default=210.8023,
        help="Expected RA of the image centre in degrees (default: M101=210.80)")
    parser.add_argument("--dec",  type=float, default=54.3492,
        help="Expected Dec of the image centre in degrees (default: M101=+54.35)")
    parser.add_argument("--tolerance", type=float, default=0.5,
        help="Acceptable position error in degrees (default: 0.5)")
    args = parser.parse_args()

    print("=" * 60)
    print("AstroPi astrometry pipeline test")
    print("=" * 60)

    test_imports()
    test_index_files()
    test_star_extraction_synthetic()
    test_star_extraction_real(args.image)
    test_full_solve(args.image, args.ra, args.dec, args.tolerance)

    print("\n" + "=" * 60)
    if failures:
        print(f"\033[91mFAILED\033[0m  {len(failures)} test(s) failed:")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    else:
        print(f"\033[92mALL TESTS PASSED\033[0m")
        sys.exit(0)


if __name__ == "__main__":
    main()
