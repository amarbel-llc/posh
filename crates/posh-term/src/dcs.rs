//! DCS dispatch: DECRQSS and XTGETTCAP.

use crate::terminal::Terminal;

impl Terminal {
    pub(crate) fn dcs_dispatch(
        &mut self,
        _params: &[Vec<u16>],
        intermediates: &[u8],
        final_byte: u8,
        data: &[u8],
    ) {
        match (intermediates.first().copied(), final_byte) {
            (Some(b'$'), b'q') => self.decrqss(data),
            (Some(b'+'), b'q') => self.xtgettcap(data),
            _ => {}
        }
    }

    /// DECRQSS: request status string. Replies `DCS 1 $ r <value> ST` for
    /// recognized settings, `DCS 0 $ r ST` otherwise.
    fn decrqss(&mut self, data: &[u8]) {
        let resp = match data {
            b"m" => Some(format!("{}m", crate::dump::sgr_params(&self.cursor.style))),
            b" q" => Some(format!("{} q", self.cursor_style_raw)),
            b"r" => {
                let (top, bot) = self.region();
                Some(format!("{};{}r", top + 1, bot + 1))
            }
            _ => None,
        };
        match resp {
            Some(v) => {
                let r = format!("\x1bP1$r{v}\x1b\\");
                self.respond(&r);
            }
            None => self.respond("\x1bP0$r\x1b\\"),
        }
    }

    /// XTGETTCAP: terminfo capability query with hex-encoded names.
    fn xtgettcap(&mut self, data: &[u8]) {
        let Ok(s) = std::str::from_utf8(data) else {
            return;
        };
        for name_hex in s.split(';') {
            let Some(name) = hex_decode(name_hex) else {
                // SECURITY: an un-decodable `name_hex` is caller-controlled
                // text that was never validated as hex, so it may contain
                // control bytes (CR, BEL, ...) the DCS string syntax
                // otherwise allows through unchanged. This reply used to
                // echo it back verbatim; a session daemon or the roaming
                // server then writes `take_responses()` straight into the
                // PTY, so whatever process printed the malformed query could
                // plant bytes that land on the shell's own input. Reply with
                // an EMPTY Pt instead — never echo unvalidated bytes. A
                // conforming client always sends valid hex, so this changes
                // nothing for real callers (vim/tmux terminfo probing).
                self.respond("\x1bP0+r\x1b\\");
                continue;
            };
            let value: Option<&str> = match name.as_str() {
                "colors" | "Co" => Some("256"),
                // Truecolor capability flags: present, no value.
                "RGB" | "Tc" => Some(""),
                _ => None,
            };
            let resp = match value {
                Some("") => format!("\x1bP1+r{}\x1b\\", hex_encode(&name)),
                Some(v) => {
                    format!("\x1bP1+r{}={}\x1b\\", hex_encode(&name), hex_encode(v))
                }
                None => format!("\x1bP0+r{}\x1b\\", hex_encode(&name)),
            };
            self.respond(&resp);
        }
    }
}

