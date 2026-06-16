//! The `.castx` recording format: a strict superset of asciinema `.cast` v2.
//!
//! Line-delimited JSON. The first non-blank line is a header object; each
//! subsequent line is an event array `[time, code, data]`. poshterity adds two
//! extensions that stock asciinema players ignore: a `poshterity` header key and
//! an `m` (named step marker) event letter. Any plain `.cast` v2 therefore
//! replays through poshterity, and any `.castx` plays in `asciinema`.
//!
//! Line-based reading is safe because the writer escapes every interior
//! newline as `\n` (via [`json_string`]): the only raw `0x0a` in a recording
//! is the record separator, so one source line is exactly one logical record.

use crate::json::{self, Value};

/// A parsed recording header. `env` and any other asciinema metadata are
/// parsed-and-ignored in phase 1; only the fields poshterity acts on are kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub version: u16,
    /// Terminal width in columns.
    pub width: u16,
    /// Terminal height in rows.
    pub height: u16,
    /// The poshterity extension block, or `None` for a plain asciinema `.cast`.
    pub poshterity: Option<Poshterity>,
}

/// The poshterity header extension: format version and the emulator revision the
/// recording was produced against (for golden-frame auditing).
#[derive(Debug, Clone, PartialEq)]
pub struct Poshterity {
    pub v: u16,
    pub emu_rev: String,
}

/// An event's type letter. Unknown letters are preserved rather than rejected,
/// so a recording carrying a future or foreign extension still replays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCode {
    /// `o` — terminal output (fed to the emulator on replay).
    Output,
    /// `i` — user input (recorded; not fed on replay).
    Input,
    /// `r` — resize, with `"COLSxROWS"` data.
    Resize,
    /// `m` — poshterity named step marker (becomes meaningful in phase 3).
    Marker,
    /// Any other letter (read-compat; ignored on replay).
    Unknown(char),
}

impl EventCode {
    fn from_letter(s: &str) -> EventCode {
        match s.chars().next() {
            Some('o') => EventCode::Output,
            Some('i') => EventCode::Input,
            Some('r') => EventCode::Resize,
            Some('m') => EventCode::Marker,
            Some(c) => EventCode::Unknown(c),
            None => EventCode::Unknown('\0'),
        }
    }

    fn letter(self) -> char {
        match self {
            EventCode::Output => 'o',
            EventCode::Input => 'i',
            EventCode::Resize => 'r',
            EventCode::Marker => 'm',
            EventCode::Unknown(c) => c,
        }
    }
}

/// One recorded event: `[time, code, data]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Seconds since recording start. Recorded but never replayed as a sleep.
    pub time: f64,
    pub code: EventCode,
    pub data: String,
}

/// A line-oriented reader over a recording's text. Call [`Reader::header`]
/// once, then [`Reader::next_event`] until it returns `None`.
pub struct Reader<'a> {
    lines: std::str::Split<'a, char>,
    line_no: usize,
}

impl<'a> Reader<'a> {
    pub fn new(src: &'a str) -> Reader<'a> {
        Reader {
            lines: src.split('\n'),
            line_no: 0,
        }
    }

    /// Read and parse the header — the first non-blank line. Must be called
    /// before [`Reader::next_event`].
    pub fn header(&mut self) -> Result<Header, String> {
        while let Some(raw) = self.lines.next() {
            self.line_no += 1;
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.trim().is_empty() {
                continue;
            }
            let v = json::parse(line).map_err(|e| self.at(e))?;
            return parse_header(&v).map_err(|e| self.at(e));
        }
        Err("empty recording: no header line".to_string())
    }

    /// Read the next event, skipping blank lines. `None` at end of input.
    pub fn next_event(&mut self) -> Option<Result<Event, String>> {
        while let Some(raw) = self.lines.next() {
            self.line_no += 1;
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.trim().is_empty() {
                continue;
            }
            let parsed = json::parse(line)
                .map_err(|e| self.at(e))
                .and_then(|v| parse_event(&v).map_err(|e| self.at(e)));
            return Some(parsed);
        }
        None
    }

    fn at(&self, msg: String) -> String {
        format!("line {}: {msg}", self.line_no)
    }
}

fn parse_header(v: &Value) -> Result<Header, String> {
    // version: present must be 2; absent assume 2 (the v2 spec requires it, but
    // be lenient so any plausible recording replays).
    let version = match v.get("version") {
        Some(val) => val.as_u16().ok_or("header `version` is not an integer")?,
        None => 2,
    };
    if version != 2 {
        return Err(format!("unsupported asciinema version {version}"));
    }
    // width/height default to a conventional 80x24 if a malformed header omits
    // them; the header is the authoritative size source for replay.
    let width = v.get("width").and_then(Value::as_u16).unwrap_or(80);
    let height = v.get("height").and_then(Value::as_u16).unwrap_or(24);
    let poshterity = v.get("poshterity").and_then(parse_poshterity);
    Ok(Header {
        version,
        width,
        height,
        poshterity,
    })
}

fn parse_poshterity(v: &Value) -> Option<Poshterity> {
    Some(Poshterity {
        v: v.get("v").and_then(Value::as_u16)?,
        emu_rev: v.get("emu_rev").and_then(Value::as_str)?.to_string(),
    })
}

fn parse_event(v: &Value) -> Result<Event, String> {
    let arr = v.as_array().ok_or("event is not a JSON array")?;
    if arr.len() < 2 {
        return Err("event array needs at least [time, code]".to_string());
    }
    let time = arr[0].as_f64().ok_or("event time is not a number")?;
    let code = EventCode::from_letter(arr[1].as_str().ok_or("event code is not a string")?);
    let data = arr.get(2).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Event { time, code, data })
}

