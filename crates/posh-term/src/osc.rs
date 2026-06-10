//! OSC (Operating System Command) dispatch.

use crate::base64;
use crate::screen::SemanticMark;
use crate::terminal::{Hyperlink, Terminal};

impl Terminal {
    pub(crate) fn osc_dispatch(&mut self, data: &[u8], bel: bool) {
        let s = String::from_utf8_lossy(data);
        let (code, rest) = match s.split_once(';') {
            Some((c, r)) => (c, r),
            None => (s.as_ref(), ""),
        };
        let Ok(code) = code.parse::<u16>() else {
            return;
        };
        match code {
            0 => {
                self.title = rest.to_string();
                self.icon_title = rest.to_string();
                self.touch();
            }
            1 => self.icon_title = rest.to_string(),
            2 => {
                self.title = rest.to_string();
                self.touch();
            }
            4 => self.osc_palette(rest, bel),
            7 => {
                self.pwd = parse_file_uri(rest);
                self.touch();
            }
            8 => self.osc_hyperlink(rest),
            9 => {
                self.last_notification = Some(rest.to_string());
                self.touch();
            }
            10 => self.osc_dynamic_color(10, rest, bel),
            11 => self.osc_dynamic_color(11, rest, bel),
            12 => self.osc_dynamic_color(12, rest, bel),
            22 => self.pointer_shape = rest.to_string(),
            52 => self.osc_clipboard(rest, bel),
            66 => self.osc_text_size(rest),
            99 => {
                // Kitty notification: metadata;payload.
                let body = rest.split_once(';').map(|(_, b)| b).unwrap_or(rest);
                self.last_notification = Some(body.to_string());
                self.touch();
            }
            104 => {
                if rest.is_empty() {
                    self.palette = crate::cell::default_palette();
                } else {
                    for part in rest.split(';') {
                        if let Ok(i) = part.parse::<u16>() {
                            if i < 256 {
                                self.palette[i as usize] =
                                    crate::cell::default_palette_entry(i as u8);
                            }
                        }
                    }
                }
                self.touch();
            }
            110 => self.fg_color = None,
            111 => self.bg_color = None,
            112 => self.cursor_color = None,
            133 => self.osc_prompt_mark(rest),
            _ => {}
        }
    }

    /// OSC 4: `4;index;spec` pairs; spec `?` queries.
    fn osc_palette(&mut self, rest: &str, bel: bool) {
        let mut parts = rest.split(';');
        while let (Some(idx), Some(spec)) = (parts.next(), parts.next()) {
            let Ok(i) = idx.parse::<u16>() else {
                continue;
            };
            if i >= 256 {
                continue;
            }
            if spec == "?" {
                let (r, g, b) = self.palette[i as usize];
                let resp = format!("\x1b]4;{i};{}{}", format_rgb16(r, g, b), st(bel));
                self.respond(&resp);
            } else if let Some(rgb) = parse_color_spec(spec) {
                self.palette[i as usize] = rgb;
                self.touch();
            }
        }
    }

    /// OSC 10/11/12: dynamic fg/bg/cursor color, with `?` query form.
    fn osc_dynamic_color(&mut self, code: u16, rest: &str, bel: bool) {
        let slot = match code {
            10 => &mut self.fg_color,
            11 => &mut self.bg_color,
            _ => &mut self.cursor_color,
        };
        if rest == "?" {
            // Unset colors report conventional defaults.
            let (r, g, b) = slot.unwrap_or(if code == 11 {
                (0, 0, 0)
            } else {
                (255, 255, 255)
            });
            let resp = format!("\x1b]{code};{}{}", format_rgb16(r, g, b), st(bel));
            self.respond(&resp);
        } else if let Some(rgb) = parse_color_spec(rest) {
            *slot = Some(rgb);
            self.touch();
        }
    }

    /// OSC 8: `8;params;uri` where params may carry `id=`.
    fn osc_hyperlink(&mut self, rest: &str) {
        let (params, uri) = rest.split_once(';').unwrap_or(("", rest));
        if uri.is_empty() {
            self.cursor.hyperlink = 0;
            return;
        }
        let id = params
            .split(':')
            .find_map(|kv| kv.strip_prefix("id="))
            .unwrap_or("")
            .to_string();
        // Reuse an existing slot for the same (id, uri) pair.
        if let Some((&k, _)) = self
            .hyperlinks
            .iter()
            .find(|(_, h)| h.id == id && h.uri == uri)
        {
            self.cursor.hyperlink = k;
            return;
        }
        self.next_hyperlink += 1;
        self.hyperlinks.insert(
            self.next_hyperlink,
            Hyperlink {
                id,
                uri: uri.to_string(),
            },
        );
        self.cursor.hyperlink = self.next_hyperlink;
    }

