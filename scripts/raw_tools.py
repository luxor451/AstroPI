#!/usr/bin/env python3
"""RAW/FITS utilities for AstroPI gallery.

Usage:
  raw_tools.py thumbnail <input> <output> [max_size]
  raw_tools.py preview   <input> <output> [stretch_percent]
  raw_tools.py fits      <input> <output>
  raw_tools.py fitshdr   <input>             # print FITS header as JSON
  raw_tools.py tiff      <input> <output> [object] [ra_deg] [dec_deg] [focal_mm] [pixel_um] [exptime] [iso]
"""

import sys
import json
import zlib
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


def _tiff_to_pil(path: str, stretch_lo: float = 0.1, stretch_hi: float = 99.9,
                 half_size: bool = False):
    """Read our 16-bit RGB TIFF (uncompressed or DEFLATE) and return a PIL Image (8-bit, stretched).

    Uses only struct + zlib + numpy — does NOT call rawpy or PIL to open the file,
    so it works correctly on our custom TIFF that Pillow silently truncates to 8-bit.
    """
    import struct
    from PIL import Image

    with open(path, 'rb') as f:
        raw = f.read()

    # TIFF header
    bom = raw[0:2]
    endian = '<' if bom == b'II' else '>'
    if struct.unpack_from(f'{endian}H', raw, 2)[0] != 42:
        raise ValueError(f'Not a TIFF file: {path}')

    ifd_off = struct.unpack_from(f'{endian}I', raw, 4)[0]
    n_entries = struct.unpack_from(f'{endian}H', raw, ifd_off)[0]

    tags: dict = {}
    type_size = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8}
    for i in range(n_entries):
        base = ifd_off + 2 + 12 * i
        tag, typ, count = struct.unpack_from(f'{endian}HHI', raw, base)
        tsz = type_size.get(typ, 4)
        if count * tsz <= 4:
            # Value stored directly in the 4-byte field (left-justified, LE)
            val = struct.unpack_from(f'{endian}H', raw, base + 8)[0] if typ == 3 \
                  else struct.unpack_from(f'{endian}I', raw, base + 8)[0]
        else:
            off = struct.unpack_from(f'{endian}I', raw, base + 8)[0]
            val = struct.unpack_from(f'{endian}' + ('H' if typ == 3 else 'I') * count, raw, off)
        tags[tag] = val

    width           = tags[256]
    height          = tags[257]
    compression     = tags.get(259, 1)
    strip_offset    = tags[273]
    strip_bytecount = tags[279]

    payload = raw[strip_offset:strip_offset + strip_bytecount]
    if compression in (8, 32946):   # DEFLATE / Adobe Deflate
        payload = zlib.decompress(payload)
    elif compression != 1:
        raise ValueError(f'Unsupported TIFF compression={compression} in {path}')

    arr = np.frombuffer(payload, dtype=f'{endian}u2').reshape(height, width, 3).astype(np.float32)

    if half_size:
        arr = arr[::2, ::2, :]

    out = np.empty(arr.shape, dtype=np.uint8)
    for c in range(3):
        out[:, :, c] = _stretch(arr[:, :, c], stretch_lo, stretch_hi)

    return Image.fromarray(out, mode='RGB')


def cmd_thumbnail(input_path: str, output_path: str, max_size: int = 400):
    """Generate a small JPEG thumbnail."""
    import rawpy
    from PIL import Image
    import io

    ext = Path(input_path).suffix.lower()

    if ext in ('.fits', '.fit'):
        img = _fits_to_pil(input_path)
    elif ext in ('.tif', '.tiff'):
        img = _tiff_to_pil(input_path, half_size=True)
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
    elif ext in ('.tif', '.tiff'):
        img = _tiff_to_pil(input_path, stretch_lo, stretch_hi)
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