/// Decodes a hex-pair-encoded capability name. Hex digits are always ASCII,
/// so a non-ASCII `s` can never be valid XTGETTCAP input; rejecting it up
/// front (rather than byte-range-slicing `&str` by raw indices, as the
/// previous implementation did) is what keeps every subsequent index a char
/// boundary. SECURITY: the old `&s[i..i + 2]` slicing panicked — and so
/// crashed the whole session daemon — whenever a multi-byte UTF-8 character
/// in caller-controlled data landed
/// across a two-byte stride boundary (e.g. "aéa", a 4-byte string whose
/// middle character spans bytes 1..3). Working byte-by-byte here avoids
/// `&str` slicing entirely, so the panic class cannot recur regardless of
/// what bytes are fed in.
fn hex_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for &[hi, lo] in bytes.as_chunks::<2>().0 {
        let hi = (hi as char).to_digit(16)?;
        let lo = (lo as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    String::from_utf8(out).ok()
}

fn hex_encode(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::hex_decode;
    use crate::terminal::Terminal;

    fn responses_to(input: &[u8]) -> Vec<u8> {
        let mut t = Terminal::new(24, 80);
        t.process(input);
        t.take_responses()
    }

    /// Before the fix: `hex_decode` sliced `&str` by raw byte index and
    /// panicked on a multi-byte char straddling a stride boundary. This
    /// drives it through the real parser (DCS `+q`), so it also proves the
    /// panic reached the daemon's whole `Terminal::process` call, not just
    /// the helper in isolation.
    #[test]
    fn xtgettcap_with_misaligned_multibyte_name_does_not_panic() {
        let query = "\x1bP+qaéa\x1b\\".as_bytes();
        let resp = responses_to(query);
        // Invalid (non-hex) name: an unsupported-capability reply, no panic.
        assert_eq!(resp, b"\x1bP0+r\x1b\\");
    }

    /// Before the fix: an un-decodable `name_hex` was echoed back verbatim,
    /// so any control byte the DCS string syntax lets through (CR, BEL, ...)
    /// rode the reply unchanged — and that reply is written straight into the
    /// PTY by whatever is reading `take_responses()` (the session daemon, the
    /// roaming server). Property under test: no XTGETTCAP reply may carry a
    /// caller-chosen byte that was never validated as hex.
    #[test]
    fn xtgettcap_invalid_name_never_echoes_caller_bytes() {
        for payload in [
            &b"\x1bP+q\rpwned\x1b\\"[..],
            &b"\x1bP+qYES\x07NO\x1b\\"[..],
            &b"\x1bP+qAB;\r\rCD\x1b\\"[..],
            // Odd length: hex-shaped but invalid, still must not echo.
            &b"\x1bP+qABC\x1b\\"[..],
        ] {
            let resp = responses_to(payload);
            assert!(
                !resp.contains(&0x0D) && !resp.contains(&0x07),
                "reply {resp:?} carries a caller-chosen control byte for input {payload:?}"
            );
            // Every ';'-separated field that fails to hex-decode must reply
            // with exactly the canned, byte-chosen-by-no-one "unsupported"
            // form — never anything built from the field's own bytes. A
            // multi-field query (e.g. "AB;\r\rCD") replies once per field, so
            // this checks the concatenation is made up ONLY of that one safe
            // shape repeated, not a mix that happens to omit the two probe
            // bytes above while still echoing other caller bytes.
            let canned = b"\x1bP0+r\x1b\\";
            assert!(
                resp.len().is_multiple_of(canned.len())
                    && resp.chunks(canned.len()).all(|c| c == canned),
                "reply {resp:?} is not made up only of the canned unsupported reply, for input {payload:?}"
            );
        }
    }

    /// Regression: a conforming client's valid hex-encoded queries are
    /// unaffected by the fix (vim/tmux terminfo probing for Co/RGB/Tc).
    #[test]
    fn xtgettcap_valid_names_are_unchanged() {
        // "Co" = 43 6F
        assert_eq!(responses_to(b"\x1bP+q436F\x1b\\"), b"\x1bP1+r436F=323536\x1b\\");
        // "RGB" = 52 47 42
        assert_eq!(responses_to(b"\x1bP+q524742\x1b\\"), b"\x1bP1+r524742\x1b\\");
        // "xx" (unsupported, but validly hex) = 78 78
        assert_eq!(responses_to(b"\x1bP+q7878\x1b\\"), b"\x1bP0+r7878\x1b\\");
    }

    #[test]
    fn hex_decode_rejects_non_ascii_instead_of_panicking() {
        assert_eq!(hex_decode("aéa"), None);
        assert_eq!(hex_decode("436F"), Some("Co".to_string()));
        assert_eq!(hex_decode("ABC"), None); // odd length
        assert_eq!(hex_decode("GG"), None); // not hex digits
    }
}
