//! The step-ratchet [`Player`]: advance a replay by discrete,
//! emulator-defined steps and inspect the screen at each. The heart of
//! poshterity — a deterministic VT100 frame debugger.
//!
//! Each step granularity is derived from a signal `posh_term` already exposes
//! (`mid_escape()`, `generation()`) or a boundary the recording carries (`o`
//! event starts, `m` markers, timestamps). The signal is the contract,
//! computed live, so stepping stays correct across emulator versions — never a
//! baked byte offset.
//!
//! The recording is flattened once into a single output byte buffer plus
//! offset-indexed annotations (write boundaries, resizes, markers, times); the
//! Player then walks a byte cursor over it, applying each resize as the cursor
//! crosses its offset. No clock, no unsafe — fully testable.

use posh_term::Terminal;

use crate::castx::{EventCode, Reader};
use crate::styled::StyledText;

/// A unit of replay advance. See the module docs for the signal behind each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// One byte.
    Byte,
    /// One complete escape sequence, or one printable/control run.
    Escape,
    /// One recorded `o` event's bytes.
    Write,
    /// Up to the next visible change (`generation()` delta).
    Change,
    /// The next coalesced redraw (consecutive events within `frame_gap`).
    Frame,
    /// Up to the next named `m` marker.
    Marker,
}

impl Granularity {
    /// Parse the `--by` CLI value.
    pub fn parse(s: &str) -> Result<Granularity, String> {
        match s {
            "byte" => Ok(Granularity::Byte),
            "escape" => Ok(Granularity::Escape),
            "write" => Ok(Granularity::Write),
            "change" => Ok(Granularity::Change),
            "frame" => Ok(Granularity::Frame),
            "marker" => Ok(Granularity::Marker),
            other => Err(format!(
                "--by expects byte|escape|write|change|frame|marker, got {other:?}"
            )),
        }
    }
}

/// Where the player currently sits in the recording.
#[derive(Debug, Clone, PartialEq)]
pub struct StepPos {
    /// Byte offset into the flattened output stream.
    pub byte_offset: usize,
    /// The emulator's visible-change counter.
    pub generation: u64,
    /// The most recent marker name at or before the cursor.
    pub marker: Option<String>,
    /// The timestamp of the most recent `o` event at or before the cursor.
    pub time: f64,
}

const DEFAULT_FRAME_GAP: f64 = 0.1;

/// A query over the screen text after each write, for [`Player::seek`]: every
/// `contains` string must appear in the screen's text and no `lacks` string
/// may. `from_write` is the first write index considered (`0` and `1` both
/// mean "after the first write": the blank screen before any output never
/// matches).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Seek {
    pub contains: Vec<String>,
    pub lacks: Vec<String>,
    /// Text that must be on screen drawn with given attributes (`--styled`).
    pub styled: Vec<StyledText>,
    pub from_write: usize,
}

impl Seek {
    /// Whether the terminal's screen satisfies the query.
    pub fn holds(&self, term: &Terminal) -> bool {
        let text = term.dump_text();
        self.contains.iter().all(|s| text.contains(s.as_str()))
            && !self.lacks.iter().any(|s| text.contains(s.as_str()))
            && self.styled.iter().all(|s| s.holds(term))
    }
}

/// A deterministic, step-ratcheted replay.
pub struct Player {
    term: Terminal,
    flat: Vec<u8>,
    cursor: usize,
    /// Flat offset where each `o` event begins (ascending).
    writes: Vec<usize>,
    /// `(offset, cols, rows)` for each `r`, applied as the cursor crosses it.
    resizes: Vec<(usize, u16, u16)>,
    /// `(offset, name)` for each `m` marker.
    marks: Vec<(usize, String)>,
    /// `(offset, time)` per `o` event (for frame bucketing / position).
    times: Vec<(usize, f64)>,
    /// Index of the next resize not yet applied.
    next_resize: usize,
    frame_gap: f64,
    /// The emulator revision the recording was produced against (`poshterity`
    /// header), for golden auditing.
    emu_rev: Option<String>,
}

