#!/usr/bin/env python3
"""RAW/FITS utilities for AstroPI gallery.

Usage:
  raw_tools.py thumbnail <input> <output> [max_size]
  raw_tools.py preview   <input> <output> [stretch_percent]
  raw_tools.py fits      <input> <output>
  raw_tools.py fitshdr   <input>             # print FITS header as JSON
"""

import sys
import json
import numpy as np
from pathlib import Path


def _stretch(data: np.ndarray, lo_pct: float = 0.1, hi_pct: float = 99.9) -> np.ndarray:
    """Linear percentile stretch to 0-255 uint8."""
    lo, hi = np.percentile(data, [lo_pct, hi_pct])
    if hi <= lo:
        hi = lo + 1
    data = np.clip(data, lo, hi)
    data = ((data - lo) / (hi - lo) * 255.0).astype(np.uint8)
    return data


def _raw_to_pil(input_path: str, half_size: bool = False):
    """Open a RAW file and return a PIL Image (RGB)."""
    import rawpy
    from PIL import Image
    with rawpy.imread(input_path) as raw:
        rgb = raw.postprocess(
            use_camera_wb=True,
            half_size=half_size,
            no_auto_bright=False,
            output_bps=8,
        )
    return Image.fromarray(rgb)


def _fits_to_pil(input_path: str, stretch_lo: float = 0.1, stretch_hi: float = 99.9):
    """Open a FITS file and return a PIL Image (grayscale, stretched)."""
    from astropy.io import fits
    from PIL import Image
    with fits.open(input_path) as hdul:
        data = hdul[0].data.astype(np.float32)
    if data.ndim == 3:
        data = data[0]  # take first plane if cube
    stretched = _stretch(data, stretch_lo, stretch_hi)
    return Image.fromarray(stretched, mode='L')


def cmd_thumbnail(input_path: str, output_path: str, max_size: int = 400):
    """Generate a small JPEG thumbnail."""
    import rawpy
    from PIL import Image
    import io

    ext = Path(input_path).suffix.lower()

    if ext in ('.fits', '.fit'):
        img = _fits_to_pil(input_path)
    else:
        # Try embedded thumbnail first (fast)
        try:
            with rawpy.imread(input_path) as raw:
                thumb = raw.extract_thumb()
            if thumb.format == rawpy.ThumbFormat.JPEG:
                img = Image.open(io.BytesIO(thumb.data))
            else:
                img = Image.fromarray(thumb.data)
        except Exception:
            img = _raw_to_pil(input_path, half_size=True)

    img.thumbnail((max_size, max_size))
    img.save(output_path, 'JPEG', quality=80)


def cmd_preview(input_path: str, output_path: str,
                stretch_lo: float = 0.1, stretch_hi: float = 99.9):
    """Generate a full-resolution JPEG preview with stretch."""
    ext = Path(input_path).suffix.lower()

    if ext in ('.fits', '.fit'):
        img = _fits_to_pil(input_path, stretch_lo, stretch_hi)
    else:
        img = _raw_to_pil(input_path, half_size=False)

    img.save(output_path, 'JPEG', quality=90)


def cmd_fits(input_path: str, output_path: str,
             object_name: str = '',
             ra_deg: float = None, dec_deg: float = None):
    """Convert a RAW file to FITS (raw Bayer CFA data) with optional mount position."""
    import rawpy
    from astropy.io import fits

    with rawpy.imread(input_path) as raw:
        bayer = raw.raw_image_visible.astype(np.uint16)
        color_desc = raw.color_desc.decode('ascii', errors='replace')

    hdu = fits.PrimaryHDU(bayer)
    h = hdu.header
    h['INSTRUME'] = ('DSLR', 'Camera instrument')
    h['BAYERPAT'] = (color_desc, 'Bayer color pattern')
    h['NAXIS1']   = bayer.shape[1]
    h['NAXIS2']   = bayer.shape[0]
    h['XBINNING'] = (1, 'Binning X')
    h['YBINNING'] = (1, 'Binning Y')
    if object_name:
        h['OBJECT'] = (object_name, 'Target object name')
    if ra_deg is not None:
        # RA in degrees (FITS standard) and hours (OBJCTRA)
        ra_h = ra_deg / 15.0
        ra_hms = '{:02.0f}:{:02.0f}:{:05.2f}'.format(
            int(ra_h), int((ra_h % 1) * 60), ((ra_h * 60) % 1) * 60)
        h['RA']      = (ra_deg, '[deg] Mount RA (J2000)')
        h['OBJCTRA'] = (ra_hms, 'Mount RA in HH:MM:SS')
    if dec_deg is not None:
        sign = '+' if dec_deg >= 0 else '-'
        adec = abs(dec_deg)
        dec_dms = '{}{:02.0f}:{:02.0f}:{:04.1f}'.format(
            sign, int(adec), int((adec % 1) * 60), ((adec * 60) % 1) * 60)
        h['DEC']      = (dec_deg, '[deg] Mount Dec (J2000)')
        h['OBJCTDEC'] = (dec_dms, 'Mount Dec in DD:MM:SS')
    h['COMMENT'] = f'Converted from {Path(input_path).name} by AstroPI'
    fits.HDUList([hdu]).writeto(output_path, overwrite=True)


