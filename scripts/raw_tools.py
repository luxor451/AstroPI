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
    else:
        print(f'Unknown command: {cmd}', file=sys.stderr)
        sys.exit(1)
