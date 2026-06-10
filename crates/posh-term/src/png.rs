//! Minimal PNG decoder for kitty graphics `f=100` images: critical chunks
//! (IHDR/IDAT/IEND) plus PLTE and tRNS, 8-bit depth, color types 0/2/3/4/6,
//! all five scanline filters. Adam7 interlacing is rejected as unsupported.
//! Output is always RGBA8.

use crate::inflate;

/// Cap on the decoded RGBA size, matching the graphics storage quota.
const MAX_RGBA_BYTES: usize = 320 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PngError {
    /// Missing or wrong 8-byte signature.
    BadSignature,
    /// Input ended inside a chunk, or a chunk CRC mismatched.
    Truncated,
    BadCrc,
    /// Malformed IHDR, zero dimensions, or oversized image.
    BadHeader,
    /// Interlacing, non-8-bit depth, or an unknown color type.
    Unsupported,
    /// Bad IDAT stream, filter byte, or palette reference.
    BadData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedPng {
    pub width: u32,
    pub height: u32,
    /// Packed RGBA8, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

struct Header {
    width: u32,
    height: u32,
    color: u8,
}

impl Header {
    /// Samples per pixel for the supported 8-bit color types.
    fn channels(&self) -> usize {
        match self.color {
            0 => 1, // grayscale
            2 => 3, // RGB
            3 => 1, // indexed
            4 => 2, // grayscale + alpha
            _ => 4, // RGBA
        }
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((crc & 1) * 0xEDB8_8320);
        }
    }
    !crc
}

fn parse_ihdr(data: &[u8]) -> Result<Header, PngError> {
    if data.len() != 13 {
        return Err(PngError::BadHeader);
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let (depth, color, compression, filter, interlace) =
        (data[8], data[9], data[10], data[11], data[12]);
    if width == 0 || height == 0 || (width as u64) * (height as u64) * 4 > MAX_RGBA_BYTES as u64 {
        return Err(PngError::BadHeader);
    }
    if compression != 0 || filter != 0 {
        return Err(PngError::BadHeader);
    }
    if depth != 8 || interlace != 0 || !matches!(color, 0 | 2 | 3 | 4 | 6) {
        return Err(PngError::Unsupported);
    }
    Ok(Header {
        width,
        height,
        color,
    })
}

/// Paeth predictor (RFC 2083 section 6.6).
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (i32::from(a), i32::from(b), i32::from(c));
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

/// Reverses the per-scanline filters in place, dropping the filter bytes.
/// `raw` is `height` scanlines of `1 + width * bpp` bytes each.
fn unfilter(raw: &mut Vec<u8>, width: usize, height: usize, bpp: usize) -> Result<(), PngError> {
    let stride = width * bpp;
    let mut out_at = 0; // start of the previous unfiltered row in `raw`
    for row in 0..height {
        let line_at = row * (stride + 1);
        let filter = raw[line_at];
        // Shift the row left over its filter byte.
        raw.copy_within(line_at + 1..line_at + 1 + stride, row * stride);
        let cur = row * stride;
        for i in 0..stride {
            let left = if i >= bpp { raw[cur + i - bpp] } else { 0 };
            let up = if row > 0 { raw[out_at + i] } else { 0 };
            let up_left = if row > 0 && i >= bpp {
                raw[out_at + i - bpp]
            } else {
                0
            };
            let x = raw[cur + i];
            raw[cur + i] = match filter {
                0 => x,
                1 => x.wrapping_add(left),
                2 => x.wrapping_add(up),
                3 => x.wrapping_add(((u16::from(left) + u16::from(up)) / 2) as u8),
                4 => x.wrapping_add(paeth(left, up, up_left)),
                _ => return Err(PngError::BadData),
            };
        }
        out_at = cur;
    }
    raw.truncate(height * stride);
    Ok(())
}

/// Expands unfiltered samples to RGBA using the palette and transparency
/// chunks where applicable.
fn to_rgba(hdr: &Header, samples: &[u8], plte: &[u8], trns: &[u8]) -> Result<Vec<u8>, PngError> {
    let pixels = hdr.width as usize * hdr.height as usize;
    let mut out = Vec::with_capacity(pixels * 4);
    match hdr.color {
        0 => {
            // Optional tRNS: one 16-bit gray sample that becomes transparent.
            let key = (trns.len() == 2).then(|| trns[1]);
            for &g in samples {
                let a = if key == Some(g) { 0 } else { 255 };
                out.extend_from_slice(&[g, g, g, a]);
            }
        }
        2 => {
            // Optional tRNS: one 16-bit RGB sample that becomes transparent.
            let key = (trns.len() == 6).then(|| [trns[1], trns[3], trns[5]]);
            for px in samples.chunks_exact(3) {
                let a = if key == Some([px[0], px[1], px[2]]) {
                    0
                } else {
                    255
                };
                out.extend_from_slice(&[px[0], px[1], px[2], a]);
            }
        }
        3 => {
            if plte.is_empty() || plte.len() % 3 != 0 {
                return Err(PngError::BadData);
            }
            for &idx in samples {
                let i = usize::from(idx) * 3;
                let rgb = plte.get(i..i + 3).ok_or(PngError::BadData)?;
                // tRNS holds per-index alpha; absent entries are opaque.
                let a = trns.get(usize::from(idx)).copied().unwrap_or(255);
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
            }
        }
        4 => {
            for px in samples.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
        }
        _ => out.extend_from_slice(samples),
    }
    Ok(out)
}

pub(crate) fn decode(data: &[u8]) -> Result<DecodedPng, PngError> {
    let rest = data
        .strip_prefix(b"\x89PNG\r\n\x1a\n")
        .ok_or(PngError::BadSignature)?;

    let mut hdr: Option<Header> = None;
    let mut idat = Vec::new();
    let mut plte: &[u8] = &[];
    let mut trns: &[u8] = &[];
    let mut at = 0;
    loop {
        let head = rest.get(at..at + 8).ok_or(PngError::Truncated)?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let kind = &head[4..8];
        let body = rest.get(at + 8..at + 8 + len).ok_or(PngError::Truncated)?;
        let crc = rest
            .get(at + 8 + len..at + 12 + len)
            .ok_or(PngError::Truncated)?;
        if crc32(&rest[at + 4..at + 8 + len]).to_be_bytes() != crc {
            return Err(PngError::BadCrc);
        }
        match kind {
            b"IHDR" => hdr = Some(parse_ihdr(body)?),
            b"IDAT" => idat.extend_from_slice(body),
            b"PLTE" => plte = body,
            b"tRNS" => trns = body,
            b"IEND" => break,
            _ => {} // ancillary chunks are skipped
        }
        if hdr.is_none() {
            return Err(PngError::BadHeader); // IHDR must come first
        }
        at += 12 + len;
    }
    let hdr = hdr.ok_or(PngError::BadHeader)?;

    let (w, h, bpp) = (hdr.width as usize, hdr.height as usize, hdr.channels());
    let expect = h * (1 + w * bpp);
    let mut raw = inflate::zlib_decompress(&idat, expect).map_err(|_| PngError::BadData)?;
    if raw.len() != expect {
        return Err(PngError::BadData);
    }
    unfilter(&mut raw, w, h, bpp)?;
    Ok(DecodedPng {
        width: hdr.width,
        height: hdr.height,
        rgba: to_rgba(&hdr, &raw, plte, trns)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // All vectors were generated at test-authoring time with Python3
    // (zlib + struct chunk assembly); see the per-test pixel expectations.

    /// 2x2 RGB: red, green / blue, white. All rows filter 0.
    const PNG_RGB_2X2: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 2,
        0, 0, 0, 253, 212, 154, 115, 0, 0, 0, 18, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 192,
        0, 194, 12, 255, 129, 0, 0, 31, 238, 5, 251, 11, 217, 104, 139, 0, 0, 0, 0, 73, 69, 78, 68,
        174, 66, 96, 130,
    ];

    /// 1x2 RGBA: (10,20,30,128) / (200,100,50,255).
    const PNG_RGBA_1X2: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 2, 8, 6,
        0, 0, 0, 153, 129, 182, 39, 0, 0, 0, 18, 73, 68, 65, 84, 120, 156, 99, 224, 18, 145, 107,
        96, 56, 145, 98, 244, 31, 0, 10, 133, 3, 26, 120, 239, 166, 98, 0, 0, 0, 0, 73, 69, 78, 68,
        174, 66, 96, 130,
    ];

    /// 2x1 grayscale: 0, 255.
    const PNG_GRAY_2X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 1, 8, 0,
        0, 0, 0, 209, 73, 32, 86, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 248, 15, 0, 1, 2,
        1, 0, 66, 190, 188, 104, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 1x1 grayscale+alpha: gray 100, alpha 200.
    const PNG_GRAY_ALPHA_1X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4,
        0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 72, 57, 1, 0, 1, 147,
        1, 45, 129, 67, 190, 118, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 2x1 indexed: PLTE [red, blue], tRNS [128] -> alphas 128, 255.
    const PNG_INDEXED_2X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 1, 8, 3,
        0, 0, 0, 195, 252, 143, 184, 0, 0, 0, 6, 80, 76, 84, 69, 255, 0, 0, 0, 0, 255, 108, 161,
        253, 142, 0, 0, 0, 1, 116, 82, 78, 83, 128, 173, 94, 91, 70, 0, 0, 0, 11, 73, 68, 65, 84,
        120, 156, 99, 96, 96, 4, 0, 0, 4, 0, 2, 191, 122, 63, 74, 0, 0, 0, 0, 73, 69, 78, 68, 174,
        66, 96, 130,
    ];

    /// 2x1 grayscale with tRNS key 7: pixels 7 (transparent), 9 (opaque).
    const PNG_GRAY_TRNS_2X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 1, 8, 0,
        0, 0, 0, 209, 73, 32, 86, 0, 0, 0, 2, 116, 82, 78, 83, 0, 7, 232, 247, 88, 155, 0, 0, 0,
        11, 73, 68, 65, 84, 120, 156, 99, 96, 231, 4, 0, 0, 26, 0, 17, 96, 205, 36, 146, 0, 0, 0,
        0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 1x2 RGB with tRNS key (1,2,3): pixel 0 transparent, pixel 1 opaque.
    const PNG_RGB_TRNS_1X2: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 2, 8, 2,
        0, 0, 0, 22, 227, 33, 112, 0, 0, 0, 6, 116, 82, 78, 83, 0, 1, 0, 2, 0, 3, 201, 75, 171,
        245, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 96, 100, 98, 102, 224, 228, 228, 4, 0, 0,
        96, 0, 34, 36, 107, 92, 151, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 2x2 RGB, row 0 Sub filter, row 1 Up filter.
    const PNG_FILTER_SUB_UP: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 2,
        0, 0, 0, 253, 212, 154, 115, 0, 0, 0, 22, 73, 68, 65, 84, 120, 156, 99, 228, 18, 145, 227,
        98, 96, 96, 98, 96, 101, 248, 205, 202, 10, 0, 6, 99, 1, 84, 39, 239, 161, 71, 0, 0, 0, 0,
        73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 2x2 grayscale, row 0 Average filter, row 1 Paeth filter.
    const PNG_FILTER_AVG_PAETH: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 0,
        0, 0, 0, 87, 221, 82, 248, 0, 0, 0, 14, 73, 68, 65, 84, 120, 156, 99, 78, 177, 97, 249, 38,
        2, 0, 5, 8, 1, 178, 45, 47, 141, 210, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 1x1 grayscale with interlace=1 (Adam7).
    const PNG_INTERLACED: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 0,
        0, 0, 1, 77, 121, 171, 195, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 96, 0, 0, 0, 2, 0,
        1, 72, 175, 164, 113, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 1x1 grayscale with bit depth 16.
    const PNG_16BIT: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 16,
        0, 0, 0, 0, 106, 238, 71, 22, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 96, 0, 0, 0,
        3, 0, 1, 184, 173, 58, 99, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    #[test]
    fn rgb_decodes_to_rgba() {
        let png = decode(PNG_RGB_2X2).unwrap();
        assert_eq!((png.width, png.height), (2, 2));
        #[rustfmt::skip]
        assert_eq!(png.rgba, [
            255, 0, 0, 255,   0, 255, 0, 255,
            0, 0, 255, 255,   255, 255, 255, 255,
        ]);
    }

    #[test]
    fn rgba_passthrough() {
        let png = decode(PNG_RGBA_1X2).unwrap();
        assert_eq!((png.width, png.height), (1, 2));
        assert_eq!(png.rgba, [10, 20, 30, 128, 200, 100, 50, 255]);
    }

    #[test]
    fn grayscale() {
        let png = decode(PNG_GRAY_2X1).unwrap();
        assert_eq!(png.rgba, [0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn grayscale_alpha() {
        let png = decode(PNG_GRAY_ALPHA_1X1).unwrap();
        assert_eq!(png.rgba, [100, 100, 100, 200]);
    }

    #[test]
    fn indexed_with_palette_alpha() {
        let png = decode(PNG_INDEXED_2X1).unwrap();
        assert_eq!(png.rgba, [255, 0, 0, 128, 0, 0, 255, 255]);
    }

    #[test]
    fn grayscale_transparency_key() {
        let png = decode(PNG_GRAY_TRNS_2X1).unwrap();
        assert_eq!(png.rgba, [7, 7, 7, 0, 9, 9, 9, 255]);
    }

    #[test]
    fn rgb_transparency_key() {
        let png = decode(PNG_RGB_TRNS_1X2).unwrap();
        assert_eq!(png.rgba, [1, 2, 3, 0, 9, 9, 9, 255]);
    }

    #[test]
    fn sub_and_up_filters() {
        let png = decode(PNG_FILTER_SUB_UP).unwrap();
        #[rustfmt::skip]
        assert_eq!(png.rgba, [
            10, 20, 30, 255,   20, 20, 30, 255,
            10, 25, 30, 255,   15, 25, 35, 255,
        ]);
    }

    #[test]
    fn average_and_paeth_filters() {
        let png = decode(PNG_FILTER_AVG_PAETH).unwrap();
        #[rustfmt::skip]
        assert_eq!(png.rgba, [
            100, 100, 100, 255,   110, 110, 110, 255,
            90, 90, 90, 255,      120, 120, 120, 255,
        ]);
    }

    #[test]
    fn interlaced_is_unsupported() {
        assert_eq!(decode(PNG_INTERLACED), Err(PngError::Unsupported));
    }

    #[test]
    fn sixteen_bit_is_unsupported() {
        assert_eq!(decode(PNG_16BIT), Err(PngError::Unsupported));
    }

    #[test]
    fn bad_signature_and_truncation() {
        assert_eq!(decode(b"not a png"), Err(PngError::BadSignature));
        assert_eq!(decode(&PNG_RGB_2X2[..30]), Err(PngError::Truncated));
        assert_eq!(decode(&[]), Err(PngError::BadSignature));
    }

    #[test]
    fn chunk_crc_is_checked() {
        let mut bad = PNG_RGB_2X2.to_vec();
        bad[50] ^= 0xFF; // inside IDAT
        assert_eq!(decode(&bad), Err(PngError::BadCrc));
    }

    #[test]
    fn corrupt_idat_stream() {
        // A corrupt zlib stream inside IDAT surfaces as BadData (and so as
        // an EBADPNG acknowledgement at the graphics layer).
        let mut bad = PNG_GRAY_2X1.to_vec();
        // Rewrite the IDAT body and fix up its CRC so only inflate fails.
        let idat_body = 41..52; // 11 bytes after "IDAT"
        for b in &mut bad[idat_body] {
            *b = 0xAA;
        }
        let crc = crc32(&bad[37..52]).to_be_bytes();
        bad[52..56].copy_from_slice(&crc);
        assert_eq!(decode(&bad), Err(PngError::BadData));
    }
}