def cmd_fitshdr(input_path: str):
    """Print FITS header as JSON to stdout."""
    from astropy.io import fits
    result = {}
    with fits.open(input_path) as hdul:
        for card in hdul[0].header.cards:
            if card.keyword:
                result[card.keyword] = str(card.value)
    print(json.dumps(result))


def _extract_stars_from_raw(image_path: str, max_stars: int = 200):
    """Extract star pixel positions from a RAW (or FITS) image.

    Uses the same threshold logic as the Rust plate solver:
      threshold = 15% of white_level (the raw integer value, no BL subtraction).

    Returns a list of (x, y) tuples in pixel coordinates, brightest first.
    """
    import rawpy
    from scipy import ndimage

    ext = Path(image_path).suffix.lower()

    if ext in ('.fits', '.fit'):
        from astropy.io import fits as astrofits
        with astrofits.open(image_path) as hdul:
            data = hdul[0].data.astype(np.float32)
        if data.ndim == 3:
            data = data[0]
        gray = data
        # For FITS, use percentile-based threshold (no concept of white_level)
        threshold_value = float(np.percentile(gray, 99.5))
    else:
        with rawpy.imread(image_path) as raw:
            # Use the full Bayer image (all channels) — matches Rust get_pixel_matrix_from_dng
            gray = raw.raw_image_visible.astype(np.float32)
            # Match Rust: threshold at 15% of white_level (no black-level subtraction)
            white_level = float(raw.white_level)
        threshold_value = 0.15 * white_level

    binary = gray > threshold_value

    labeled, num_features = ndimage.label(binary)
    if num_features == 0:
        return []

    # Compute centroid and peak brightness for each blob
    centroids = ndimage.center_of_mass(gray, labeled, range(1, num_features + 1))
    peaks = ndimage.maximum(gray, labeled, range(1, num_features + 1))

    # Filter: blobs must have >= 5 pixels (same as Rust MIN_STAR_PIXELS)
    binary_int = binary.astype(np.uint8)
    sizes = ndimage.sum(binary_int, labeled, range(1, num_features + 1))

    stars = []
    for i, (cy, cx) in enumerate(centroids):
        if sizes[i] >= 5:
            stars.append((float(cx), float(cy), float(peaks[i])))

    # Sort by brightness descending, return top max_stars
    stars.sort(key=lambda s: s[2], reverse=True)
    return [(s[0], s[1]) for s in stars[:max_stars]]


