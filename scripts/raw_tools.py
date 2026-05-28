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
import math
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


def _read_fits(path: str):
    """Read a FITS file without astropy. Returns (data_array, header_dict).

    Supports BITPIX 16 (int16) and -32 (float32).  Applies BZERO / BSCALE.
    """
    import struct
    keywords: dict = {}
    header_blocks = 0
    found_end = False

    with open(path, 'rb') as f:
        while not found_end:
            block = f.read(2880)
            if len(block) < 2880:
                header_blocks += 1
                break
            header_blocks += 1
            for i in range(36):
                card = block[i * 80:(i + 1) * 80].decode('ascii', errors='replace')
                key = card[:8].strip()
                if key == 'END':
                    found_end = True
                    break
                if len(card) >= 10 and card[8:10] == '= ':
                    raw_val = card[10:].strip()
                    if raw_val.startswith("'"):
                        end_q = raw_val.find("'", 1)
                        val = raw_val[1:end_q].strip() if end_q > 0 else ''
                    else:
                        val_str = raw_val.split('/')[0].strip()
                        try:
                            val = int(val_str)
                        except ValueError:
                            try:
                                val = float(val_str)
                            except ValueError:
                                val = val_str
                    if key:
                        keywords[key] = val

        f.seek(header_blocks * 2880)
        bitpix = int(keywords.get('BITPIX', 16))
        w = int(keywords.get('NAXIS1', 0))
        h = int(keywords.get('NAXIS2', 0))
        n = w * h
        if bitpix == 16:
            data = np.frombuffer(f.read(n * 2), dtype='>i2').astype(np.float32)
        elif bitpix == -32:
            data = np.frombuffer(f.read(n * 4), dtype='>f4').astype(np.float32)
        elif bitpix == 32:
            data = np.frombuffer(f.read(n * 4), dtype='>i4').astype(np.float32)
        else:
            raise ValueError(f'Unsupported BITPIX={bitpix}')

    bscale = float(keywords.get('BSCALE', 1.0))
    bzero  = float(keywords.get('BZERO',  0.0))
    if bscale != 1.0:
        data *= bscale
    if bzero != 0.0:
        data += bzero
    return data.reshape(h, w), keywords


def _write_fits_16bit(path: str, arr: np.ndarray, keywords: list) -> None:
    """Write a 16-bit FITS file using only struct + numpy (no astropy).

    keywords: list of (key, value, comment) tuples appended after the mandatory
    SIMPLE/BITPIX/NAXIS*/BZERO/BSCALE cards.  uint16 data is stored as int16
    with BZERO=32768 per the FITS standard for unsigned-short images.
    """
    h, w = arr.shape[:2]

    def _fmt(key, val, comment=''):
        k = f'{key:<8}'
        if isinstance(val, bool):
            v = f"{'T' if val else 'F':>20}"
        elif isinstance(val, int):
            v = f'{val:>20d}'
        elif isinstance(val, float):
            v = f'{val:>20.10G}'
        else:
            v = f"'{str(val):<18}'"
        s = f'{k}= {v}'
        if comment:
            s = f'{s} / {comment}'
        return s[:80].ljust(80)

    cards = [
        _fmt('SIMPLE', True,    'file conforms to FITS standard'),
        _fmt('BITPIX', 16,      'bits per data pixel'),
        _fmt('NAXIS',  2,       'number of data axes'),
        _fmt('NAXIS1', w,       'length of data axis 1 (columns)'),
        _fmt('NAXIS2', h,       'length of data axis 2 (rows)'),
        _fmt('BZERO',  32768.0, 'offset: physical = stored + BZERO'),
        _fmt('BSCALE', 1.0,     'data scaling factor'),
    ]
    for key, val, comment in keywords:
        cards.append(_fmt(key, val, comment))
    cards.append(('END' + ' ' * 77)[:80])
    while len(cards) % 36:
        cards.append(' ' * 80)

    stored = (arr.astype(np.int32) - 32768).astype('>i2').tobytes()
    pad = (2880 - len(stored) % 2880) % 2880

    with open(path, 'wb') as f:
        f.write(''.join(cards).encode('ascii'))
        f.write(stored)
        if pad:
            f.write(b'\x00' * pad)


_RAW_EXTS     = ('cr3', 'cr2', 'nef', 'arw', 'dng', 'raf', 'orf')
_CAPTURES_ROOT = 'imgs/astro_captures'
_PREVIEW_DIR   = 'imgs/.previews'