/// Serialize a header as one JSON line (no trailing newline).
pub fn write_header(h: &Header) -> String {
    let mut s = format!(
        "{{\"version\":{},\"width\":{},\"height\":{}",
        h.version, h.width, h.height
    );
    if let Some(pr) = &h.poshterity {
        s.push_str(&format!(",\"poshterity\":{{\"v\":{},\"emu_rev\":", pr.v));
        s.push_str(&json_string(&pr.emu_rev));
        s.push('}');
    }
    s.push('}');
    s
}

/// Serialize an event as one JSON line (no trailing newline). The numeric,
/// unquoted time and the `o`/`i`/`r`/`m` letter match asciinema's wire shape.
pub fn write_event(e: &Event) -> String {
    let mut s = String::from("[");
    s.push_str(&format_time(e.time));
    s.push(',');
    s.push_str(&json_string(&e.code.letter().to_string()));
    s.push(',');
    s.push_str(&json_string(&e.data));
    s.push(']');
    s
}

/// Format an event timestamp the way asciinema does: the shortest decimal that
/// round-trips (so `1.0` -> `"1"`, `1.5` -> `"1.5"`). Pinned by test.
pub fn format_time(t: f64) -> String {
    format!("{t}")
}

/// JSON-escape a string. A byte-for-byte clone of posh's `json_string`
/// (crates/posh/src/session/mod.rs); its `json_string_escaping` test is the
/// conformance oracle. Duplicated, not shared, because poshterity must not depend
/// on the posh binary crate (per ADR-0003 — small reassemblers stay separate).
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Streaming `.castx` writer over any [`std::io::Write`] (a file, a buffer).
/// The caller supplies each event's timestamp, so the writer holds no clock and
/// stays deterministically testable.
///
/// Output (`o`) bytes are reassembled across calls so every emitted event is a
/// valid-UTF-8 JSON string: a multi-byte char split across two PTY reads is
/// held until its continuation arrives (the ADR-0003 convention). A
/// genuinely-invalid byte is replaced with U+FFFD so the stream still makes
/// progress. `i`/`r` events flush any held output first to preserve ordering.
pub struct Recorder<W: std::io::Write> {
    writer: W,
    pending: Vec<u8>,
    last_time: f64,
}

impl<W: std::io::Write> Recorder<W> {
    pub fn new(writer: W) -> Recorder<W> {
        Recorder {
            writer,
            pending: Vec::new(),
            last_time: 0.0,
        }
    }

    /// Write the header line.
    pub fn write_header(&mut self, header: &Header) -> std::io::Result<()> {
        self.writer.write_all(write_header(header).as_bytes())?;
        self.writer.write_all(b"\n")
    }

    /// Record output bytes as `o` event(s), reassembling UTF-8 across calls; an
    /// incomplete trailing sequence is held for the next call.
    pub fn output(&mut self, time: f64, bytes: &[u8]) -> std::io::Result<()> {
        self.last_time = time;
        self.pending.extend_from_slice(bytes);
        self.flush_complete(time)
    }

    /// Record a resize as an `r` event with asciinema `"COLSxROWS"` data.
    pub fn resize(&mut self, time: f64, cols: u16, rows: u16) -> std::io::Result<()> {
        self.last_time = time;
        self.flush_held(time)?;
        self.write_line(time, EventCode::Resize, &format!("{cols}x{rows}"))
    }

