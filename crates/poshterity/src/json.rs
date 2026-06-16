//! Minimal, dependency-free JSON reader for the `.castx` line format.
//!
//! A `.castx`/asciinema `.cast` v2 recording is line-delimited JSON: one
//! header object, then one array per event ([`crate::castx`]). This module is
//! exactly enough JSON to read those two shapes — a recursive-descent parser
//! producing a [`Value`] with typed projections. The workspace carries no
//! serde (and intends not to), and the writer side mirrors posh's hand-rolled
//! `json_string` escaper, so the string unescape here is its exact inverse.

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// JSON has a single number type; callers project to the int or float they
    /// expect via [`Value::as_u16`] / [`Value::as_f64`].
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    /// Insertion-ordered fields; small N, linear [`Value::get`] lookup.
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// The value as an `f64` (e.g. an event timestamp). `None` for non-numbers.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// An integer-valued number within `u16` range (e.g. a width/height).
    /// Tolerates `80.0`; rejects `80.5`, negatives, and out-of-range.
    pub fn as_u16(&self) -> Option<u16> {
        match self {
            Value::Num(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= u16::MAX as f64 => {
                Some(*n as u16)
            }
            _ => None,
        }
    }

    /// The value as a string slice. `None` for non-strings.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The value as an array slice. `None` for non-arrays.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// Look up an object field by key (first match wins — asciinema never
    /// duplicates keys). `None` for non-objects or a missing key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Parse a complete JSON value from `input`. Leading/trailing JSON whitespace
/// is allowed; any other trailing content is an error (this is what catches a
/// garbled recording line rather than silently reading a prefix).
pub fn parse(input: &str) -> Result<Value, String> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(format!("trailing data at byte {}", p.pos));
    }
    Ok(v)
}