impl Player {
    /// Parse and flatten a `.castx`/`.cast` recording into a stepper.
    pub fn from_source(src: &str) -> Result<Player, String> {
        let mut reader = Reader::new(src);
        let header = reader.header()?;
        let emu_rev = header.poshterity.as_ref().map(|p| p.emu_rev.clone());

        let mut flat = Vec::new();
        let mut writes = Vec::new();
        let mut resizes = Vec::new();
        let mut marks = Vec::new();
        let mut times = Vec::new();
        while let Some(ev) = reader.next_event() {
            let ev = ev?;
            match ev.code {
                EventCode::Output => {
                    writes.push(flat.len());
                    times.push((flat.len(), ev.time));
                    flat.extend_from_slice(ev.data.as_bytes());
                }
                EventCode::Resize => {
                    if let Some((cols, rows)) = parse_resize(&ev.data) {
                        resizes.push((flat.len(), cols, rows));
                    }
                }
                EventCode::Marker => marks.push((flat.len(), ev.data.clone())),
                EventCode::Input | EventCode::Unknown(_) => {}
            }
        }

        Ok(Player {
            term: Terminal::new(header.height, header.width),
            flat,
            cursor: 0,
            writes,
            resizes,
            marks,
            times,
            next_resize: 0,
            frame_gap: DEFAULT_FRAME_GAP,
            emu_rev,
        })
    }

    /// Set the frame-coalescing gap (seconds) for [`Granularity::Frame`].
    pub fn with_frame_gap(mut self, secs: f64) -> Player {
        self.frame_gap = secs;
        self
    }

    /// The `emu_rev` from the recording's `poshterity` header, if present.
    pub fn emu_rev(&self) -> Option<&str> {
        self.emu_rev.as_deref()
    }

    /// Advance to the end of the recording (feed everything remaining).
    pub fn step_to_end(&mut self) {
        self.step(Granularity::Write, usize::MAX);
    }

    /// Read access to the emulated terminal (cells, cursor, dumps, ...).
    pub fn terminal(&self) -> &Terminal {
        &self.term
    }

    /// True once the whole output stream has been consumed.
    pub fn at_end(&self) -> bool {
        self.cursor >= self.flat.len()
    }

    /// The current position.
    pub fn position(&self) -> StepPos {
        StepPos {
            byte_offset: self.cursor,
            generation: self.term.generation(),
            marker: self
                .marks
                .iter()
                .rev()
                .find(|(o, _)| *o <= self.cursor)
                .map(|(_, n)| n.clone()),
            time: self.event_time_at(self.cursor),
        }
    }

    /// Advance by `n` steps of granularity `g`. Returns the number actually
    /// taken (fewer than `n` only at end of stream / no more markers).
    pub fn step(&mut self, g: Granularity, n: usize) -> usize {
        let mut taken = 0;
        for _ in 0..n {
            let advanced = match g {
                Granularity::Byte => self.feed_byte(),
                Granularity::Escape => self.step_escape(),
                Granularity::Write => self.step_write(),
                Granularity::Change => self.step_to_change(),
                Granularity::Frame => self.step_frame(),
                Granularity::Marker => self.step_marker(),
            };
            if !advanced {
                break;
            }
            taken += 1;
        }
        self.flush_trailing_resizes();
        taken
    }

    /// Advance to the next visible change. Returns false at end of stream.
    pub fn step_to_change(&mut self) -> bool {
        if self.at_end() {
            return false;
        }
        let g = self.term.generation();
        while !self.at_end() {
            self.feed_byte();
            if self.term.generation() != g {
                break;
            }
        }
        self.flush_trailing_resizes();
        true
    }

    /// How many `o` writes have been fed so far: the `N` that
    /// `step --by write --n N` reaches, so indices agree between the two.
    pub fn write_index(&self) -> usize {
        if self.at_end() {
            return self.writes.len();
        }
        self.writes.iter().take_while(|&&w| w < self.cursor).count()
    }

    /// Advance write by write until the screen satisfies `q`, one pass over
    /// the recording (not a replay per candidate). Returns the write index
    /// the player now sits at, or `None` if the stream ended first (the
    /// player is then at the end). The write index of a hit depends on how
    /// the recorder happened to chunk output, so a caller asserting "the
    /// banner appeared" should seek by content, never by a fixed index.
    pub fn seek(&mut self, q: &Seek) -> Option<usize> {
        let from = q.from_write.max(1);
        let to_skip = from.saturating_sub(self.write_index());
        if to_skip > 0 {
            self.step(Granularity::Write, to_skip);
        }
        loop {
            if self.write_index() >= from && q.holds(&self.term) {
                return Some(self.write_index());
            }
            if self.step(Granularity::Write, 1) == 0 {
                return None;
            }
        }
    }

