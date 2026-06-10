//! Minimal standard-alphabet base64 (RFC 4648) used by OSC 52 and the kitty
//! graphics protocol.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

fn decode_digit(b: u8) -> Option<u32> {
    match b {
        b'A'..=b'Z' => Some(u32::from(b - b'A')),
        b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decodes base64, ignoring whitespace and tolerating missing padding.
/// Returns `None` on any other invalid input.
pub(crate) fn decode(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut done = false;
    for &b in data {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            done = true;
            continue;
        }
        if done {
            return None; // data after padding
        }
        acc = (acc << 6) | decode_digit(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn decode_roundtrip() {
        for input in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"\x00\xff\x80",
            b"hello world",
        ] {
            assert_eq!(decode(encode(input).as_bytes()).unwrap(), input);
        }
    }

    #[test]
    fn decode_unpadded_and_invalid() {
        assert_eq!(decode(b"Zm9vYg").unwrap(), b"foob");
        assert_eq!(decode(b"Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(decode(b"Zm9!").is_none());
    }
}