def _write_tiff_16bit_rgb(path: str, rgb: np.ndarray, description: str = '') -> None:
    """Write a 16-bit RGB TIFF using only numpy + struct + zlib (no external dependencies).

    Produces a valid TIFF 6.0, DEFLATE-compressed (tag 259=8), little-endian,
    chunky RGB with FITS-card metadata in ImageDescription so Siril can read it.
    """
    import struct

    h, w = rgb.shape[:2]
    raw_pixels = rgb.astype('<u2').tobytes()           # LE uint16, RGBRGB…
    img_data   = zlib.compress(raw_pixels, 6)          # DEFLATE level 6
    bps_block  = struct.pack('<3H', 16, 16, 16)        # BitsPerSample R/G/B
    desc_block = description.encode('ascii', errors='replace') + b'\x00'
    res_block  = struct.pack('<2I', 72, 1)             # rational 72/1 dpi (X & Y)

    # IFD entries – (tag, tiff_type, count, direct_value_or_None)
    # Types: 2=ASCII  3=SHORT(u16)  4=LONG(u32)  5=RATIONAL(2×u32)
    # None → value field will hold an offset to extra data below.
    ifd_def = [
        (256, 4, 1, w),                   # ImageWidth
        (257, 4, 1, h),                   # ImageLength
        (258, 3, 3, None),                # BitsPerSample  → bps_block
        (259, 3, 1, 8),                   # Compression    = DEFLATE
        (262, 3, 1, 2),                   # PhotometricInterp = RGB
        (270, 2, len(desc_block), None),  # ImageDescription → desc_block
        (273, 4, 1, None),                # StripOffsets   → img_data
        (277, 3, 1, 3),                   # SamplesPerPixel = 3
        (278, 4, 1, h),                   # RowsPerStrip   = whole image
        (279, 4, 1, len(img_data)),       # StripByteCounts (compressed size)
        (282, 5, 1, None),                # XResolution    → res_block
        (283, 5, 1, None),                # YResolution    → res_block
        (284, 3, 1, 1),                   # PlanarConfiguration = chunky
        (296, 3, 1, 2),                   # ResolutionUnit = inch
    ]
    n = len(ifd_def)

    # Byte layout:
    #   0  – 7    : TIFF header (8 bytes)
    #   8  – 9    : IFD entry count (2 bytes)
    #  10  – 10+12n-1 : IFD entries
    #  10+12n – +3: next-IFD offset = 0
    # extra_base = 8 + 2 + 12*n + 4
    extra_base = 14 + 12 * n
    bps_off  = extra_base
    desc_off = bps_off  + len(bps_block)
    xres_off = desc_off + len(desc_block)
    yres_off = xres_off + len(res_block)
    img_off  = yres_off + len(res_block)

    offsets = {258: bps_off, 270: desc_off, 273: img_off, 282: xres_off, 283: yres_off}

    buf = bytearray()
    buf += struct.pack('<2sHI', b'II', 42, 8)   # header
    buf += struct.pack('<H', n)                  # entry count
    for tag, typ, count, val in ifd_def:
        buf += struct.pack('<HHII', tag, typ, count, offsets[tag] if val is None else val)
    buf += struct.pack('<I', 0)   # next IFD = none
    buf += bps_block
    buf += desc_block
    buf += res_block              # XResolution
    buf += res_block              # YResolution
    buf += img_data               # compressed pixel data

    with open(path, 'wb') as f:
        f.write(buf)