    /// Advance to just BEFORE the last alternate-screen exit in the recording
    /// (`CSI ? 1049 l`, `? 1047 l` or `? 47 l`), so the screen is what the
    /// program last drew rather than the primary screen it hands back on exit.
    /// Returns false (the player has not moved) if the recording never leaves
    /// the alternate screen at or ahead of the cursor.
    pub fn step_to_before_alt_exit(&mut self) -> bool {
        const EXITS: [&[u8]; 3] = [b"\x1b[?1049l", b"\x1b[?1047l", b"\x1b[?47l"];
        let last = EXITS
            .iter()
            .filter_map(|pat| self.flat.windows(pat.len()).rposition(|w| w == *pat))
            .max()
            .filter(|&at| at >= self.cursor);
        match last {
            Some(at) => {
                self.feed_until(at);
                true
            }
            None => false,
        }
    }

    /// Advance to the next `m` marker named `name` (at or ahead of the cursor).
    /// Returns false if there is no such marker ahead.
    pub fn step_to_marker(&mut self, name: &str) -> bool {
        let target = self
            .marks
            .iter()
            .find(|(o, n)| *o >= self.cursor && n == name)
            .map(|(o, _)| *o);
        match target {
            Some(o) => {
                self.feed_until(o);
                true
            }
            None => false,
        }
    }

    // --- internals ----------------------------------------------------------

    /// Feed one byte (applying any resize at the cursor first), draining query
    /// replies. Returns false at end of stream.
    fn feed_byte(&mut self) -> bool {
        if self.cursor >= self.flat.len() {
            return false;
        }
        self.apply_resizes_through(self.cursor);
        let b = self.flat[self.cursor];
        self.term.process(&[b]);
        let _ = self.term.take_responses();
        self.cursor += 1;
        true
    }

    fn step_escape(&mut self) -> bool {
        if self.at_end() {
            return false;
        }
        let first = self.flat[self.cursor];
        if first == 0x1b || first == 0x9b {
            // An escape sequence: feed its introducer, then bytes until the
            // parser leaves mid-escape (the sequence completed).
            self.feed_byte();
            while !self.at_end() && self.term.mid_escape() {
                self.feed_byte();
            }
        } else {
            // A printable/control run: bytes up to the next escape introducer.
            while !self.at_end() {
                let b = self.flat[self.cursor];
                if b == 0x1b || b == 0x9b {
                    break;
                }
                self.feed_byte();
            }
        }
        true
    }

    fn step_write(&mut self) -> bool {
        if self.at_end() {
            return false;
        }
        let target = self
            .writes
            .iter()
            .copied()
            .find(|&w| w > self.cursor)
            .unwrap_or(self.flat.len());
        self.feed_until(target);
        true
    }

    fn step_frame(&mut self) -> bool {
        if self.at_end() {
            return false;
        }
        let mut cur_t = self.event_time_at(self.cursor);
        self.step_write();
        // Keep absorbing the next event while it's within frame_gap of the
        // previous one (a coalesced burst of redraws).
        loop {
            match self.times.iter().find(|(o, _)| *o == self.cursor) {
                Some(&(_, next_t)) if next_t - cur_t <= self.frame_gap => {
                    cur_t = next_t;
                    self.step_write();
                }
                _ => break,
            }
        }
        true
    }

    fn step_marker(&mut self) -> bool {
        if self.at_end() {
            return false;
        }
        match self.marks.iter().map(|(o, _)| *o).find(|&o| o > self.cursor) {
            Some(target) => {
                self.feed_until(target);
                true
            }
            None => false,
        }
    }

    fn feed_until(&mut self, target: usize) {
        while self.cursor < target {
            if !self.feed_byte() {
                break;
            }
        }
        self.flush_trailing_resizes();
    }

    fn apply_resizes_through(&mut self, offset: usize) {
        while self.next_resize < self.resizes.len() && self.resizes[self.next_resize].0 <= offset {
            let (_, cols, rows) = self.resizes[self.next_resize];
            self.term.resize(rows, cols);
            self.next_resize += 1;
        }
    }