def _preview_jpeg_path(fits_path: str) -> Path:
    """Return the path of the companion JPEG for a FITS file.

    Mirrors the FITS subfolder structure under imgs/.previews/ so the
    captures directory stays clean.
    e.g. imgs/astro_captures/M31/lights/frame_0001.fits
      →  imgs/.previews/M31/lights/frame_0001.jpg
    """
    p = Path(fits_path)
    try:
        rel = p.relative_to(_CAPTURES_ROOT)
    except ValueError:
        rel = Path(p.name)
    dest = Path(_PREVIEW_DIR) / rel.with_suffix('.jpg')
    dest.parent.mkdir(parents=True, exist_ok=True)
    return dest


def _make_companion_jpeg(fits_path: str) -> bool:
    """Generate a companion JPEG in imgs/.previews/ using rawpy, then delete the RAW.

    Called lazily the first time a FITS file without a companion is previewed.
    Returns True if the JPEG was created successfully.
    """
    p = Path(fits_path)
    for ext in _RAW_EXTS:
        for raw in (p.with_suffix('.' + ext), p.with_suffix('.' + ext.upper())):
            if not raw.exists():
                continue
            try:
                import rawpy
                from PIL import Image as _Im
                with rawpy.imread(str(raw)) as r:
                    rgb8 = r.postprocess(use_camera_wb=True, half_size=True,
                                         no_auto_bright=False, output_bps=8)
                _Im.fromarray(rgb8).save(str(_preview_jpeg_path(fits_path)), 'JPEG', quality=85)
                try:
                    raw.unlink()
                except Exception:
                    pass
                return True
            except Exception:
                pass
    return False


def _fits_to_pil(input_path: str, stretch_lo: float = 0.1, stretch_hi: float = 99.9):
    """Return a PIL Image for a FITS file.

    Priority:
      1. Companion JPEG in imgs/.previews/ (rawpy-processed, exact camera colours).
      2. If missing but a sibling RAW still exists, generate the companion then use it.
      3. Fall back to WB-corrected debayer + percentile stretch (slightly green).
    """
    from PIL import Image
    companion = _preview_jpeg_path(input_path)
    if not companion.exists():
        _make_companion_jpeg(input_path)
    if companion.exists():
        return Image.open(str(companion))

    # No rawpy source available — best-effort from FITS data.
    data, kw = _read_fits(input_path)
    bayerpat = str(kw.get('BAYERPAT', '')).strip().upper()
    if bayerpat in ('RGGB', 'BGGR', 'GRBG', 'GBRG'):
        arr16 = np.clip(data, 0, 65535).astype(np.uint16)
        rgb   = _debayer_2x2(arr16, bayerpat).astype(np.float32)
        wb_r  = float(kw.get('WB_RED',  2.0))
        wb_b  = float(kw.get('WB_BLUE', 1.5))
        rgb[:, :, 0] *= wb_r
        rgb[:, :, 2] *= wb_b
        rgb = np.clip(rgb, 0, 65535)
        out = np.empty(rgb.shape, dtype=np.uint8)
        for c in range(3):
            out[:, :, c] = _stretch(rgb[:, :, c], stretch_lo, stretch_hi)
        return Image.fromarray(out, mode='RGB')
    stretched = _stretch(data, stretch_lo, stretch_hi)
    return Image.fromarray(stretched, mode='L')


def _read_tiff_bayerpat(path: str, default: str = 'RGGB') -> str:
    """Read BAYERPAT from a TIFF's ImageDescription FITS cards (header only, no pixel I/O)."""
    import struct
    try:
        with open(path, 'rb') as f:
            hdr = f.read(8)
            endian = '<' if hdr[:2] == b'II' else '>'
            if struct.unpack_from(f'{endian}H', hdr, 2)[0] != 42:
                return default
            ifd_off = struct.unpack_from(f'{endian}I', hdr, 4)[0]
            f.seek(ifd_off)
            n = struct.unpack_from(f'{endian}H', f.read(2))[0]
            entries = f.read(12 * n)
            desc_off = desc_len = None
            for i in range(n):
                tag, typ, count = struct.unpack_from(f'{endian}HHI', entries, 12 * i)
                if tag == 270 and typ == 2:   # ImageDescription (ASCII)
                    off = struct.unpack_from(f'{endian}I', entries, 12 * i + 8)[0]
                    desc_off, desc_len = (off, count) if count > 4 else (None, None)
                    break
            if desc_off is None:
                return default
            f.seek(desc_off)
            desc = f.read(desc_len).rstrip(b'\x00').decode('ascii', errors='replace')
    except Exception:
        return default

    for pos in range(0, len(desc), 80):
        card = desc[pos:pos + 80]
        if len(card) < 10 or card[:8].strip() == 'END':
            break
        if card[:8].strip() == 'BAYERPAT' and card[8:10] == '= ':
            raw_val = card[10:].strip()
            if raw_val.startswith("'"):
                end_q = raw_val.find("'", 1)
                val = raw_val[1:end_q].strip() if end_q > 0 else raw_val[1:].strip()
            else:
                val = raw_val.split('/')[0].strip()
            if val:
                pat = val[:4].upper()
                # 'RGBG' is rawpy's color_desc (lookup table), not a real Bayer
                # pattern — remap it to the standard 4-letter tile string.
                if pat not in ('RGGB', 'BGGR', 'GRBG', 'GBRG'):
                    pat = default
                return pat
    return default