    /// Record input bytes as an `i` event. Keystrokes are small and usually
    /// ASCII; invalid bytes are replaced lossily (input is never fed on replay).
    pub fn input(&mut self, time: f64, bytes: &[u8]) -> std::io::Result<()> {
        self.last_time = time;
        self.flush_held(time)?;
        let data = String::from_utf8_lossy(bytes);
        self.write_line(time, EventCode::Input, &data)
    }

    /// Flush any held incomplete output (lossily) and the underlying writer.
    pub fn finish(&mut self) -> std::io::Result<()> {
        let time = self.last_time;
        self.flush_held(time)?;
        self.writer.flush()
    }

    /// Emit `o` events for every complete-UTF-8 run in `pending`, holding only
    /// an incomplete trailing sequence.
    fn flush_complete(&mut self, time: f64) -> std::io::Result<()> {
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    if !s.is_empty() {
                        let s = s.to_string();
                        self.write_line(time, EventCode::Output, &s)?;
                        self.pending.clear();
                    }
                    return Ok(());
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    match e.error_len() {
                        // Incomplete tail at the end: emit the valid prefix,
                        // hold the rest for the next write.
                        None => {
                            if valid > 0 {
                                let s = String::from_utf8(self.pending[..valid].to_vec()).unwrap();
                                self.write_line(time, EventCode::Output, &s)?;
                                self.pending.drain(..valid);
                            }
                            return Ok(());
                        }
                        // Genuinely invalid byte(s): emit the valid prefix plus
                        // one U+FFFD, drop the bad bytes, and continue.
                        Some(bad) => {
                            let mut s = String::from_utf8(self.pending[..valid].to_vec()).unwrap();
                            s.push('\u{FFFD}');
                            self.write_line(time, EventCode::Output, &s)?;
                            self.pending.drain(..valid + bad);
                        }
                    }
                }
            }
        }
    }

    /// Emit whatever is held (lossily) as one `o` event, before a non-output
    /// event or at finish, so ordering is preserved.
    fn flush_held(&mut self, time: f64) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let s = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        self.write_line(time, EventCode::Output, &s)
    }

    fn write_line(&mut self, time: f64, code: EventCode, data: &str) -> std::io::Result<()> {
        let event = Event {
            time,
            code,
            data: data.to_string(),
        };
        self.writer.write_all(write_event(&event).as_bytes())?;
        self.writer.write_all(b"\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_minimal_header_without_poshterity() {
        let mut r = Reader::new(r#"{"version":2,"width":80,"height":24}"#);
        let h = r.header().unwrap();
        assert_eq!(h.version, 2);
        assert_eq!(h.width, 80);
        assert_eq!(h.height, 24);
        assert_eq!(h.poshterity, None);
        assert!(r.next_event().is_none());
    }

    #[test]
    fn reads_header_with_poshterity_block() {
        let mut r = Reader::new(
            r#"{"version":2,"width":100,"height":40,"poshterity":{"v":1,"emu_rev":"0.1.0"}}"#,
        );
        let h = r.header().unwrap();
        assert_eq!(
            h.poshterity,
            Some(Poshterity {
                v: 1,
                emu_rev: "0.1.0".to_string()
            })
        );
    }

    #[test]
    fn empty_input_has_no_header() {
        assert!(Reader::new("").header().is_err());
        assert!(Reader::new("\n\n  \n").header().is_err());
    }

    #[test]
    fn reads_events_and_classifies_codes() {
        let src = "{\"version\":2,\"width\":80,\"height\":24}\n\
                   [0.1,\"o\",\"hello\"]\n\
                   [0.2,\"i\",\"x\"]\n\
                   [0.3,\"r\",\"100x40\"]\n\
                   [0.4,\"m\",\"step-1\"]\n\
                   [0.5,\"z\",\"future\"]\n";
        let mut r = Reader::new(src);
        r.header().unwrap();
        let codes: Vec<EventCode> = std::iter::from_fn(|| r.next_event())
            .map(|e| e.unwrap().code)
            .collect();
        assert_eq!(
            codes,
            vec![
                EventCode::Output,
                EventCode::Input,
                EventCode::Resize,
                EventCode::Marker,
                EventCode::Unknown('z'),
            ]
        );
    }

    #[test]
    fn write_event_matches_asciinema_shape() {
        let e = Event {
            time: 1.5,
            code: EventCode::Output,
            data: "hi\nthere".to_string(),
        };
        // Interior newline is escaped, so the record stays one line.
        assert_eq!(write_event(&e), r#"[1.5,"o","hi\nthere"]"#);
    }

    #[test]
    fn format_time_is_shortest_roundtrip() {
        assert_eq!(format_time(1.0), "1");
        assert_eq!(format_time(1.5), "1.5");
        assert_eq!(format_time(0.0), "0");
    }

    #[test]
    fn header_and_events_round_trip() {
        let header = Header {
            version: 2,
            width: 80,
            height: 24,
            poshterity: Some(Poshterity {
                v: 1,
                emu_rev: "0.1.0".to_string(),
            }),
        };
        let events = vec![
            Event {
                time: 0.0,
                code: EventCode::Output,
                data: "a\tb".to_string(),
            },
            Event {
                time: 1.25,
                code: EventCode::Resize,
                data: "100x40".to_string(),
            },
        ];
        let mut doc = write_header(&header);
        for e in &events {
            doc.push('\n');
            doc.push_str(&write_event(e));
        }

        let mut r = Reader::new(&doc);
        assert_eq!(r.header().unwrap(), header);
        let read: Vec<Event> = std::iter::from_fn(|| r.next_event())
            .map(Result::unwrap)
            .collect();
        assert_eq!(read, events);
    }

    /// Record into a buffer and return the resulting document, applying `f` to
    /// a Recorder between header and finish.
    fn record_doc(header: &Header, f: impl FnOnce(&mut Recorder<&mut Vec<u8>>)) -> String {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut rec = Recorder::new(&mut buf);
            rec.write_header(header).unwrap();
            f(&mut rec);
            rec.finish().unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    fn events_of(doc: &str) -> Vec<Event> {
        let mut r = Reader::new(doc);
        r.header().unwrap();
        std::iter::from_fn(|| r.next_event())
            .map(Result::unwrap)
            .collect()
    }

    fn header_24x80() -> Header {
        Header {
            version: 2,
            width: 80,
            height: 24,
            poshterity: None,
        }
    }

    #[test]
    fn recorder_reassembles_utf8_split_across_writes() {
        // 'é' is 0xC3 0xA9; split it across two output() calls.
        let doc = record_doc(&header_24x80(), |rec| {
            rec.output(0.0, &[0xC3]).unwrap(); // incomplete — held, emits nothing
            rec.output(0.1, &[0xA9]).unwrap(); // completes — one 'o' event "é"
        });
        let outputs: Vec<String> = events_of(&doc)
            .into_iter()
            .filter(|e| e.code == EventCode::Output)
            .map(|e| e.data)
            .collect();
        assert_eq!(outputs.concat(), "é");
    }

    #[test]
    fn recorder_preserves_output_resize_ordering() {
        let doc = record_doc(&header_24x80(), |rec| {
            rec.output(0.0, b"before").unwrap();
            rec.resize(0.1, 100, 40).unwrap();
            rec.output(0.2, b"after").unwrap();
        });
        let codes: Vec<(EventCode, String)> =
            events_of(&doc).into_iter().map(|e| (e.code, e.data)).collect();
        assert_eq!(
            codes,
            vec![
                (EventCode::Output, "before".to_string()),
                (EventCode::Resize, "100x40".to_string()),
                (EventCode::Output, "after".to_string()),
            ]
        );
    }

    #[test]
    fn recorder_round_trips_through_reader() {
        let header = Header {
            version: 2,
            width: 80,
            height: 24,
            poshterity: Some(Poshterity {
                v: 1,
                emu_rev: "0.1.0".to_string(),
            }),
        };
        let doc = record_doc(&header, |rec| {
            rec.output(0.0, b"hello \xe2\x86\x92 world").unwrap(); // includes → (U+2192)
            rec.input(0.5, b"q").unwrap();
        });
        let mut r = Reader::new(&doc);
        assert_eq!(r.header().unwrap(), header);
        let events = std::iter::from_fn(|| r.next_event())
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert_eq!(events[0].code, EventCode::Output);
        assert_eq!(events[0].data, "hello → world");
        assert_eq!(events[1].code, EventCode::Input);
        assert_eq!(events[1].data, "q");
    }

    #[test]
    fn recorder_finish_flushes_held_incomplete_tail() {
        // A lone lead byte never completes; finish emits it lossily as U+FFFD.
        let doc = record_doc(&header_24x80(), |rec| {
            rec.output(0.0, &[0xC3]).unwrap();
        });
        let outputs: Vec<String> = events_of(&doc)
            .into_iter()
            .filter(|e| e.code == EventCode::Output)
            .map(|e| e.data)
            .collect();
        assert_eq!(outputs.concat(), "\u{FFFD}");
    }
}