def cmd_tiff(input_path: str, output_path: str,
             object_name: str = '',
             ra_deg: float = None, dec_deg: float = None,
             focal_mm: float = None, pixel_size_um: float = None,
             exptime_s: float = None, iso: int = None):
    """Convert a RAW to a 16-bit linear debayered TIFF with Siril-compatible FITS metadata.

    No external packages beyond rawpy and numpy are required.
    """
    import rawpy

    with rawpy.imread(input_path) as raw:
        rgb = raw.postprocess(
            use_camera_wb=True,
            output_bps=16,
            no_auto_bright=True,
            gamma=(1, 1),     # linear – no tone curve, preserves full dynamic range
        )  # (H, W, 3) uint16

    # Build FITS-card header string for Siril's ImageDescription reader.
    def _card(key, value, comment=''):
        if isinstance(value, bool):
            v = f"{'T' if value else 'F':>20}"
        elif isinstance(value, float):
            v = f"{value:>20.6f}"
        elif isinstance(value, int):
            v = f"{value:>20d}"
        else:
            v = f"'{str(value):<18}'"
        return (f"{key:<8}= {v} / {comment}")[:80].ljust(80)

    cards = [
        _card('SIMPLE',  True,         'FITS-compliant header'),
        _card('BITPIX',  16,           'Bits per data value'),
        _card('NAXIS',   3,            'Number of data axes'),
        _card('NAXIS1',  rgb.shape[1], 'Image width in pixels'),
        _card('NAXIS2',  rgb.shape[0], 'Image height in pixels'),
        _card('NAXIS3',  3,            'Number of channels (RGB)'),
        _card('PROGRAM', 'AstroPI',    'Capture software'),
    ]
    if object_name:
        cards.append(_card('OBJECT',   object_name,       'Target object name'))
    if exptime_s is not None:
        cards.append(_card('EXPTIME',  float(exptime_s),  '[s] Exposure duration'))
    if iso is not None:
        cards.append(_card('ISOSPEED', int(iso),          'ISO/gain setting'))
    if focal_mm is not None:
        cards.append(_card('FOCAL',    float(focal_mm),   '[mm] Telescope focal length'))
    if pixel_size_um is not None:
        cards.append(_card('XPIXSZ',   float(pixel_size_um), '[um] Pixel size X'))
        cards.append(_card('YPIXSZ',   float(pixel_size_um), '[um] Pixel size Y'))
    if ra_deg is not None:
        ra_h = ra_deg / 15.0
        ra_hms = '{:02.0f}:{:02.0f}:{:05.2f}'.format(
            int(ra_h), int((ra_h % 1) * 60), ((ra_h * 60) % 1) * 60)
        cards.append(_card('RA',       float(ra_deg), '[deg] Mount RA J2000'))
        cards.append(_card('OBJCTRA',  ra_hms,        'Mount RA  HH:MM:SS.ss'))
    if dec_deg is not None:
        sign = '+' if dec_deg >= 0 else '-'
        adec = abs(dec_deg)
        dec_dms = '{}{:02.0f}:{:02.0f}:{:04.1f}'.format(
            sign, int(adec), int((adec % 1) * 60), ((adec * 60) % 1) * 60)
        cards.append(_card('DEC',      float(dec_deg), '[deg] Mount Dec J2000'))
        cards.append(_card('OBJCTDEC', dec_dms,        'Mount Dec DD:MM:SS.s'))
    cards.append(('END' + ' ' * 77)[:80])

    _write_tiff_16bit_rgb(output_path, rgb, ''.join(cards))


def cmd_fitshdr(input_path: str):
    """Print FITS header as JSON to stdout."""
    from astropy.io import fits
    result = {}
    with fits.open(input_path) as hdul:
        for card in hdul[0].header.cards:
            if card.keyword:
                result[card.keyword] = str(card.value)
    print(json.dumps(result))