def _debayer_2x2(bayer: np.ndarray, pattern: str = 'RGGB') -> np.ndarray:
    """Fast 2×2 block debayer — halves each dimension, no interpolation artifacts.

    Each 2×2 CFA block is collapsed into one RGB pixel by averaging the two
    green sub-pixels. Supports all four standard Bayer patterns.
    Returns a (H//2, W//2, 3) uint16 array.
    """
    H, W = bayer.shape[0] // 2, bayer.shape[1] // 2
    # Extract the four 2×2 sub-planes
    s = [bayer[r::2, c::2][:H, :W].astype(np.float32)
         for r, c in ((0, 0), (0, 1), (1, 0), (1, 1))]
    p = pattern.upper()
    r_acc = np.zeros((H, W), np.float32)
    g_acc = np.zeros((H, W), np.float32)
    b_acc = np.zeros((H, W), np.float32)
    g_n   = 0
    for sub, color in zip(s, p):
        if color == 'R':
            r_acc += sub
        elif color == 'B':
            b_acc += sub
        else:   # G
            g_acc += sub
            g_n   += 1
    if g_n > 1:
        g_acc /= g_n
    return np.stack([r_acc, g_acc, b_acc], axis=2).clip(0, 65535).astype(np.uint16)


def _read_tiff_array(path: str) -> np.ndarray:
    """Return the pixel data of our 16-bit RGB TIFF as a (H, W, 3) uint16 array.

    Handles both uncompressed (compression=1) and DEFLATE (compression=8/32946).
    Uses only struct + zlib + numpy so it works on any platform without tifffile.
    """
    import struct

    with open(path, 'rb') as f:
        raw = f.read()

    bom = raw[0:2]
    endian = '<' if bom == b'II' else '>'
    if struct.unpack_from(f'{endian}H', raw, 2)[0] != 42:
        raise ValueError(f'Not a TIFF file: {path}')

    ifd_off    = struct.unpack_from(f'{endian}I', raw, 4)[0]
    n_entries  = struct.unpack_from(f'{endian}H', raw, ifd_off)[0]
    type_size  = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8}

    tags: dict = {}
    for i in range(n_entries):
        base = ifd_off + 2 + 12 * i
        tag, typ, count = struct.unpack_from(f'{endian}HHI', raw, base)
        tsz = type_size.get(typ, 4)
        if count * tsz <= 4:
            val = struct.unpack_from(f'{endian}H', raw, base + 8)[0] if typ == 3 \
                  else struct.unpack_from(f'{endian}I', raw, base + 8)[0]
        else:
            off = struct.unpack_from(f'{endian}I', raw, base + 8)[0]
            val = struct.unpack_from(f'{endian}' + ('H' if typ == 3 else 'I') * count, raw, off)
        tags[tag] = val

    width, height   = tags[256], tags[257]
    compression     = tags.get(259, 1)
    strip_offset    = tags[273]
    strip_bytecount = tags[279]
    spp             = tags.get(277, 1)  # SamplesPerPixel

    payload = raw[strip_offset:strip_offset + strip_bytecount]
    if compression in (8, 32946):
        payload = zlib.decompress(payload)
    elif compression != 1:
        raise ValueError(f'Unsupported TIFF compression={compression} in {path}')

    arr = np.frombuffer(payload, dtype=f'{endian}u2')
    if spp == 1:
        return arr.reshape(height, width)       # (H, W) — Bayer or grayscale
    return arr.reshape(height, width, spp)      # (H, W, C) — RGB legacy


