//! DEFLATE (RFC 1951) and zlib (RFC 1950) decompression, used by the kitty
//! graphics protocol (`o=z` payloads) and the PNG decoder. Supports stored,
//! fixed-Huffman, and dynamic-Huffman blocks; output is capped by a caller
//! limit so a small stream cannot expand without bound.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InflateError {
    /// Input ended before the stream was complete.
    Truncated,
    /// Bad zlib CMF/FLG header, or a preset dictionary was requested.
    BadHeader,
    /// Reserved block type 3.
    BadBlockType,
    /// Stored block LEN/NLEN mismatch.
    BadStoredLength,
    /// Over-subscribed or otherwise invalid Huffman code lengths.
    BadHuffman,
    /// A code that does not map to any symbol, or an invalid symbol
    /// (e.g. length code 286/287).
    BadSymbol,
    /// Back-reference distance reaches before the start of output.
    BadDistance,
    /// Adler-32 checksum mismatch.
    BadChecksum,
    /// Decompressed output would exceed the caller's limit.
    TooLarge,
}

/// Base lengths and extra bits for length codes 257..=285.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Base distances and extra bits for distance codes 0..=29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Order in which code-length-code lengths appear in a dynamic block header.
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Canonical Huffman decoding table: symbol counts per code length plus the
/// symbols sorted by (length, symbol), as in RFC 1951 section 3.2.2.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, InflateError> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return Err(InflateError::BadHuffman);
            }
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        // Reject over-subscribed codes (incomplete ones are tolerated; zlib
        // emits them for degenerate distance alphabets).
        let mut left = 1i32;
        for &count in &counts[1..] {
            left = (left << 1) - i32::from(count);
            if left < 0 {
                return Err(InflateError::BadHuffman);
            }
        }
        let mut offsets = [0u16; 16];
        for len in 1..15 {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; lengths.iter().filter(|&&l| l != 0).count()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[usize::from(offsets[usize::from(l)])] = sym as u16;
                offsets[usize::from(l)] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn fixed_literals() -> Huffman {
        let mut lengths = [8u8; 288];
        lengths[144..256].fill(9);
        lengths[256..280].fill(7);
        Huffman::new(&lengths).expect("fixed literal code is valid")
    }

    fn fixed_distances() -> Huffman {
        Huffman::new(&[5u8; 30]).expect("fixed distance code is valid")
    }
}

/// LSB-first bit reader over a byte slice.
struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position.
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn bits(&mut self, n: u32) -> Result<u32, InflateError> {
        let mut out = 0u32;
        for i in 0..n {
            let byte = self
                .data
                .get(self.pos >> 3)
                .ok_or(InflateError::Truncated)?;
            out |= u32::from((byte >> (self.pos & 7)) & 1) << i;
            self.pos += 1;
        }
        Ok(out)
    }

    /// Walks a canonical Huffman code one bit at a time.
    fn decode(&mut self, h: &Huffman) -> Result<u16, InflateError> {
        let mut code = 0usize;
        let mut first = 0usize;
        let mut index = 0usize;
        for len in 1..=15 {
            code |= self.bits(1)? as usize;
            let count = usize::from(h.counts[len]);
            if code < first + count {
                return Ok(h.symbols[index + code - first]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(InflateError::BadSymbol)
    }
}

/// Reads the code-length tables of a dynamic-Huffman block.
fn dynamic_tables(r: &mut BitReader) -> Result<(Huffman, Huffman), InflateError> {
    let hlit = r.bits(5)? as usize + 257;
    let hdist = r.bits(5)? as usize + 1;
    let hclen = r.bits(4)? as usize + 4;
    let mut clen_lengths = [0u8; 19];
    for &slot in CLEN_ORDER.iter().take(hclen) {
        clen_lengths[slot] = r.bits(3)? as u8;
    }
    let clen = Huffman::new(&clen_lengths)?;
    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        match r.decode(&clen)? {
            sym @ 0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(InflateError::BadHuffman);
                }
                let prev = lengths[i - 1];
                for _ in 0..3 + r.bits(2)? {
                    *lengths.get_mut(i).ok_or(InflateError::BadHuffman)? = prev;
                    i += 1;
                }
            }
            17 => i += 3 + r.bits(3)? as usize,
            18 => i += 11 + r.bits(7)? as usize,
            _ => return Err(InflateError::BadHuffman),
        }
        if i > lengths.len() {
            return Err(InflateError::BadHuffman);
        }
    }
    if lengths[256] == 0 {
        return Err(InflateError::BadHuffman); // end-of-block must be codable
    }
    Ok((
        Huffman::new(&lengths[..hlit])?,
        Huffman::new(&lengths[hlit..])?,
    ))
}