/// Byte-cursor recursive-descent parser. `input` is a `&str`, so all non-ASCII
/// runs are valid UTF-8 and pass through byte-for-byte; only `\u` escapes need
/// decoding.
struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Value::Str(self.parse_string()?)),
            Some(b't' | b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b) if b == b'-' || b.is_ascii_digit() => self.parse_number(),
            Some(b) => Err(format!("unexpected byte {:?} at {}", b as char, self.pos)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.pos += 1; // consume '{'
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Obj(out));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(format!("expected object key at byte {}", self.pos));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.bump() != Some(b':') {
                return Err(format!("expected ':' after key {key:?}"));
            }
            self.skip_ws();
            let val = self.parse_value()?;
            out.push((key, val));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                other => {
                    return Err(format!(
                        "expected ',' or '}}' in object, got {:?}",
                        other.map(|b| b as char)
                    ))
                }
            }
        }
        Ok(Value::Obj(out))
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.pos += 1; // consume '['
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Arr(out));
        }
        loop {
            self.skip_ws();
            out.push(self.parse_value()?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                other => {
                    return Err(format!(
                        "expected ',' or ']' in array, got {:?}",
                        other.map(|b| b as char)
                    ))
                }
            }
        }
        Ok(Value::Arr(out))
    }

    /// Parse a string starting at the opening quote. Unescapes the inverse of
    /// posh's `json_string`, plus `\/` and UTF-16 surrogate pairs that a stock
    /// asciinema writer may emit.
    fn parse_string(&mut self) -> Result<String, String> {
        self.pos += 1; // consume opening '"'
        let mut out: Vec<u8> = Vec::new();
        loop {
            let b = self.bump().ok_or("unterminated string")?;
            match b {
                b'"' => break,
                b'\\' => {
                    let e = self.bump().ok_or("unterminated escape")?;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let ch = self.parse_unicode_escape()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => return Err(format!("invalid escape \\{}", other as char)),
                    }
                }
                _ => out.push(b),
            }
        }
        String::from_utf8(out).map_err(|_| "invalid UTF-8 in string".to_string())
    }

    /// Decode the code unit(s) after a `\u`, joining a high+low surrogate pair
    /// into one scalar. A lone or malformed surrogate yields U+FFFD rather than
    /// erroring, so a hand-written recording still replays.
    fn parse_unicode_escape(&mut self) -> Result<char, String> {
        let hi = self.parse_hex4()?;
        if (0xD800..=0xDBFF).contains(&hi) {
            // High surrogate: expect an immediately-following \uXXXX low half.
            if self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'u') {
                self.pos += 2; // consume "\u"
                let lo = self.parse_hex4()?;
                if (0xDC00..=0xDFFF).contains(&lo) {
                    let cp = 0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32;
                    return Ok(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                }
            }
            return Ok('\u{FFFD}'); // lone/broken high surrogate
        }
        if (0xDC00..=0xDFFF).contains(&hi) {
            return Ok('\u{FFFD}'); // lone low surrogate
        }
        Ok(char::from_u32(hi as u32).unwrap_or('\u{FFFD}'))
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let b = self.bump().ok_or("truncated \\u escape")?;
            let d = match b {
                b'0'..=b'9' => (b - b'0') as u16,
                b'a'..=b'f' => (b - b'a' + 10) as u16,
                b'A'..=b'F' => (b - b'A' + 10) as u16,
                _ => return Err(format!("invalid hex digit {:?}", b as char)),
            };
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        // The slice is ASCII number bytes; let f64 validate the grammar.
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        s.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| format!("invalid number {s:?}"))
    }

    fn parse_bool(&mut self) -> Result<Value, String> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(Value::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(Value::Bool(false))
        } else {
            Err(format!("invalid literal at byte {}", self.pos))
        }
    }

    fn parse_null(&mut self) -> Result<Value, String> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(Value::Null)
        } else {
            Err(format!("invalid literal at byte {}", self.pos))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_u16_accepts_integers_rejects_fractions_and_range() {
        assert_eq!(parse("80").unwrap().as_u16(), Some(80));
        assert_eq!(parse("80.0").unwrap().as_u16(), Some(80));
        assert_eq!(parse("0").unwrap().as_u16(), Some(0));
        assert_eq!(parse("80.5").unwrap().as_u16(), None);
        assert_eq!(parse("-1").unwrap().as_u16(), None);
        assert_eq!(parse("70000").unwrap().as_u16(), None);
    }

    #[test]
    fn as_f64_reads_floats() {
        assert_eq!(parse("1.5").unwrap().as_f64(), Some(1.5));
        assert_eq!(parse("0").unwrap().as_f64(), Some(0.0));
        assert_eq!(parse("1.234567").unwrap().as_f64(), Some(1.234567));
    }

    #[test]
    fn unescapes_inverse_of_json_string() {
        let v = parse(r#""with \"quotes\" \\ back \n nl \t tab""#).unwrap();
        assert_eq!(v.as_str(), Some("with \"quotes\" \\ back \n nl \t tab"));
        // A \uXXXX BMP escape decodes (same path json_string uses for c < 0x20).
        assert_eq!(parse("\"x\\u0041y\"").unwrap().as_str(), Some("xAy"));
        // Raw multibyte UTF-8 passes through untouched.
        assert_eq!(
            parse("\"uni\u{2192}ok\"").unwrap().as_str(),
            Some("uni\u{2192}ok")
        );
    }

    #[test]
    fn unescapes_solidus_and_surrogate_pairs() {
        assert_eq!(parse(r#""a\/b""#).unwrap().as_str(), Some("a/b"));
        // A \uXXXX\uXXXX surrogate pair joins to one scalar: U+1F600 (😀).
        assert_eq!(parse("\"\\uD83D\\uDE00\"").unwrap().as_str(), Some("\u{1F600}"));
        // A raw multibyte emoji also passes straight through.
        assert_eq!(parse("\"\u{1F600}\"").unwrap().as_str(), Some("\u{1F600}"));
        // A lone high surrogate degrades to U+FFFD rather than erroring.
        assert_eq!(parse("\"\\uD83D\"").unwrap().as_str(), Some("\u{FFFD}"));
    }

    #[test]
    fn rejects_trailing_data() {
        assert!(parse(r#"[1,"o","x"]junk"#).is_err());
    }

    #[test]
    fn parses_empty_containers() {
        assert_eq!(parse("{}").unwrap(), Value::Obj(vec![]));
        assert_eq!(parse("[]").unwrap(), Value::Arr(vec![]));
        assert_eq!(parse(r#""""#).unwrap().as_str(), Some(""));
    }

    #[test]
    fn parses_nested_header_shape() {
        let v = parse(
            r#"{"version":2,"width":80,"height":24,"env":{"TERM":"xterm"},"poshterity":{"v":1,"emu_rev":"0.1.0"}}"#,
        )
        .unwrap();
        assert_eq!(v.get("version").and_then(Value::as_u16), Some(2));
        assert_eq!(v.get("width").and_then(Value::as_u16), Some(80));
        assert_eq!(
            v.get("poshterity")
                .and_then(|p| p.get("emu_rev"))
                .and_then(Value::as_str),
            Some("0.1.0")
        );
        assert_eq!(
            v.get("env")
                .and_then(|e| e.get("TERM"))
                .and_then(Value::as_str),
            Some("xterm")
        );
    }

    #[test]
    fn parses_event_array() {
        let v = parse(r#"[1.5,"o","hi"]"#).unwrap();
        let a = v.as_array().unwrap();
        assert_eq!(a[0].as_f64(), Some(1.5));
        assert_eq!(a[1].as_str(), Some("o"));
        assert_eq!(a[2].as_str(), Some("hi"));
    }
}