def _tiff_to_pil(path: str, stretch_lo: float = 0.1, stretch_hi: float = 99.9,
                 half_size: bool = False):
    """Read a 16-bit TIFF (Bayer single-channel or RGB) and return a stretched 8-bit color PIL Image."""
    from PIL import Image

    arr = _read_tiff_array(path)

    if arr.ndim == 2:
        pat = _read_tiff_bayerpat(path)
        arr = _debayer_2x2(arr, pat).astype(np.float32)
        # Read WB from the FITS cards in ImageDescription
        wb_r = wb_b = None
        try:
            import struct as _struct
            with open(path, 'rb') as _f:
                _raw = _f.read()
            _bom = _raw[0:2]
            _e = '<' if _bom == b'II' else '>'
            _ifd = _struct.unpack_from(f'{_e}I', _raw, 4)[0]
            _n   = _struct.unpack_from(f'{_e}H', _raw, _ifd)[0]
            for _i in range(_n):
                _b = _ifd + 2 + 12 * _i
                _tag, _typ, _cnt = _struct.unpack_from(f'{_e}HHI', _raw, _b)
                if _tag == 270 and _cnt > 4:
                    _off = _struct.unpack_from(f'{_e}I', _raw, _b + 8)[0]
                    _desc = _raw[_off:_off + _cnt].rstrip(b'\x00').decode('ascii', 'replace')
                    for _pos in range(0, len(_desc), 80):
                        _card = _desc[_pos:_pos + 80]
                        _key  = _card[:8].strip()
                        if _key in ('WB_RED', 'WB_BLUE') and _card[8:10] == '= ':
                            _v = _card[10:].split('/')[0].strip()
                            try:
                                if _key == 'WB_RED':  wb_r = float(_v)
                                else:                 wb_b = float(_v)
                            except ValueError:
                                pass
        except Exception:
            pass
        if wb_r is not None: arr[:, :, 0] *= wb_r
        if wb_b is not None: arr[:, :, 2] *= wb_b
        arr = np.clip(arr, 0, 65535)
    else:
        arr = arr.astype(np.float32)

    if half_size and arr.ndim == 3:
        arr = arr[::2, ::2]

    out = np.empty(arr.shape, dtype=np.uint8)
    for c in range(arr.shape[2]):
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
    elif ext in ('.jpg', '.jpeg', '.png'):
        from PIL import Image
        img = Image.open(input_path)
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
    elif ext in ('.jpg', '.jpeg', '.png'):
        from PIL import Image
        img = Image.open(input_path)
    elif ext in ('.tif', '.tiff'):
        img = _tiff_to_pil(input_path, stretch_lo, stretch_hi)
    else:
        img = _raw_to_pil(input_path, half_size=False)

    img.save(output_path, 'JPEG', quality=90)


def cmd_snap_jpeg(input_path: str, output_path: str):
    """Convert a RAW file to JPEG for preview/plate-solve, without dnglab.

    Fast path (Canon CR3 and most DSLRs): extract the camera-generated embedded
    JPEG thumbnail directly — ~10-50 ms, zero debayering overhead.
    Fallback: half-size rawpy postprocess — ~300-800 ms, still 10-40× faster
    than the dnglab → rawloader path.

    Always writes a companion <output>.scale_factor file containing a single
    float: the ratio (sensor_width / jpeg_width).  The plate-solve endpoint
    multiplies the sensor pixel-scale by this factor so the solver receives the
    correct arcsec/pixel value for the JPEG it is actually solving.
    """
    import rawpy, io
    from PIL import Image

    import time as _time
    t0 = _time.perf_counter()
    print(f"[snap_jpeg] input={input_path}", file=sys.stderr)

    scale_factor = 1.0  # updated below once we know the output dimensions

    with rawpy.imread(input_path) as raw:
        full_h, full_w = raw.raw_image_visible.shape  # sensor pixel count
        try:
            thumb = raw.extract_thumb()
            if thumb.format == rawpy.ThumbFormat.JPEG:
                with open(output_path, 'wb') as f:
                    f.write(thumb.data)
                # Measure the embedded JPEG dimensions to compute scale factor
                with Image.open(io.BytesIO(thumb.data)) as _t:
                    thumb_w, thumb_h = _t.size
                scale_factor = full_w / thumb_w
                kb = len(thumb.data) // 1024
                print(f"[snap_jpeg] embedded JPEG {thumb_w}x{thumb_h} (sensor {full_w}x{full_h})  "
                      f"sensor_width={full_w}  {kb} KB  {_time.perf_counter()-t0:.2f}s",
                      file=sys.stderr)
                _write_scale_factor(output_path, full_w)
                return
            elif thumb.format == rawpy.ThumbFormat.BITMAP:
                img = Image.fromarray(thumb.data)
                thumb_w, thumb_h = img.size
                scale_factor = full_w / thumb_w
                img.save(output_path, 'JPEG', quality=90)
                print(f"[snap_jpeg] bitmap thumbnail {thumb_w}x{thumb_h}  sensor_width={full_w}  "
                      f"{_time.perf_counter()-t0:.2f}s", file=sys.stderr)
                _write_scale_factor(output_path, full_w)
                return
        except Exception as ex:
            print(f"[snap_jpeg] no embedded thumbnail ({ex}), falling back to rawpy decode", file=sys.stderr)

        # Fallback: half-size debayer — measure actual output dimensions instead of assuming 2×
        rgb = raw.postprocess(use_camera_wb=True, half_size=True, no_auto_bright=False, output_bps=8)
        h_out, w_out = rgb.shape[:2]
        scale_factor = full_w / w_out if w_out > 0 else 2.0

    Image.fromarray(rgb).save(output_path, 'JPEG', quality=90)
    print(f"[snap_jpeg] rawpy half-size decode  sensor_width={full_w}  "
          f"{_time.perf_counter()-t0:.2f}s", file=sys.stderr)
    _write_scale_factor(output_path, full_w)