/// Decompresses one compressed (fixed or dynamic) block into `out`.
fn inflate_compressed(
    r: &mut BitReader,
    out: &mut Vec<u8>,
    limit: usize,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), InflateError> {
    loop {
        let sym = r.decode(lit)?;
        match sym {
            0..=255 => {
                if out.len() >= limit {
                    return Err(InflateError::TooLarge);
                }
                out.push(sym as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let idx = usize::from(sym - 257);
                let len =
                    usize::from(LENGTH_BASE[idx]) + r.bits(u32::from(LENGTH_EXTRA[idx]))? as usize;
                let dsym = usize::from(r.decode(dist)?);
                if dsym >= 30 {
                    return Err(InflateError::BadSymbol);
                }
                let d =
                    usize::from(DIST_BASE[dsym]) + r.bits(u32::from(DIST_EXTRA[dsym]))? as usize;
                if d > out.len() {
                    return Err(InflateError::BadDistance);
                }
                if out.len() + len > limit {
                    return Err(InflateError::TooLarge);
                }
                for _ in 0..len {
                    let b = out[out.len() - d];
                    out.push(b); // byte-by-byte: ranges may overlap
                }
            }
            _ => return Err(InflateError::BadSymbol),
        }
    }
}

/// Decompresses a raw DEFLATE stream. Returns the output and the number of
/// input bytes consumed (the stream may be followed by trailer bytes).
pub(crate) fn inflate(data: &[u8], limit: usize) -> Result<(Vec<u8>, usize), InflateError> {
    let mut r = BitReader { data, pos: 0 };
    let mut out = Vec::new();
    loop {
        let bfinal = r.bits(1)?;
        match r.bits(2)? {
            0 => {
                // Stored: byte-align, then LEN and its one's complement.
                r.pos = (r.pos + 7) & !7;
                let len = r.bits(16)? as usize;
                let nlen = r.bits(16)? as usize;
                if len ^ nlen != 0xFFFF {
                    return Err(InflateError::BadStoredLength);
                }
                if out.len() + len > limit {
                    return Err(InflateError::TooLarge);
                }
                let start = r.pos >> 3;
                let bytes = data
                    .get(start..start + len)
                    .ok_or(InflateError::Truncated)?;
                out.extend_from_slice(bytes);
                r.pos += len * 8;
            }
            1 => {
                let (lit, dist) = (Huffman::fixed_literals(), Huffman::fixed_distances());
                inflate_compressed(&mut r, &mut out, limit, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut r)?;
                inflate_compressed(&mut r, &mut out, limit, &lit, &dist)?;
            }
            _ => return Err(InflateError::BadBlockType),
        }
        if bfinal == 1 {
            return Ok((out, r.pos.div_ceil(8)));
        }
    }
}

pub(crate) fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a = 1u32;
    let mut b = 0u32;
    // 5552 is the largest chunk for which the sums fit in u32 (zlib's NMAX).
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Decompresses a zlib (RFC 1950) stream, verifying the header and the
/// Adler-32 trailer. Output larger than `limit` fails with `TooLarge`.
pub(crate) fn zlib_decompress(data: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    let [cmf, flg, rest @ ..] = data else {
        return Err(InflateError::Truncated);
    };
    let (cmf, flg) = (*cmf, *flg);
    let header = u16::from(cmf) << 8 | u16::from(flg);
    // CM must be 8 (deflate); FDICT (preset dictionaries) is unsupported.
    if cmf & 0x0F != 8 || header % 31 != 0 || flg & 0x20 != 0 {
        return Err(InflateError::BadHeader);
    }
    let (out, used) = inflate(rest, limit)?;
    let trailer = rest.get(used..used + 4).ok_or(InflateError::Truncated)?;
    let want = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    if adler32(&out) != want {
        return Err(InflateError::BadChecksum);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors below were produced with Python3's zlib at test-authoring time
    // (`zlib.compress(...)` / `compressobj(..., zlib.Z_FIXED)`).

    /// `zlib.compressobj(6, DEFLATED, 15, 8, Z_FIXED)` of b"abc" * 6.
    const ZLIB_FIXED: &[u8] = &[120, 1, 75, 76, 74, 78, 68, 69, 0, 65, 124, 6, 229];

    /// `zlib.compress(LOREM * 8, 9)`: a dynamic-Huffman (btype 2) block.
    const ZLIB_DYNAMIC: &[u8] = &[
        120, 218, 237, 205, 193, 13, 3, 49, 8, 68, 209, 86, 166, 128, 40, 149, 108, 19, 196, 160,
        21, 146, 193, 94, 3, 253, 199, 82, 106, 200, 205, 231, 209, 159, 119, 141, 37, 6, 157, 81,
        6, 30, 125, 44, 132, 38, 200, 36, 95, 104, 195, 67, 90, 74, 214, 2, 177, 78, 141, 166, 126,
        67, 186, 238, 49, 132, 119, 0, 209, 10, 27, 140, 20, 155, 59, 86, 111, 202, 202, 229, 137,
        74, 116, 250, 236, 123, 72, 254, 174, 5, 70, 183, 19, 168, 235, 83, 244, 198, 117, 236, 99,
        31, 251, 175, 246, 23, 63, 227, 109, 64,
    ];
    const LOREM: &[u8] = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
        sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. ";

    /// `compressobj(9)` with a Z_FULL_FLUSH between two halves: multiple
    /// deflate blocks, including an empty stored sync block.
    const ZLIB_MULTI_BLOCK: &[u8] = &[
        120, 218, 74, 203, 44, 42, 46, 81, 200, 72, 204, 73, 83, 72, 163, 61, 19, 0, 0, 0, 255,
        255, 43, 78, 77, 206, 207, 75, 81, 200, 72, 204, 73, 83, 40, 166, 35, 27, 0, 233, 110, 83,
        133,
    ];

    /// Hand-built single stored block: BFINAL=1, BTYPE=00, LEN=5.
    fn stored_raw() -> Vec<u8> {
        let mut v = vec![0x01, 0x05, 0x00, 0xFA, 0xFF];
        v.extend_from_slice(b"hello");
        v
    }

    fn zlib_wrap(deflate: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x78, 0x01];
        v.extend_from_slice(deflate);
        v.extend_from_slice(&adler32(payload).to_be_bytes());
        v
    }

    #[test]
    fn stored_block() {
        let (out, used) = inflate(&stored_raw(), 1 << 20).unwrap();
        assert_eq!(out, b"hello");
        assert_eq!(used, 10);
    }

    #[test]
    fn stored_block_zlib() {
        let stream = zlib_wrap(&stored_raw(), b"hello");
        assert_eq!(zlib_decompress(&stream, 1 << 20).unwrap(), b"hello");
    }

    #[test]
    fn fixed_huffman_block() {
        let out = zlib_decompress(ZLIB_FIXED, 1 << 20).unwrap();
        assert_eq!(out, b"abcabcabcabcabcabc");
    }

    #[test]
    fn dynamic_huffman_block() {
        let out = zlib_decompress(ZLIB_DYNAMIC, 1 << 20).unwrap();
        assert_eq!(out, LOREM.repeat(8));
    }

    #[test]
    fn multi_block_stream() {
        let out = zlib_decompress(ZLIB_MULTI_BLOCK, 1 << 20).unwrap();
        let mut want = b"first half ".repeat(10);
        want.extend_from_slice(&b"second half ".repeat(10));
        assert_eq!(out, want);
    }

    #[test]
    fn overlapping_match_copy() {
        // Hand-built fixed block: BFINAL=1 BTYPE=01, literal 'A' (fixed code
        // 0x30+0x41 = 0x71), a length=10 distance=1 match (an overlapping
        // copy that must be done byte-by-byte), then end-of-block. Verified
        // against `zlib.decompress(..., -15)`.
        let (out, used) = inflate(&[0x73, 0x44, 0x00, 0x00], 64).unwrap();
        assert_eq!(out, b"AAAAAAAAAAA"); // 1 literal + 10 copies
        assert_eq!(used, 4);
    }

    #[test]
    fn truncated_stream() {
        assert_eq!(
            zlib_decompress(&ZLIB_DYNAMIC[..20], 1 << 20),
            Err(InflateError::Truncated)
        );
        assert_eq!(zlib_decompress(&[], 16), Err(InflateError::Truncated));
        assert_eq!(zlib_decompress(&[0x78], 16), Err(InflateError::Truncated));
    }

    #[test]
    fn bad_zlib_header() {
        // CM != 8.
        assert_eq!(
            zlib_decompress(&[0x77, 0x01, 0, 0], 16),
            Err(InflateError::BadHeader)
        );
        // FCHECK failure.
        assert_eq!(
            zlib_decompress(&[0x78, 0x02, 0, 0], 16),
            Err(InflateError::BadHeader)
        );
        // FDICT set (0x78 0x3C passes the %31 check with bit 5 set).
        assert_eq!(
            zlib_decompress(&[0x78, 0x3C, 0, 0, 0, 0], 16),
            Err(InflateError::BadHeader)
        );
    }

    #[test]
    fn bad_adler_checksum() {
        let mut stream = zlib_wrap(&stored_raw(), b"hello");
        let n = stream.len();
        stream[n - 1] ^= 0xFF;
        assert_eq!(
            zlib_decompress(&stream, 1 << 20),
            Err(InflateError::BadChecksum)
        );
    }

    #[test]
    fn stored_length_complement_mismatch() {
        let mut raw = stored_raw();
        raw[3] = 0x00; // NLEN no longer ~LEN
        assert_eq!(inflate(&raw, 16), Err(InflateError::BadStoredLength));
    }

    #[test]
    fn reserved_block_type() {
        // BFINAL=1, BTYPE=11.
        assert_eq!(inflate(&[0b0000_0111], 16), Err(InflateError::BadBlockType));
    }

    #[test]
    fn output_limit_enforced() {
        assert_eq!(
            zlib_decompress(ZLIB_DYNAMIC, 100),
            Err(InflateError::TooLarge)
        );
        assert_eq!(inflate(&stored_raw(), 4), Err(InflateError::TooLarge));
        // Exactly at the limit is fine.
        assert!(zlib_decompress(ZLIB_DYNAMIC, LOREM.len() * 8).is_ok());
    }

    #[test]
    fn adler32_vectors() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E60398);
        assert_eq!(adler32(&[0xFFu8; 7000]), 0xD6483E3E); // multi-chunk
    }
}