    /// OSC 52: `52;selection;base64-data`, with `?` query form. The
    /// selection parameter names one or more targets — `c` clipboard, `p`
    /// primary, `s` select (empty defaults to `c`). A set updates every
    /// named slot; a query answers from the first.
    fn osc_clipboard(&mut self, rest: &str, bel: bool) {
        let (sel, payload) = rest.split_once(';').unwrap_or(("", rest));
        let mut kinds: Vec<char> = sel
            .chars()
            .filter(|c| matches!(c, 'c' | 'p' | 's'))
            .collect();
        if kinds.is_empty() {
            kinds.push('c');
        }
        if payload == "?" {
            let kind = kinds[0];
            let data = match kind {
                'p' => &self.primary_selection,
                's' => &self.select_selection,
                _ => &self.clipboard,
            };
            let resp = format!("\x1b]52;{kind};{}{}", base64::encode(data), st(bel));
            self.respond(&resp);
        } else if let Some(decoded) = base64::decode(payload.as_bytes()) {
            for kind in kinds {
                let slot = match kind {
                    'p' => &mut self.primary_selection,
                    's' => &mut self.select_selection,
                    _ => &mut self.clipboard,
                };
                *slot = decoded.clone();
            }
            self.touch();
        }
    }

    /// OSC 66 (kitty text-sizing protocol): `metadata;text` where metadata
    /// is colon-separated `key=value` pairs. Partial support: the text is
    /// inserted as ordinary cells, a `w=` key advances the cursor to the
    /// declared cell width, and the raw payload is kept for callers
    /// ([`Terminal::last_text_size`]); scale keys do not multiply glyphs.
    fn osc_text_size(&mut self, rest: &str) {
        let (meta, text) = rest.split_once(';').unwrap_or(("", rest));
        self.last_text_size = Some(rest.to_string());
        if text.is_empty() {
            return;
        }
        let start = self.cursor.col;
        for ch in text.chars() {
            self.print(ch);
        }
        let width = meta.split(':').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == "w" {
                v.parse::<u16>().ok()
            } else {
                None
            }
        });
        if let Some(w) = width.filter(|&w| w > 0) {
            self.cursor.col = start.saturating_add(w).min(self.cols() - 1);
            self.cursor.pending_wrap = false;
        }
        self.touch();
    }

    /// OSC 133: shell integration prompt marks (fish emits these).
    fn osc_prompt_mark(&mut self, rest: &str) {
        let kind = rest.split(';').next().unwrap_or("");
        let mark = match kind {
            "A" => SemanticMark::PromptStart,
            "B" => SemanticMark::InputStart,
            "C" => SemanticMark::OutputStart,
            "D" => SemanticMark::CommandEnd,
            _ => return,
        };
        let row = self.cursor.row;
        self.scr_mut().row_mut(row).mark = Some(mark);
        self.touch();
    }
}

fn st(bel: bool) -> &'static str {
    if bel {
        "\x07"
    } else {
        "\x1b\\"
    }
}

/// 16-bit-per-channel color report form used by OSC query replies.
fn format_rgb16(r: u8, g: u8, b: u8) -> String {
    let up = |v: u8| u16::from(v) * 257;
    format!("rgb:{:04x}/{:04x}/{:04x}", up(r), up(g), up(b))
}

/// Parses X-style color specs: `rgb:RR/GG/BB` (1-4 hex digits per channel)
/// and `#RGB` / `#RRGGBB`.
fn parse_color_spec(spec: &str) -> Option<(u8, u8, u8)> {
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut chans = rest.split('/');
        let mut out = [0u8; 3];
        for slot in &mut out {
            let chan = chans.next()?;
            if chan.is_empty() || chan.len() > 4 {
                return None;
            }
            let v = u32::from_str_radix(chan, 16).ok()?;
            // Scale an n-digit value to 8 bits.
            let max = (1u32 << (4 * chan.len() as u32)) - 1;
            *slot = ((v * 255 + max / 2) / max) as u8;
        }
        if chans.next().is_some() {
            return None;
        }
        return Some((out[0], out[1], out[2]));
    }
    if let Some(hex) = spec.strip_prefix('#') {
        let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
        return match hex.len() {
            3 => Some((digit(0)? * 17, digit(1)? * 17, digit(2)? * 17)),
            6 => {
                let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
                Some((byte(0)?, byte(2)?, byte(4)?))
            }
            _ => None,
        };
    }
    None
}

/// Extracts and percent-decodes the path of an OSC 7 `file://host/path`
/// URI; non-file values are stored verbatim.
fn parse_file_uri(value: &str) -> String {
    let Some(rest) = value.strip_prefix("file://") else {
        return value.to_string();
    };
    let path = match rest.find('/') {
        Some(i) => &rest[i..],
        None => "/",
    };
    percent_decode(path)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_spec_forms() {
        assert_eq!(parse_color_spec("rgb:ff/00/80"), Some((255, 0, 128)));
        assert_eq!(parse_color_spec("rgb:ffff/0000/8080"), Some((255, 0, 128)));
        assert_eq!(parse_color_spec("rgb:f/0/8"), Some((255, 0, 136)));
        assert_eq!(parse_color_spec("#ff0080"), Some((255, 0, 128)));
        assert_eq!(parse_color_spec("#f08"), Some((255, 0, 136)));
        assert_eq!(parse_color_spec("bogus"), None);
        assert_eq!(parse_color_spec("rgb:ff/00"), None);
    }

    #[test]
    fn file_uri_parsing() {
        assert_eq!(parse_file_uri("file://host/home/user"), "/home/user");
        assert_eq!(parse_file_uri("file:///tmp/a%20b"), "/tmp/a b");
        assert_eq!(parse_file_uri("/plain/path"), "/plain/path");
    }
}