def _write_scale_factor(jpeg_path: str, sensor_width_px: int):
    """Write the original sensor width (pixels) to a sidecar file.

    Rust reads this value, then divides by the actual JPEG width AFTER any
    resize step to get the true scale_factor (sensor_px / jpeg_px).
    Storing the raw sensor width (not the ratio) is required because Rust
    resizes the preview JPEG before writing snap_latest.jpg, and the ratio
    must be recomputed from the post-resize dimensions.
    """
    sidecar = jpeg_path + '.scale_factor'
    try:
        with open(sidecar, 'w') as f:
            f.write(str(int(sensor_width_px)))
    except Exception as ex:
        print(f"[snap_jpeg] warning: could not write sensor_width sidecar: {ex}", file=sys.stderr)


def cmd_fits(input_path: str, output_path: str,
             object_name: str = '',
             ra_deg: float = None, dec_deg: float = None):
    """Convert a RAW file to FITS (raw Bayer CFA data) with optional mount position."""
    import rawpy

    with rawpy.imread(input_path) as raw:
        bayer      = raw.raw_image_visible.astype(np.uint16)
        color_desc = raw.color_desc.decode('ascii', errors='replace')
        try:
            bayer_pat = ''.join(color_desc[i] for i in raw.raw_pattern.flatten()[:4])
        except Exception:
            bayer_pat = color_desc[:4]

    kw = [
        ('INSTRUME', 'DSLR',    'Camera instrument'),
        ('BAYERPAT', bayer_pat, 'Bayer color pattern'),
        ('XBINNING', 1,         'Binning X'),
        ('YBINNING', 1,         'Binning Y'),
    ]
    if object_name:
        kw.append(('OBJECT', object_name, 'Target object name'))
    if ra_deg is not None:
        ra_h = ra_deg / 15.0
        ra_hms = '{:02.0f}:{:02.0f}:{:05.2f}'.format(
            int(ra_h), int((ra_h % 1) * 60), ((ra_h * 60) % 1) * 60)
        kw.append(('RA',      float(ra_deg), '[deg] Mount RA (J2000)'))
        kw.append(('OBJCTRA', ra_hms,        'Mount RA in HH:MM:SS'))
    if dec_deg is not None:
        sign = '+' if dec_deg >= 0 else '-'
        adec = abs(dec_deg)
        dec_dms = '{}{:02.0f}:{:02.0f}:{:04.1f}'.format(
            sign, int(adec), int((adec % 1) * 60), ((adec * 60) % 1) * 60)
        kw.append(('DEC',      float(dec_deg), '[deg] Mount Dec (J2000)'))
        kw.append(('OBJCTDEC', dec_dms,        'Mount Dec in DD:MM:SS'))

    _write_fits_16bit(output_path, bayer, kw)


