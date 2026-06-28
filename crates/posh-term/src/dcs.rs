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
                let resp = format!("\x1bP0+r{name_hex}\x1b\\");
                self.respond(&resp);
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

fn hex_decode(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        out.push(u8::from_str_radix(&s[i..i + 2], 16).ok()?);
    }
    String::from_utf8(out).ok()
}

fn hex_encode(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02X}")).collect()
}