def _extract_stars_from_raw(image_path: str, max_stars: int = 300,
                            threshold_percentile: float = 99.5):
    """Extract star pixel positions from a RAW (or FITS) image.

    Pipeline:
      1. Extract embedded JPEG thumbnail from RAW (fast path, ~10 ms).
      2. Downsample 4× — 16× fewer pixels, scipy ops are fast.
      3. Subtract local background so vignetting / light-pollution gradients
         don't bias the threshold toward the bright centre.
      4. Label connected components; compute per-blob stats with np.bincount
         (vectorised — no Python loop over individual features).
      5. Filter: discard blobs that are too small (noise/hot-pixels) or too
         large (galaxies, nebulae, satellite trails).
      6. Scale centroids back to full-resolution coordinates.

    Returns a list of (x, y) tuples in full-res pixel coordinates, brightest first.
    """
    import rawpy
    import io
    from PIL import Image
    from scipy.ndimage import label as sc_label, uniform_filter

    DOWNSAMPLE = 4   # spatial factor before blob detection
    # Stars in the downsampled image are 1-4 px across.
    # Extended objects (galaxies/nebulae) are much larger — reject them.
    MIN_BLOB_PX = 2
    MAX_BLOB_PX = 40   # in downsampled pixels (~160 full-res px across)

    ext = Path(image_path).suffix.lower()
    scale_x = float(DOWNSAMPLE)
    scale_y = float(DOWNSAMPLE)

    if ext in ('.fits', '.fit'):
        from astropy.io import fits as astrofits
        with astrofits.open(image_path) as hdul:
            data = hdul[0].data.astype(np.float32)
        if data.ndim == 3:
            data = data[0]
        h, w = data.shape
        h2, w2 = h // DOWNSAMPLE, w // DOWNSAMPLE
        gray = data[:h2*DOWNSAMPLE, :w2*DOWNSAMPLE] \
                   .reshape(h2, DOWNSAMPLE, w2, DOWNSAMPLE).mean(axis=(1, 3))
        scale_x = scale_y = float(DOWNSAMPLE)
    else:
        with rawpy.imread(image_path) as raw:
            full_h, full_w = raw.raw_image_visible.shape
            try:
                thumb = raw.extract_thumb()
                if thumb.format == rawpy.ThumbFormat.JPEG:
                    img = Image.open(io.BytesIO(thumb.data)).convert('L')
                else:
                    img = Image.fromarray(thumb.data).convert('L')
            except Exception:
                # Slow fallback: decode full Bayer plane
                raw_arr = raw.raw_image_visible.astype(np.uint8)
                img = Image.fromarray(raw_arr).convert('L')

        thumb_w, thumb_h = img.size
        scale_x = full_w / (thumb_w / DOWNSAMPLE)
        scale_y = full_h / (thumb_h / DOWNSAMPLE)
        small = img.resize((thumb_w // DOWNSAMPLE, thumb_h // DOWNSAMPLE),
                           Image.BOX)
        gray = np.array(small, dtype=np.float32)

    # ── Local background subtraction ─────────────────────────────────────────
    # uniform_filter with a kernel ~1/8 the image height removes large-scale
    # gradients (vignetting, light pollution, sky glow) while leaving stars
    # (point sources) unaffected.  O(n) — fast regardless of kernel size.
    bg_kernel = max(5, min(gray.shape) // 8)
    background = uniform_filter(gray, size=bg_kernel)
    gray_sub = np.clip(gray.astype(np.float32) - background, 0.0, None)

    # Threshold on the background-subtracted image so the percentile reflects
    # actual source brightness, not the varying sky pedestal.
    positives = gray_sub[gray_sub > 0]
    if len(positives) == 0:
        return []
    threshold_value = float(np.percentile(positives, threshold_percentile))
    binary = gray_sub > threshold_value

    labeled, num_features = sc_label(binary)
    if num_features == 0:
        return []

    labels_flat = labeled.ravel()
    gray_flat   = gray_sub.ravel()   # use bg-subtracted for brightness ranking

    # ── Vectorised per-label stats (no Python loop) ───────────────────────────
    counts  = np.bincount(labels_flat, minlength=num_features + 1)
    sum_x   = np.bincount(labels_flat,
                          weights=np.tile(np.arange(gray.shape[1]), gray.shape[0]),
                          minlength=num_features + 1)
    sum_y   = np.bincount(labels_flat,
                          weights=np.repeat(np.arange(gray.shape[0]), gray.shape[1]),
                          minlength=num_features + 1)

    order     = np.argsort(gray_flat)
    peak_vals = np.zeros(num_features + 1, dtype=np.float32)
    peak_vals[labels_flat[order]] = gray_flat[order]   # last write wins → max

    counts   = counts[1:]
    cx_arr   = sum_x[1:] / np.maximum(counts, 1)
    cy_arr   = sum_y[1:] / np.maximum(counts, 1)
    peak_arr = peak_vals[1:]

    # Size filter: too small → noise/hot-pixel; too large → galaxy/nebula/trail
    mask = (counts >= MIN_BLOB_PX) & (counts <= MAX_BLOB_PX)
    cx_arr   = cx_arr[mask]
    cy_arr   = cy_arr[mask]
    peak_arr = peak_arr[mask]

    # Sort by brightness, return top max_stars scaled to full resolution
    order = np.argsort(peak_arr)[::-1][:max_stars]
    return [(float(cx_arr[i]) * scale_x,
             float(cy_arr[i]) * scale_y) for i in order]


def _t(label: str, t0: float) -> float:
    """Print a timing checkpoint to stderr and return current time."""
    import time
    now = time.perf_counter()
    print(f"  [TIMING] {label}: {now - t0:.3f}s", file=sys.stderr)
    return now


def _adaptive_extract_stars(image_path: str) -> list:
    """Extract stars, retrying with a lower threshold if too few are found.

    Targets 30–300 stars — enough for a confident match without flooding the
    solver with noise.  Returns a list of (x, y) full-resolution coordinates,
    brightest first.
    """
    # Try progressively lower percentile thresholds until we have enough stars.
    for pct in (99.5, 99.0, 98.5, 98.0, 97.0):
        stars = _extract_stars_from_raw(image_path, max_stars=300,
                                        threshold_percentile=pct)
        if len(stars) >= 15:
            print(f"  [TIMING] star extraction  ({len(stars)} stars, "
                  f"threshold=p{pct})", file=sys.stderr)
            return stars
    # Last resort: return whatever we got from the lowest threshold
    return stars


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
    Timing checkpoints are printed to stderr.

    Three attempts are made before giving up, each with more relaxed params:
      1. Tight:  given scale range + hint radius,  positional_noise=2 px
      2. Wide:   2× scale range + 2× hint radius,  positional_noise=3 px
      3. Blind:  full scale range,  no position hint, positional_noise=3 px
    """
    import time
    import astrometry

    t_total = time.perf_counter()
    print(f"  [TIMING] solve started  image={image_path}", file=sys.stderr)

    if index_cache_dir is None:
        index_cache_dir = Path.home() / '.cache' / 'astrometry'

    # scales 5-7 cover ~1–7 arcsec/pix — right for typical DSLR+telescope setups
    scales = {5, 6, 7}

    t = time.perf_counter()
    try:
        index_files = astrometry.series_4200.index_files(
            cache_directory=index_cache_dir,
            scales=scales,
        )
    except Exception as e:
        print(json.dumps({"success": False, "error": f"Failed to load index files: {e}"}))
        sys.exit(0)
    _t(f"index files loaded  ({len(index_files)} files)", t)

    if not index_files:
        print(json.dumps({"success": False, "error": "No index files available"}))
        sys.exit(0)

    t = time.perf_counter()
    try:
        stars = _adaptive_extract_stars(image_path)
    except Exception as e:
        print(json.dumps({"success": False, "error": f"Star extraction failed: {e}"}))
        sys.exit(0)
    _t(f"star extraction total", t)

    if len(stars) < 5:
        print(json.dumps({"success": False,
                          "error": f"Too few stars detected ({len(stars)}), need at least 5"}))
        sys.exit(0)

    # ── Attempt parameters (escalating relaxation) ────────────────────────────
    #
    # positional_noise_pixels: our stars come from a 4× downsampled image
    # scaled back up, so each coordinate has ~±2 full-res pixel uncertainty.
    # The default of 1.0 is too strict — we'd reject good matches near the
    # centroid error boundary.
    #
    # scale range: widen on each retry so a miscalibrated focal length
    # doesn't block every attempt.
    #
    # radius: mount pointing can be off by more than the nominal value after
    # a long slew or if no sync has been done.
    mid_scale   = (pixel_scale_low + pixel_scale_high) / 2.0
    half_range  = (pixel_scale_high - pixel_scale_low) / 2.0

    attempts = [
        dict(
            label="tight (given hint)",
            size_hint=astrometry.SizeHint(pixel_scale_low, pixel_scale_high),
            position_hint=astrometry.PositionHint(ra_hint_deg, dec_hint_deg,
                                                   search_radius_deg),
            noise=2.0,
        ),
        dict(
            label="wide (2× scale + 2× radius)",
            size_hint=astrometry.SizeHint(
                max(0.1, mid_scale - half_range * 2),
                mid_scale + half_range * 2,
            ),
            position_hint=astrometry.PositionHint(ra_hint_deg, dec_hint_deg,
                                                   search_radius_deg * 2),
            noise=3.0,
        ),
        dict(
            label="blind (no position hint)",
            size_hint=astrometry.SizeHint(
                max(0.1, mid_scale - half_range * 3),
                mid_scale + half_range * 3,
            ),
            position_hint=None,
            noise=3.0,
        ),
    ]

    solution = None
    attempt_label = ""

    try:
        with astrometry.Solver(index_files) as solver:
            for attempt in attempts:
                t = time.perf_counter()
                sol = solver.solve(
                    stars=stars,
                    size_hint=attempt["size_hint"],
                    position_hint=attempt["position_hint"],
                    solution_parameters=astrometry.SolutionParameters(
                        positional_noise_pixels=attempt["noise"],
                        logodds_callback=lambda logodds_list: (
                            astrometry.Action.STOP
                            if max(logodds_list) > 40
                            else astrometry.Action.CONTINUE
                        ),
                    ),
                )
                _t(f"attempt '{attempt['label']}'", t)

                if sol.has_match():
                    m = sol.best_match()
                    # Sanity check: solution must be within 35° of the hint
                    # (blind attempt has no constraint — always accept).
                    if attempt["position_hint"] is not None:
                        d_ra  = abs(m.center_ra_deg  - ra_hint_deg)
                        d_dec = abs(m.center_dec_deg - dec_hint_deg)
                        # RA wraps at 360°
                        d_ra = min(d_ra, 360.0 - d_ra)
                        dist = (d_ra ** 2 + d_dec ** 2) ** 0.5
                        if dist > 35.0:
                            print(f"  [TIMING] attempt '{attempt['label']}' rejected "
                                  f"(solution {dist:.1f}° from hint)", file=sys.stderr)
                            continue
                    solution = sol
                    attempt_label = attempt["label"]
                    break
    except Exception as e:
        print(json.dumps({"success": False, "error": f"Solver error: {e}"}))
        sys.exit(0)

    _t("TOTAL", t_total)

    if solution is not None and solution.has_match():
        m = solution.best_match()
        print(json.dumps({
            "success": True,
            "ra_deg": m.center_ra_deg,
            "dec_deg": m.center_dec_deg,
            "scale_arcsec_per_pixel": m.scale_arcsec_per_pixel,
            "logodds": m.logodds,
            "attempt": attempt_label,
        }))
    else:
        print(json.dumps({"success": False, "error": "No solution found after all attempts"}))


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
    elif cmd == 'tiff':
        def _opt_float(idx): return float(sys.argv[idx]) if len(sys.argv) > idx and sys.argv[idx] else None
        def _opt_int(idx):   return int(sys.argv[idx])   if len(sys.argv) > idx and sys.argv[idx] else None
        cmd_tiff(
            sys.argv[2], sys.argv[3],
            object_name   = sys.argv[4] if len(sys.argv) > 4 else '',
            ra_deg        = _opt_float(5),
            dec_deg       = _opt_float(6),
            focal_mm      = _opt_float(7),
            pixel_size_um = _opt_float(8),
            exptime_s     = _opt_float(9),
            iso           = _opt_int(10),
        )
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