def _write_tiff_16bit_gray(path: str, arr: np.ndarray, description: str = '') -> None:
    """Write a 16-bit single-channel TIFF with fast DEFLATE compression (level 1).

    Stores raw Bayer sensor data exactly as it came off the sensor — no debayering,
    no tone curve.  File size is ~3× smaller than a debayered RGB TIFF and the
    compression step is ~10× faster (level 1 vs 6).  Siril reads BAYERPAT from
    the FITS cards in ImageDescription and debayers automatically during stacking.
    """
    import struct

    h, w   = arr.shape[:2]
    pixels = arr.astype('<u2').tobytes()
    img_data   = zlib.compress(pixels, 1)              # level 1 = fast
    desc_block = description.encode('ascii', errors='replace') + b'\x00'
    res_block  = struct.pack('<2I', 72, 1)             # RATIONAL 72/1 dpi

    # BitsPerSample=16 fits directly in the IFD value field (count=1, 2 bytes ≤ 4).
    ifd_def = [
        (256, 4, 1, w),                   # ImageWidth
        (257, 4, 1, h),                   # ImageLength
        (258, 3, 1, 16),                  # BitsPerSample = 16  (direct)
        (259, 3, 1, 8),                   # Compression   = DEFLATE
        (262, 3, 1, 1),                   # PhotometricInterp = BlackIsZero
        (270, 2, len(desc_block), None),  # ImageDescription → desc_block
        (273, 4, 1, None),                # StripOffsets   → img_data
        (277, 3, 1, 1),                   # SamplesPerPixel = 1
        (278, 4, 1, h),                   # RowsPerStrip
        (279, 4, 1, len(img_data)),       # StripByteCounts
        (282, 5, 1, None),                # XResolution    → res_block
        (283, 5, 1, None),                # YResolution    → res_block
        (284, 3, 1, 1),                   # PlanarConfiguration = chunky
        (296, 3, 1, 2),                   # ResolutionUnit = inch
    ]
    n          = len(ifd_def)
    extra_base = 14 + 12 * n              # header(8)+count(2)+entries(12n)+nextIFD(4)
    desc_off   = extra_base
    xres_off   = desc_off + len(desc_block)
    yres_off   = xres_off + 8             # RATIONAL = 2×LONG = 8 bytes
    img_off    = yres_off + 8

    offsets = {270: desc_off, 273: img_off, 282: xres_off, 283: yres_off}

    buf  = bytearray()
    buf += struct.pack('<2sHI', b'II', 42, 8)
    buf += struct.pack('<H', n)
    for tag, typ, count, val in ifd_def:
        buf += struct.pack('<HHII', tag, typ, count, offsets[tag] if val is None else val)
    buf += struct.pack('<I', 0)
    buf += desc_block
    buf += res_block   # XResolution
    buf += res_block   # YResolution
    buf += img_data

    with open(path, 'wb') as f:
        f.write(buf)