    /// Apply resizes anchored at or past end-of-stream once everything is
    /// consumed (a resize after the last output byte still sets the size).
    fn flush_trailing_resizes(&mut self) {
        if self.at_end() {
            let end = self.flat.len();
            self.apply_resizes_through(end);
        }
    }

    /// Timestamp of the most recent `o` event starting at or before `offset`.
    fn event_time_at(&self, offset: usize) -> f64 {
        let mut t = 0.0;
        for &(o, et) in &self.times {
            if o <= offset {
                t = et;
            } else {
                break;
            }
        }
        t
    }
}

/// Parse an asciinema resize payload `"COLSxROWS"` into `(cols, rows)`.
fn parse_resize(data: &str) -> Option<(u16, u16)> {
    let (w, h) = data.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // header 20x5, then: printable "hi", a CSI-SGR + 'X' + CSI-SGR reset, a
    // marker, "Y", a resize, "Z".  is ESC (CSI introducer).
    const FIXTURE: &str = "{\"version\":2,\"width\":20,\"height\":5}\n\
                           [0.00,\"o\",\"hi\"]\n\
                           [0.01,\"o\",\"\\u001b[31mX\\u001b[0m\"]\n\
                           [0.02,\"m\",\"mark1\"]\n\
                           [0.03,\"o\",\"Y\"]\n\
                           [0.50,\"r\",\"10x4\"]\n\
                           [0.51,\"o\",\"Z\"]\n";

    fn row0(p: &Player) -> String {
        p.terminal().screen().row(0).unwrap().text(true)
    }

    #[test]
    fn byte_steps_one_at_a_time() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert_eq!(p.step(Granularity::Byte, 1), 1);
        assert_eq!(p.position().byte_offset, 1);
        assert_eq!(p.step(Granularity::Byte, 1), 1);
        assert_eq!(row0(&p), "hi");
    }

    #[test]
    fn escape_groups_sequences_and_printable_runs() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        // Step 1: the printable run "hi".
        p.step(Granularity::Escape, 1);
        assert_eq!(p.position().byte_offset, 2);
        assert_eq!(row0(&p), "hi");
        // Step 2: the full CSI-SGR "ESC[31m" (no new visible cell yet).
        p.step(Granularity::Escape, 1);
        assert_eq!(p.position().byte_offset, 7);
        assert_eq!(row0(&p), "hi");
        // Step 3: the printable run "X".
        p.step(Granularity::Escape, 1);
        assert_eq!(p.position().byte_offset, 8);
        assert_eq!(row0(&p), "hiX");
    }

    #[test]
    fn write_lands_on_event_boundaries() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        p.step(Granularity::Write, 1); // "hi"
        assert_eq!(p.position().byte_offset, 2);
        p.step(Granularity::Write, 1); // the escape event
        assert_eq!(p.position().byte_offset, 12);
        assert_eq!(row0(&p), "hiX");
    }

    #[test]
    fn change_advances_to_next_visible_change() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        // The very first change is printing 'h'.
        assert!(p.step_to_change());
        assert!(row0(&p).starts_with('h'));
        let g = p.position().generation;
        assert!(g >= 1);
    }

    #[test]
    fn marker_steps_land_exactly_on_the_marker() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        // mark1 sits after "hi" + the 10-byte escape event = offset 12.
        assert!(p.step_to_marker("mark1"));
        assert_eq!(p.position().byte_offset, 12);
        assert_eq!(p.position().marker.as_deref(), Some("mark1"));
        // "Y" not yet fed.
        assert_eq!(row0(&p), "hiX");
        assert!(!p.step_to_marker("nope"));
    }

    #[test]
    fn resize_applies_as_the_cursor_crosses_it() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert_eq!((p.terminal().cols(), p.terminal().rows()), (20, 5));
        // Run to the end: the "10x4" resize precedes the final "Z".
        while !p.at_end() {
            p.step(Granularity::Write, 1);
        }
        assert_eq!((p.terminal().cols(), p.terminal().rows()), (10, 4));
        assert!(row0(&p).contains('Z'));
    }

    // header 20x3: enter the alt screen, draw, leave it, then a line on the
    // primary screen. A finished full-screen program ends this way.
    const ALT_FIXTURE: &str = "{\"version\":2,\"width\":20,\"height\":3}\n\
                               [0.0,\"o\",\"\\u001b[?1049hhello\"]\n\
                               [0.1,\"o\",\"\\u001b[?1049lback\"]\n";

    #[test]
    fn step_to_before_alt_exit_shows_the_last_screen_the_program_drew() {
        let mut p = Player::from_source(ALT_FIXTURE).unwrap();
        assert!(p.step_to_before_alt_exit());
        assert_eq!(row0(&p), "hello", "the alt screen, just before it is left");
        p.step_to_end();
        assert_eq!(row0(&p), "back", "the primary screen after the exit");
    }

    #[test]
    fn step_to_before_alt_exit_uses_the_last_exit_and_reports_none() {
        const TWICE: &str = "{\"version\":2,\"width\":20,\"height\":3}\n\
                             [0.0,\"o\",\"\\u001b[?1049hone\\u001b[?1049l\"]\n\
                             [0.1,\"o\",\"\\u001b[?1049htwo\\u001b[?1049l\"]\n";
        let mut p = Player::from_source(TWICE).unwrap();
        assert!(p.step_to_before_alt_exit());
        assert_eq!(row0(&p), "two", "the LAST exit, not the first");
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert!(!p.step_to_before_alt_exit(), "no alternate screen in the recording");
    }

    // --- seek: find the first write whose screen matches ------------------

    fn seek(contains: &[&str], lacks: &[&str], from: usize) -> Seek {
        Seek {
            contains: contains.iter().map(|s| s.to_string()).collect(),
            lacks: lacks.iter().map(|s| s.to_string()).collect(),
            from_write: from,
            ..Seek::default()
        }
    }

    #[test]
    fn seek_styled_finds_the_write_where_the_text_gets_its_style() {
        // "hi" plain, then the same cells restyled inverse by a later write.
        const SRC: &str = "{\"version\":2,\"width\":20,\"height\":3}\n\
                           [0.0,\"o\",\"hi\"]\n\
                           [0.1,\"o\",\"\\u001b[1G\\u001b[7mhi\\u001b[0m\"]\n";
        let mut p = Player::from_source(SRC).unwrap();
        let q = Seek {
            styled: vec![StyledText::parse("hi=inverse").unwrap()],
            ..Seek::default()
        };
        assert_eq!(p.seek(&q), Some(2), "plain after write 1, inverse after write 2");
    }

    #[test]
    fn seek_finds_the_first_write_whose_screen_matches() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        // write 1 = "hi", write 2 adds "X": the first screen with "hiX".
        assert_eq!(p.seek(&seek(&["hiX"], &[], 0)), Some(2));
        assert_eq!(p.write_index(), 2, "the player stays at the match");
        assert_eq!(row0(&p), "hiX");
    }

    #[test]
    fn seek_all_contains_and_no_lacks_must_hold_together() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        // "hi" is on screen from write 1; "Y" arrives with write 3.
        assert_eq!(p.seek(&seek(&["hi"], &["Y"], 0)), Some(1));
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert_eq!(p.seek(&seek(&["hi", "Y"], &[], 0)), Some(3));
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert_eq!(p.seek(&seek(&["hi"], &["X"], 2)), None, "X is there from write 2 on");
    }

    #[test]
    fn seek_from_write_skips_earlier_matches() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert_eq!(p.seek(&seek(&["hi"], &[], 3)), Some(3));
        // Zero means "from the first write": the blank screen before any
        // output is not a match for a `lacks`-only query.
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert_eq!(p.seek(&seek(&[], &["nope"], 0)), Some(1));
    }

    #[test]
    fn seek_returns_none_at_end_of_stream() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert_eq!(p.seek(&seek(&["never"], &[], 0)), None);
        assert!(p.at_end());
    }

    #[test]
    fn write_index_counts_writes_consumed() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        assert_eq!(p.write_index(), 0);
        p.step(Granularity::Write, 3);
        assert_eq!(p.write_index(), 3);
        p.step_to_end();
        assert_eq!(p.write_index(), 4);
    }

    #[test]
    fn step_reports_steps_taken_and_stops_at_end() {
        let mut p = Player::from_source(FIXTURE).unwrap();
        let taken = p.step(Granularity::Write, 99);
        assert_eq!(taken, 4); // exactly four `o` events
        assert!(p.at_end());
    }
}