def cmd_solve(image_path: str, ra_hint_deg: float, dec_hint_deg: float,
              pixel_scale_low: float, pixel_scale_high: float,
              search_radius_deg: float = 10.0,
              index_cache_dir: str = None):
    """Plate-solve an image using astrometry.net (Python package).

    Outputs a JSON object on stdout:
      {"success": true, "ra_deg": ..., "dec_deg": ...,
       "scale_arcsec_per_pixel": ..., "logodds": ...}
    or
      {"success": false, "error": "..."}
    """
    import astrometry

    if index_cache_dir is None:
        index_cache_dir = Path.home() / '.cache' / 'astrometry'

    # Select scales that overlap with the provided pixel-scale range.
    # series_4200 scale k roughly covers 2^(k/2)..2^((k+2)/2) arcsec/pix.
    # We use scales 5-7 which cover ~1.7-7 arcsec/pix – broad enough for
    # typical DSLR setups.
    scales = {5, 6, 7}

    try:
        index_files = astrometry.series_4200.index_files(
            cache_directory=index_cache_dir,
            scales=scales,
        )
    except Exception as e:
        print(json.dumps({"success": False, "error": f"Failed to load index files: {e}"}))
        sys.exit(0)

    if not index_files:
        print(json.dumps({"success": False, "error": "No index files available"}))
        sys.exit(0)

    # Extract stars from the image
    try:
        stars = _extract_stars_from_raw(image_path)
    except Exception as e:
        print(json.dumps({"success": False, "error": f"Star extraction failed: {e}"}))
        sys.exit(0)

    if len(stars) < 5:
        print(json.dumps({"success": False,
                          "error": f"Too few stars detected ({len(stars)}), need at least 5"}))
        sys.exit(0)

    try:
        with astrometry.Solver(index_files) as solver:
            solution = solver.solve(
                stars=stars,
                size_hint=astrometry.SizeHint(
                    lower_arcsec_per_pixel=pixel_scale_low,
                    upper_arcsec_per_pixel=pixel_scale_high,
                ),
                position_hint=astrometry.PositionHint(
                    ra_deg=ra_hint_deg,
                    dec_deg=dec_hint_deg,
                    radius_deg=search_radius_deg,
                ),
                solution_parameters=astrometry.SolutionParameters(
                    logodds_callback=lambda logodds: (
                        astrometry.Action.STOP if logodds > 40 else astrometry.Action.CONTINUE
                    ),
                ),
            )
    except Exception as e:
        print(json.dumps({"success": False, "error": f"Solver error: {e}"}))
        sys.exit(0)

    if solution.has_match():
        m = solution.best_match()
        print(json.dumps({
            "success": True,
            "ra_deg": m.center_ra_deg,
            "dec_deg": m.center_dec_deg,
            "scale_arcsec_per_pixel": m.scale_arcsec_per_pixel,
            "logodds": m.logodds,
        }))
    else:
        print(json.dumps({"success": False, "error": "No solution found"}))


if __name__ == '__main__':
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)

    cmd = sys.argv[1]
    if cmd == 'thumbnail':
        max_s = int(sys.argv[4]) if len(sys.argv) > 4 else 400
        cmd_thumbnail(sys.argv[2], sys.argv[3], max_s)
    elif cmd == 'preview':
        lo = float(sys.argv[4]) if len(sys.argv) > 4 else 0.1
        hi = float(sys.argv[5]) if len(sys.argv) > 5 else 99.9
        cmd_preview(sys.argv[2], sys.argv[3], lo, hi)
    elif cmd == 'fits':
        obj  = sys.argv[4] if len(sys.argv) > 4 else ''
        ra   = float(sys.argv[5]) if len(sys.argv) > 5 else None
        dec  = float(sys.argv[6]) if len(sys.argv) > 6 else None
        cmd_fits(sys.argv[2], sys.argv[3], obj, ra, dec)
    elif cmd == 'fitshdr':
        cmd_fitshdr(sys.argv[2])
    elif cmd == 'solve':
        # solve <image> <ra_deg> <dec_deg> <scale_low> <scale_high> [radius_deg] [cache_dir]
        if len(sys.argv) < 7:
            print('Usage: raw_tools.py solve <image> <ra_deg> <dec_deg> <scale_low_arcsec_per_pix> <scale_high_arcsec_per_pix> [radius_deg] [cache_dir]',
                  file=sys.stderr)
            sys.exit(1)
        radius = float(sys.argv[7]) if len(sys.argv) > 7 else 10.0
        cache  = sys.argv[8]        if len(sys.argv) > 8 else None
        cmd_solve(
            image_path=sys.argv[2],
            ra_hint_deg=float(sys.argv[3]),
            dec_hint_deg=float(sys.argv[4]),
            pixel_scale_low=float(sys.argv[5]),
            pixel_scale_high=float(sys.argv[6]),
            search_radius_deg=radius,
            index_cache_dir=cache,
        )
    else:
        print(f'Unknown command: {cmd}', file=sys.stderr)
        sys.exit(1)