def cmd_tiff(input_path: str, output_path: str,
             object_name: str = '',
             ra_deg: float = None, dec_deg: float = None,
             focal_mm: float = None, pixel_size_um: float = None,
             exptime_s: float = None, iso: int = None):
    """Convert a RAW file to a 16-bit single-channel Bayer FITS file.

    Saves raw Bayer sensor data (no debayering) — the same approach used by ZWO
    ASI Air.  Siril reads BAYERPAT and debayers during its preprocessing step.
    Using native FITS (via astropy) ensures every keyword is read correctly by
    Siril without any ImageDescription parsing.  Write time is ~1 s (no
    compression), file size ~48 MB for a 24 MP sensor.
    """
    import rawpy
    from PIL import Image as _Image
    with rawpy.imread(input_path) as raw:
        bayer      = raw.raw_image_visible.copy()          # (H, W) uint16
        color_desc = raw.color_desc.decode('ascii', errors='replace')
        try:
            bayer_pat = ''.join(color_desc[i] for i in raw.raw_pattern.flatten()[:4])
        except Exception:
            bayer_pat = 'RGGB'
        try:
            black_level = int(round(
                sum(raw.black_level_per_channel) / len(raw.black_level_per_channel)))
        except Exception:
            black_level = None
        white_level = getattr(raw, 'white_level', None)
        try:
            wb = raw.camera_whitebalance
            g  = wb[1] if wb[1] > 0 else 1.0
            wb_r = wb[0] / g
            wb_b = wb[2] / g
        except Exception:
            wb_r, wb_b = 2.0, 1.5
        # Companion preview JPEG — full camera pipeline (WB + CCM + gamma + NR).
        # Generated while rawpy holds the file open; saved alongside the FITS so
        # gallery preview/thumbnail serve this instead of re-processing the FITS.
        try:
            rgb8 = raw.postprocess(use_camera_wb=True, half_size=True,
                                    no_auto_bright=False, output_bps=8)
            preview_jpg = _preview_jpeg_path(output_path)
            _Image.fromarray(rgb8).save(str(preview_jpg), 'JPEG', quality=85)
        except Exception:
            pass

    kw = [
        ('BAYERPAT', bayer_pat,      'Bayer color filter array pattern'),
        ('WB_RED',   float(wb_r),    'White balance R/G multiplier'),
        ('WB_BLUE',  float(wb_b),    'White balance B/G multiplier'),
        ('INSTRUME', 'DSLR',         'Camera type'),
        ('PROGRAM',  'AstroPI',      'Capture software'),
    ]
    if black_level is not None:
        kw.append(('PEDESTAL', black_level,       'Black level pedestal (ADU)'))
    if white_level is not None:
        kw.append(('MAXADU',   int(white_level),  'Saturation level (ADU)'))
    if object_name:
        kw.append(('OBJECT',   object_name,           'Target object name'))
    if exptime_s is not None:
        kw.append(('EXPTIME',  float(exptime_s),      '[s] Exposure duration'))
    if iso is not None:
        kw.append(('ISOSPEED', int(iso),  'ISO speed'))
        kw.append(('GAIN',     int(iso),  'ISO/gain (Siril)'))
    if focal_mm is not None:
        kw.append(('FOCAL',    float(focal_mm),       '[mm] Telescope focal length'))
    if pixel_size_um is not None:
        kw.append(('XPIXSZ',   float(pixel_size_um),  '[um] Pixel size X'))
        kw.append(('YPIXSZ',   float(pixel_size_um),  '[um] Pixel size Y'))
    if ra_deg is not None:
        ra_h = ra_deg / 15.0
        ra_hms = '{:02.0f}:{:02.0f}:{:05.2f}'.format(
            int(ra_h), int((ra_h % 1) * 60), ((ra_h * 60) % 1) * 60)
        kw.append(('RA',      float(ra_deg), '[deg] Mount RA J2000'))
        kw.append(('OBJCTRA', ra_hms,        'Mount RA HH:MM:SS.ss'))
    if dec_deg is not None:
        sign = '+' if dec_deg >= 0 else '-'
        adec = abs(dec_deg)
        dec_dms = '{}{:02.0f}:{:02.0f}:{:04.1f}'.format(
            sign, int(adec), int((adec % 1) * 60), ((adec * 60) % 1) * 60)
        kw.append(('DEC',      float(dec_deg), '[deg] Mount Dec J2000'))
        kw.append(('OBJCTDEC', dec_dms,        'Mount Dec DD:MM:SS.s'))

    _write_fits_16bit(output_path, bayer, kw)


def cmd_fitshdr(input_path: str):
    """Print FITS header as JSON to stdout."""
    _, kw = _read_fits(input_path)
    print(json.dumps({k: str(v) for k, v in kw.items()}))


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
        data, _ = _read_fits(image_path)
        h, w = data.shape
        h2, w2 = h // DOWNSAMPLE, w // DOWNSAMPLE
        gray = data[:h2*DOWNSAMPLE, :w2*DOWNSAMPLE] \
                   .reshape(h2, DOWNSAMPLE, w2, DOWNSAMPLE).mean(axis=(1, 3))
        scale_x = scale_y = float(DOWNSAMPLE)
    elif ext in ('.tif', '.tiff'):
        arr16 = _read_tiff_array(image_path)
        full_h, full_w = arr16.shape[:2]
        if arr16.ndim == 2:
            # Single-channel Bayer — use as-is (stars show up fine in raw Bayer)
            gray_full = arr16.astype(np.float32)
        else:
            # Legacy RGB — use green channel (most sensitive to stars)
            gray_full = arr16[:, :, 1].astype(np.float32)
        h2, w2 = full_h // DOWNSAMPLE, full_w // DOWNSAMPLE
        gray = gray_full[:h2 * DOWNSAMPLE, :w2 * DOWNSAMPLE] \
                   .reshape(h2, DOWNSAMPLE, w2, DOWNSAMPLE).mean(axis=(1, 3))
        scale_x = scale_y = float(DOWNSAMPLE)
    elif ext in ('.jpg', '.jpeg', '.png'):
        # JPEG/PNG snaps — load directly with PIL (no rawpy needed)
        img = Image.open(image_path).convert('L')
        full_w, full_h = img.size
        h2, w2 = full_h // DOWNSAMPLE, full_w // DOWNSAMPLE
        small = img.resize((w2, h2), Image.BOX)
        gray = np.array(small, dtype=np.float32)
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

    # Limit to brightest 100 stars — fewer stars = far fewer quadruplets to test,
    # dramatically faster solves. Stars are already sorted brightest-first by
    # _adaptive_extract_stars (flux descending).
    stars = stars[:100]
    print(f"  [TIMING] using {len(stars)} stars for solving", file=sys.stderr)

    # ── Attempt parameters ────────────────────────────────────────────────────
    #
    # noise=3.0 for all attempts: our stars come from a downsampled image
    # so centroids have ~±2-3 px uncertainty. noise=2.0 was too strict —
    # it caused 400+ second solves by exhausting all quadruplets without
    # finding a match, while noise=3.0 solves the same field in <5s.
    mid_scale   = (pixel_scale_low + pixel_scale_high) / 2.0
    half_range  = (pixel_scale_high - pixel_scale_low) / 2.0

    attempts = [
        dict(
            label="tight (given hint)",
            size_hint=astrometry.SizeHint(pixel_scale_low, pixel_scale_high),
            position_hint=astrometry.PositionHint(ra_hint_deg, dec_hint_deg,
                                                   search_radius_deg),
            noise=3.0,
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
        # Rotation from WCS CD matrix: PA of North from image up, CCW positive
        wf = m.wcs_fields
        cd21 = (wf.get('CD2_1') or (0.0, None))[0] or 0.0
        cd22 = (wf.get('CD2_2') or (0.0, None))[0] or 0.0
        rotation_deg = math.degrees(math.atan2(-cd21, cd22)) if (cd21 or cd22) else 0.0
        print(json.dumps({
            "success": True,
            "ra_deg": m.center_ra_deg,
            "dec_deg": m.center_dec_deg,
            "scale_arcsec_per_pixel": m.scale_arcsec_per_pixel,
            "rotation_deg": rotation_deg,
            "logodds": m.logodds,
            "attempt": attempt_label,
        }))
    else:
        print(json.dumps({"success": False, "error": "No solution found after all attempts"}))


def cmd_tiffhdr(input_path: str):
    """Print FITS-card metadata from a TIFF's ImageDescription tag as JSON."""
    import struct

    try:
        with open(input_path, 'rb') as f:
            raw = f.read()
    except Exception as e:
        print(json.dumps({'error': str(e)}))
        return

    bom = raw[0:2]
    if bom not in (b'II', b'MM'):
        print(json.dumps({}))
        return
    endian = '<' if bom == b'II' else '>'
    if struct.unpack_from(f'{endian}H', raw, 2)[0] != 42:
        print(json.dumps({}))
        return

    ifd_off = struct.unpack_from(f'{endian}I', raw, 4)[0]
    n_entries = struct.unpack_from(f'{endian}H', raw, ifd_off)[0]

    desc_str = ''
    for i in range(n_entries):
        base = ifd_off + 2 + 12 * i
        tag, typ, count = struct.unpack_from(f'{endian}HHI', raw, base)
        if tag != 270:   # ImageDescription
            continue
        # count * 1 byte (ASCII) – almost always > 4, so value field is an offset
        if count <= 4:
            val_bytes = raw[base + 8:base + 8 + count]
        else:
            off = struct.unpack_from(f'{endian}I', raw, base + 8)[0]
            val_bytes = raw[off:off + count]
        if val_bytes.endswith(b'\x00'):
            val_bytes = val_bytes[:-1]
        desc_str = val_bytes.decode('ascii', errors='replace')
        break

    if not desc_str:
        print(json.dumps({}))
        return

    result = {}
    for pos in range(0, len(desc_str), 80):
        card = desc_str[pos:pos + 80]
        if len(card) < 10:
            break
        key = card[:8].strip()
        if not key or key in ('END', 'COMMENT', 'HISTORY', 'SIMPLE'):
            continue
        if card[8:10] == '= ':
            raw_val = card[10:].strip()
            if raw_val.startswith("'"):
                end_q = raw_val.find("'", 1)
                val = raw_val[1:end_q].strip() if end_q > 0 else raw_val[1:].strip()
            else:
                val = raw_val.split('/')[0].strip()
            if val:
                result[key] = val

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
    elif cmd == 'snap_jpeg':
        cmd_snap_jpeg(sys.argv[2], sys.argv[3])
    elif cmd == 'fitshdr':
        cmd_fitshdr(sys.argv[2])
    elif cmd == 'tiffhdr':
        cmd_tiffhdr(sys.argv[2])
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
