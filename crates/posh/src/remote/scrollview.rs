//! Shared scrollback scroll-view machinery (FDR 0005), used by BOTH the remote
//! roaming client (`remote/client.rs`) and the local session frame client
//! (`session/client.rs`). Extracted so the two clients drive one implementation
//! of the wheel-intercept filter, the scroll-offset math, and the frozen
//! history compose — parameterized over the shared inputs (a [`ScrollbackRing`],
//! the client's `server_term`/`rows`/`cols`, its render memo and `last_drawn`)
//! rather than over either client's full state, so neither client's private
//! concerns (prediction, palette, stats on the remote side; the reliable-socket
//! render tail on the local side) leak in.

use std::borrow::Cow;

use posh_term::{MouseMode, Terminal};

use crate::remote::display::{self, Snapshot};
use crate::remote::sync::{ScrollbackRing, ViewRow};

/// Whether the client intercepts the outer terminal's wheel right now: the
/// inner app has set no mouse mode of its own AND it is on the primary screen
/// (the only screen with scrollback). True at a bare prompt — where the wheel
/// drives the scrollback scroll-view (FDR 0005) by default, or the legacy
/// wheel→arrow grab transform when `POSH_GRAB_MOUSE=on` (posh#50). This is the
/// "enable wheel reporting" predicate (render side); the input side then picks
/// arrows-vs-scroll handling via `grab_mouse` (`POSH_GRAB_MOUSE`).
pub(crate) fn wheel_active(server_term: &Terminal) -> bool {
    server_term.mouse_mode() == MouseMode::None && !server_term.is_alt_screen()
}

/// Lines moved per wheel tick (matches a typical terminal's wheel step).
pub(crate) const WHEEL_STEP: usize = 3;

/// The scroll-view render memo: `(scroll_offset, view_revision, arriving,
/// server_generation)`. `view_revision` is the ring's monotonic
/// [`ScrollbackRing::view_revision`] — it moves on every append (so an
/// append that evicts at capacity still repaints), every hole, and every
/// mark merged into the tail hole (which relabels it without adding a view
/// row). A repaint is skipped while the key is unchanged. `None` forces the
/// next compose.
pub(crate) type ScrollMemo = Option<(usize, u64, u64, u64)>;

/// Rows of the scroll view above the live screen: the ring's rows and
/// not-received holes, plus one arriving row while rows are still
/// arriving (posh#225 Stage 4). The offset clamps to this.
pub(crate) fn history_view_len(scrollback: &ScrollbackRing, arriving: u64) -> usize {
    scrollback.view_len() + usize::from(arriving > 0)
}

/// The words of a hole row (UX decision 8) — the one place they live.
pub(crate) fn hole_label(lines: u64, arriving: bool) -> String {
    let plural = if lines == 1 { "" } else { "s" };
    let what = if arriving { "arriving" } else { "not received" };
    format!("{} line{plural} {what}", display::group_thousands(lines))
}

/// A hole row as ring-row bytes: dim, centred in `cols`, truncated so it
/// never wraps, CRLF-terminated. The one place a hole's look lives — the
/// visual treatment is an open item (flood-delivery UX design). Like a
/// `dump_scrollback_row` row it starts from the default pen and resets the
/// pen before its CRLF, so its dim cannot bleed into the next row.
pub(crate) fn hole_row(label: &str, cols: u16) -> Vec<u8> {
    let text = format!("··· {label} ···");
    // Every character here is one column wide.
    let width = text.chars().count().min(usize::from(cols));
    let pad = (usize::from(cols) - width) / 2;
    // A soft-wrapped ring row carries no pen reset of its own
    // (`dump_scrollback_row` skips it), so start from the default pen
    // explicitly rather than trusting the row before.
    let mut row = String::from("\x1b[0m");
    row.push_str(&" ".repeat(pad));
    row.push_str("\x1b[2m");
    row.extend(text.chars().take(width));
    row.push_str("\x1b[0m\r\n");
    row.into_bytes()
}

/// Sets the scroll-view offset, clamped to the available history
/// (`history_len`, the history view length — [`history_view_len`]: ring rows,
/// holes and the arriving row). On a real change it invalidates the scroll memo (so the
/// next compose repaints) and returns `true` — the caller invalidates any
/// *additional* memo of its own (the remote client's live-render memo). At
/// offset 0 the caller resumes the live view.
#[must_use = "the `changed` bool gates the caller's own memo invalidation; \
    ignore it explicitly with `let _` only when there is no additional memo"]
pub(crate) fn set_scroll(
    scroll_offset: &mut usize,
    scroll_memo: &mut ScrollMemo,
    history_len: usize,
    offset: usize,
) -> bool {
    let offset = offset.min(history_len);
    if offset != *scroll_offset {
        *scroll_offset = offset;
        *scroll_memo = None;
        true
    } else {
        false
    }
}

/// Applies wheel ticks to the scroll offset: positive = up (scroll back into
/// history), negative = down (toward live); reaching 0 returns to the live view
/// (FDR 0005). Returns `true` when the offset actually moved (see [`set_scroll`]).
#[must_use = "the `changed` bool gates the caller's own memo invalidation; \
    ignore it explicitly with `let _` only when there is no additional memo"]
pub(crate) fn scroll_by(
    scroll_offset: &mut usize,
    scroll_memo: &mut ScrollMemo,
    history_len: usize,
    ticks: i32,
) -> bool {
    let delta = ticks * WHEEL_STEP as i32;
    let new = (*scroll_offset as i64 + i64::from(delta)).max(0) as usize;
    set_scroll(scroll_offset, scroll_memo, history_len, new)
}

/// Builds the scroll-view escape stream (FDR 0005): a window of the accumulated
/// scrollback ring plus the current visible grid, rendered frozen at
/// `scroll_offset`, with a top status-bar indicator. Returns empty when the
/// view already matches (the [`ScrollMemo`] key — offset, view rows added,
/// arriving count and server generation — is unchanged). The live view is bypassed while scrolled; a window resize or a
/// keystroke returns `scroll_offset` to 0 and resumes the live path.
///
/// Parameterized over the two clients' fields: the frozen window is replayed
/// through a scratch terminal and diffed against `last_drawn` exactly like any
/// other frame, so the caller's normal render bookkeeping (`initialized`,
/// `last_drawn`) advances identically whether the live or the scroll path drew.
///
/// The history is the ring's view rows — ring rows with each not-received hole
/// collapsed to one dim row — then, while `arriving > 0`, one dim row naming
/// the rows still arriving, directly above the live screen (posh#225 Stage 4);
/// the bar names them too. With no holes and nothing arriving the bytes are
/// exactly the plain ring's (the local client always passes `arriving = 0`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compose_scroll_frame(
    scroll_offset: usize,
    scrollback: &ScrollbackRing,
    arriving: u64,
    server_term: &Terminal,
    rows: u16,
    cols: u16,
    scroll_memo: &mut ScrollMemo,
    initialized: &mut bool,
    last_drawn: &mut Snapshot,
    scroll_opt: bool,
) -> Vec<u8> {
    let memo = (scroll_offset, scrollback.view_revision(), arriving, server_term.generation());
    if *initialized && *scroll_memo == Some(memo) {
        return Vec::new();
    }
    *scroll_memo = Some(memo);

    let rows_usize = rows as usize;
    let view_len = scrollback.view_len();
    let sb_len = history_view_len(scrollback, arriving);
    // Visible grid rows serialized in the same per-row byte format as the ring,
    // so the whole logical history is one uniform sequence (FDR 0005).
    let visible = server_term.dump_visible_rows();
    let total = sb_len + visible.len();
    let offset = scroll_offset.min(sb_len);
    // Viewport: the `rows` logical rows ending `offset` above the live bottom.
    let top = total.saturating_sub(rows_usize).saturating_sub(offset);
    let end = (top + rows_usize).min(total);

    // Replay the window through a scratch terminal (posh_term regenerates the
    // wrap seams by autowrapping), then diff it like any other frame. The
    // scratch height is the tty row count, never the (larger) history depth.
    let mut term = Terminal::with_scrollback(rows, cols, 0);
    let count = end - top;
    for (j, i) in (top..end).enumerate() {
        let row: Cow<[u8]> = if i < view_len {
            match scrollback.view_row(i) {
                Some(ViewRow::Row(bytes)) => Cow::Borrowed(bytes),
                Some(ViewRow::NotReceived(n)) => Cow::Owned(hole_row(&hole_label(n, false), cols)),
                None => Cow::Borrowed(&[]),
            }
        } else if i < sb_len {
            Cow::Owned(hole_row(&hole_label(arriving, true), cols))
        } else {
            Cow::Borrowed(&visible[i - sb_len])
        };
        let row: &[u8] = &row;
        // The final row drops its trailing CRLF so it doesn't scroll the grid.
        if j + 1 == count {
            term.process(row.strip_suffix(b"\r\n").unwrap_or(row));
        } else {
            term.process(row);
        }
    }

    let mut snap = Snapshot::from_term(&term);
    snap.cursor_visible = false; // no live cursor in history
    display::apply_scroll_indicator(&mut snap, offset, arriving);
    // `last_wheel` is passed equal to the current wheel intent (self-referential
    // = "no transition"): reporting must stay on across a history scroll even if
    // the live model goes alt-screen underneath, so the grab teardown is left to
    // the scroll->live return through the live compose path (github #106).
    let wheel = wheel_active(server_term);
    let bytes = display::new_frame_opt(*initialized, last_drawn, &snap, wheel, wheel, scroll_opt);
    *initialized = true;
    *last_drawn = snap;
    bytes
}

/// Cap on a buffered candidate SGR mouse sequence. A real one is at most
/// `ESC [ < 223 ; 65535 ; 65535 M` (22 bytes); a longer run with no
/// terminator is not a mouse sequence, so the filter gives up and flushes it
/// raw — bounding the buffer and never swallowing real input forever. posh#52.
pub(crate) const MAX_MOUSE_SEQ: usize = 32;

/// A byte-fed state machine that intercepts SGR mouse sequences
/// (`ESC [ < Cb ; Cx ; Cy (M|m)`) in the input stream and either reports the
/// wheel ones as scroll ticks (scroll mode) or translates them to arrow keys,
/// dropping the rest — the wheel-grab transform (posh#50). Modeled on mosh's
/// `UserInput` (and posh-term's own parser): the state persists across calls,
/// so a sequence split across `read()`s reassembles at *any* byte boundary with
/// no held-buffer special-casing (posh#52). Only bytes that are part of a live
/// `ESC[<…` match are withheld; the instant a match fails (or overflows
/// `MAX_MOUSE_SEQ`), every buffered byte is flushed verbatim — so all non-mouse
/// input (Esc, arrows, ctrl-keys, UTF-8) round-trips losslessly.
///
/// Accepted tradeoff: a lone trailing `ESC` (and a partial `ESC[`) is held
/// until the next byte resolves whether it begins a mouse sequence — the
/// classic Esc-vs-escape-sequence ambiguity every VT input layer faces (cf.
/// vim `ttimeoutlen`, readline `keyseq-timeout`). So a *solo* Esc keypress is
/// withheld until the next key. This only bites under `POSH_GRAB_MOUSE=on`
/// AND when the inner app has set no mouse mode (a bare prompt, where a lone
/// Esc rarely matters); mosh's `UserInput` holds ESC the same way. A
/// millisecond timeout flush (the other standard resolution) is deliberately
/// not added — it would put a deadline in the poll loop for a default-off
/// feature's edge. Rationale recorded in docs/decisions/0002.
#[derive(Default)]
pub(crate) struct MouseFilter {
    state: MouseState,
    /// Bytes consumed for the in-progress candidate, replayed verbatim if the
    /// candidate turns out not to be a (complete) mouse sequence.
    pending: Vec<u8>,
}

#[derive(Default, PartialEq)]
enum MouseState {
    #[default]
    Ground,
    Esc,     // saw ESC
    Bracket, // saw ESC [
    Body,    // saw ESC [ < ; collecting Cb;Cx;Cy until M/m
}

/// What a [`MouseFilter::feed`] batch yields: the non-mouse bytes to forward,
/// and the net wheel ticks recognized (+ = up/scroll-back, - = down). In scroll
/// mode wheel events populate `wheel` and produce no bytes; in arrows mode
/// (legacy `POSH_GRAB_MOUSE`) they are translated into arrow keys in `bytes`.
#[derive(Default)]
pub(crate) struct FilterOut {
    pub(crate) bytes: Vec<u8>,
    pub(crate) wheel: i32,
}

impl MouseFilter {
    /// Feed one input batch; returns the bytes to forward plus any net wheel
    /// ticks. `scroll` selects the wheel handling: true → report ticks for the
    /// scrollback view; false → translate to arrow keys (legacy grab). Any
    /// incomplete trailing sequence stays in `self` for the next call.
    pub(crate) fn feed(&mut self, buf: &[u8], app_cursor_keys: bool, scroll: bool) -> FilterOut {
        let mut out = FilterOut {
            bytes: Vec::with_capacity(buf.len() + self.pending.len()),
            wheel: 0,
        };
        for &b in buf {
            self.step(b, app_cursor_keys, scroll, &mut out);
        }
        out
    }

    fn step(&mut self, b: u8, app_cursor_keys: bool, scroll: bool, out: &mut FilterOut) {
        match self.state {
            MouseState::Ground => {
                if b == 0x1b {
                    self.pending.push(b);
                    self.state = MouseState::Esc;
                } else {
                    out.bytes.push(b);
                }
            }
            MouseState::Esc => {
                if b == b'[' {
                    self.pending.push(b);
                    self.state = MouseState::Bracket;
                } else {
                    // Not ESC [ — a real Esc or some other ESC sequence.
                    // Flush ESC and reprocess this byte from Ground.
                    self.flush(out);
                    self.step(b, app_cursor_keys, scroll, out);
                }
            }
            MouseState::Bracket => {
                if b == b'<' {
                    self.pending.push(b);
                    self.state = MouseState::Body;
                } else {
                    // ESC [ <other> — a real CSI (arrow, etc.), not mouse.
                    self.flush(out);
                    self.step(b, app_cursor_keys, scroll, out);
                }
            }
            MouseState::Body => {
                if b == b'M' || b == b'm' {
                    // Complete: translate the button code, drop non-wheel.
                    let body = &self.pending[3..]; // after ESC [ <
                    let cb = body.split(|&c| c == b';').next().and_then(|s| {
                        std::str::from_utf8(s).ok().and_then(|s| s.parse::<u32>().ok())
                    });
                    match cb {
                        // Wheel up/down: report a scroll tick (scroll mode) or
                        // translate to an arrow key (legacy grab mode).
                        Some(64) if scroll => out.wheel += 1,
                        Some(65) if scroll => out.wheel -= 1,
                        Some(64) => out.bytes.extend_from_slice(arrow_up(app_cursor_keys)),
                        Some(65) => out.bytes.extend_from_slice(arrow_down(app_cursor_keys)),
                        // click / motion / other button → dropped; a malformed
                        // ESC[<M with no button code (cb == None) drops too,
                        // which is correct: the grabbed app requested no mouse
                        // reporting, so no mouse event should reach it.
                        _ => {}
                    }
                    self.pending.clear();
                    self.state = MouseState::Ground;
                } else if b.is_ascii_digit() || b == b';' {
                    self.pending.push(b);
                    if self.pending.len() > MAX_MOUSE_SEQ {
                        // Not a real mouse sequence; give up and flush raw.
                        self.flush(out);
                    }
                } else {
                    // Unexpected byte in the body: not a valid mouse sequence.
                    self.flush(out);
                    self.step(b, app_cursor_keys, scroll, out);
                }
            }
        }
    }

    /// Emit the buffered candidate verbatim and reset to Ground (the bytes
    /// weren't a mouse sequence after all).
    fn flush(&mut self, out: &mut FilterOut) {
        out.bytes.extend_from_slice(&self.pending);
        self.pending.clear();
        self.state = MouseState::Ground;
    }

    /// Reset to Ground and return any held partial verbatim. Called when the
    /// grab disengages mid-sequence (the app took over the mouse): the held
    /// bytes are real user input and must not be dropped — handing them back
    /// lets the caller forward the now-complete sequence to the app that just
    /// asked for mouse reporting, rather than losing the prefix and leaking a
    /// corrupt tail. posh#52.
    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        self.state = MouseState::Ground;
        std::mem::take(&mut self.pending)
    }
}

fn arrow_up(app_cursor_keys: bool) -> &'static [u8] {
    if app_cursor_keys {
        b"\x1bOA"
    } else {
        b"\x1b[A"
    }
}

fn arrow_down(app_cursor_keys: bool) -> &'static [u8] {
    if app_cursor_keys {
        b"\x1bOB"
    } else {
        b"\x1b[B"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a whole batch through a fresh filter in legacy arrows mode (no split
    /// across reads), returning the forwarded bytes.
    fn filter_once(buf: &[u8], app_cursor_keys: bool) -> Vec<u8> {
        MouseFilter::default().feed(buf, app_cursor_keys, false).bytes
    }

    #[test]
    fn grabbed_wheel_becomes_arrows_and_other_events_drop() {
        // Wheel-up (Cb 64) and wheel-down (Cb 65) → CSI cursor keys; a click
        // (Cb 0) and motion are dropped; surrounding literal bytes survive.
        assert_eq!(filter_once(b"\x1b[<64;10;5M", false), b"\x1b[A");
        assert_eq!(filter_once(b"\x1b[<65;10;5M", false), b"\x1b[B");
        assert_eq!(filter_once(b"\x1b[<0;3;4M", false), b"");
        assert_eq!(filter_once(b"\x1b[<0;3;4m", false), b"");
        // Application cursor keys → SS3 form.
        assert_eq!(filter_once(b"\x1b[<64;1;1M", true), b"\x1bOA");
        assert_eq!(filter_once(b"\x1b[<65;1;1M", true), b"\x1bOB");
        // Literal bytes around a wheel event pass through; two ticks coalesce.
        assert_eq!(filter_once(b"a\x1b[<64;1;1Mb\x1b[<65;1;1M", false), b"a\x1b[Ab\x1b[B");
        // A plain keystroke is untouched.
        assert_eq!(filter_once(b"x", false), b"x");
    }

    #[test]
    fn non_mouse_escape_sequences_round_trip_losslessly() {
        // The filter must never CORRUPT real input. A real arrow key (ESC [ A),
        // a ctrl-arrow, an ESC O cursor key, and a control byte all emerge
        // verbatim once complete — the candidate dies at the non-`<` byte and
        // everything buffered is flushed unchanged.
        assert_eq!(filter_once(b"\x1b[A", false), b"\x1b[A"); // real up-arrow
        assert_eq!(filter_once(b"\x1b[1;5C", false), b"\x1b[1;5C"); // ctrl-right
        assert_eq!(filter_once(b"\x1bOA", false), b"\x1bOA"); // SS3 up
        assert_eq!(filter_once(b"\x03", false), b"\x03"); // Ctrl-C

        // A lone trailing ESC is HELD (it could begin a mouse seq next read) —
        // the byte machine's nature, matching mosh's UserInput. It is not lost:
        // the next byte completes the decision and flushes it.
        let mut f = MouseFilter::default();
        assert_eq!(f.feed(b"\x1b", false, false).bytes, b"", "lone ESC held pending next byte");
        assert_eq!(f.feed(b"a", false, false).bytes, b"\x1ba", "next byte flushes the held ESC");
    }

    #[test]
    fn grabbed_split_sequence_reassembles_at_any_boundary() {
        // posh#52: the persistent state machine reassembles a wheel sequence
        // split across reads at EVERY byte boundary, with no raw leak — the
        // case the old buffer-scan could only partly handle.
        for split in 1..b"\x1b[<64;10;5M".len() {
            let seq = b"\x1b[<64;10;5M";
            let mut f = MouseFilter::default();
            let mut out = f.feed(&seq[..split], false, false).bytes;
            out.extend(f.feed(&seq[split..], false, false).bytes);
            assert_eq!(out, b"\x1b[A", "split at {split} must reassemble to one arrow");
        }
    }

    #[test]
    fn grab_flip_mid_sequence_hands_back_the_held_partial() {
        // posh#52 / review candidate 1: if grab disengages (app took the
        // mouse) while a wheel sequence is half-read, the held prefix must be
        // handed back, not dropped — so the app receives the complete event.
        let mut f = MouseFilter::default();
        assert_eq!(f.feed(b"\x1b[<64", false, false).bytes, b"", "front half held while grabbed");
        // Grab flips off; the caller drains the partial and prepends the tail.
        let pending = f.take_pending();
        assert_eq!(pending, b"\x1b[<64", "held prefix returned, not lost");
        let mut delivered = pending;
        delivered.extend_from_slice(b";1;1M");
        assert_eq!(delivered, b"\x1b[<64;1;1M", "app gets the whole sequence");
        // And the filter is back at Ground for whatever comes next.
        assert_eq!(f.feed(b"x", false, false).bytes, b"x");
    }

    #[test]
    fn grabbed_partial_is_bounded_and_flushed_not_held_forever() {
        // An ESC[< that never terminates must not grow the buffer without
        // bound: past MAX_MOUSE_SEQ it isn't a real mouse sequence, so it's
        // flushed raw rather than swallowing input indefinitely.
        let mut junk = b"\x1b[<".to_vec();
        junk.extend(std::iter::repeat_n(b'9', MAX_MOUSE_SEQ));
        let out = filter_once(&junk, false);
        assert_eq!(out, junk, "over-long candidate is flushed literally");
    }

    #[test]
    fn scroll_mode_reports_wheel_ticks_not_arrows() {
        // scroll=true: wheel up/down become ticks (+/-), not arrow keys.
        let up = MouseFilter::default().feed(b"\x1b[<64;1;1M", false, true);
        assert_eq!(up.wheel, 1);
        assert!(up.bytes.is_empty(), "scroll mode emits no arrow bytes");
        let down = MouseFilter::default().feed(b"\x1b[<65;1;1M", false, true);
        assert_eq!(down.wheel, -1);
        // A click is dropped (wheel 0); surrounding keystrokes pass through.
        let mixed = MouseFilter::default().feed(b"a\x1b[<0;3;4Mb", false, true);
        assert_eq!(mixed.wheel, 0);
        assert_eq!(mixed.bytes, b"ab");
    }

    #[test]
    fn wheel_active_requires_primary_screen_without_app_mouse_mode() {
        let mut term = Terminal::with_scrollback(5, 20, 0);
        // Bare prompt, primary screen, no app mouse mode → wheel intercepted.
        assert!(wheel_active(&term));
        // App enables mouse tracking → posh steps back, passes events through.
        term.process(b"\x1b[?1000h");
        assert!(!wheel_active(&term));
        // No app mouse mode again, but on the alt screen → no scrollback there.
        term.process(b"\x1b[?1000l\x1b[?1049h");
        assert!(!wheel_active(&term));
    }

    #[test]
    fn scroll_offset_clamps_to_ring_and_returns_to_live_at_bottom() {
        let mut offset = 0usize;
        let mut memo: ScrollMemo = None;
        let ring_len = 2; // ring depth 2
        let _ = scroll_by(&mut offset, &mut memo, ring_len, 1); // +WHEEL_STEP, clamped
        assert_eq!(offset, 2);
        let _ = scroll_by(&mut offset, &mut memo, ring_len, -1); // back past the bottom → live
        assert_eq!(offset, 0);
    }

    /// The local client's witness (posh#225 Task 4.3): with no holes and
    /// nothing arriving, the scroll view's bytes are exactly today's.
    #[test]
    fn compose_without_holes_or_arriving_is_unchanged() {
        let mut server_term = Terminal::with_scrollback(5, 20, 0);
        server_term.process(b"live0\r\nlive1\r\n$ ");
        let mut ring = ScrollbackRing::new(100);
        for s in ["line0", "line1", "line2", "line3"] {
            ring.append(&[format!("{s}\r\n").into_bytes()]);
        }
        let mut memo: ScrollMemo = None;
        let mut initialized = false;
        let mut last_drawn = Snapshot::blank(5, 20);
        let bytes = compose_scroll_frame(
            4,
            &ring,
            0,
            &server_term,
            5,
            20,
            &mut memo,
            &mut initialized,
            &mut last_drawn,
            true,
        );
        let want: &[u8] = b"\x1b[?5l\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[?25l\x1b[0;1;7m-- SCROLLBACK \xc2\xb7 4 li\r\n\x1b[0mline1\x1b[K\r\nline2\x1b[K\r\nline3\x1b[K\r\nlive0\x1b[K\x1b[5;6H\x1b[?25l\x1b[0m\x1b]8;;\x1b\\\x1b[?2004l\x1b[?1l\x1b>\x1b[?1003l\x1b[?1002l\x1b[?9l\x1b[?1000h\x1b[?1006h\x1b[?1004l\x1b[?1007l\x1b[=0;1u";
        assert_eq!(bytes, want);
    }

    #[test]
    fn set_scroll_reports_change_and_invalidates_memo() {
        let mut offset = 0usize;
        let mut memo: ScrollMemo = Some((0, 0, 0, 0));
        assert!(set_scroll(&mut offset, &mut memo, 10, 3), "a move reports changed");
        assert_eq!(offset, 3);
        assert_eq!(memo, None, "a move invalidates the scroll memo");
        memo = Some((3, 0, 0, 0));
        assert!(!set_scroll(&mut offset, &mut memo, 10, 3), "no move reports unchanged");
        assert_eq!(memo, Some((3, 0, 0, 0)), "an unchanged offset leaves the memo intact");
    }

    // ---- posh#225 Task 4.3: hole rows and the arriving row ----

    /// Replays `bytes` onto a fresh `rows`x`cols` terminal: each row's text
    /// (trailing blanks trimmed) and whether every non-blank cell is dim.
    fn readback(bytes: &[u8], rows: u16, cols: u16) -> Vec<(String, bool)> {
        let mut t = Terminal::with_scrollback(rows, cols, 0);
        t.process(bytes);
        let snap = Snapshot::from_term(&t);
        snap.cells
            .iter()
            .map(|row| {
                let cells = row.iter().filter(|c| c.width > 0);
                let text: String = cells.clone().map(|c| if c.ch == '\0' { ' ' } else { c.ch }).collect();
                let dim = cells.filter(|c| c.ch != ' ' && c.ch != '\0').all(|c| c.style.dim);
                (text.trim_end().to_string(), dim)
            })
            .collect()
    }

    /// A ring of the given rows; `Err(n)` marks a not-received hole of `n`
    /// lines at that point.
    fn ring_of(rows: &[Result<&str, u64>]) -> ScrollbackRing {
        let mut ring = ScrollbackRing::new(100);
        for r in rows {
            match r {
                Ok(s) => ring.append(&[format!("{s}\r\n").into_bytes()]),
                Err(n) => ring.mark_not_received(*n),
            }
        }
        ring
    }

    /// One fresh compose of `ring` at `offset` over a live screen showing
    /// `live0`, `live1`, ….
    fn compose_fresh(ring: &ScrollbackRing, arriving: u64, offset: usize, rows: u16, cols: u16) -> Vec<u8> {
        let mut server_term = Terminal::with_scrollback(rows, cols, 0);
        for r in 0..rows - 1 {
            server_term.process(format!("live{r}\r\n").as_bytes());
        }
        let mut memo: ScrollMemo = None;
        let mut initialized = false;
        let mut last_drawn = Snapshot::blank(rows, cols);
        compose_scroll_frame(
            offset,
            ring,
            arriving,
            &server_term,
            rows,
            cols,
            &mut memo,
            &mut initialized,
            &mut last_drawn,
            true,
        )
    }

    /// Row text with the centring padding trimmed too, plus the dim flag.
    fn trimmed(rows: &[(String, bool)]) -> Vec<(String, bool)> {
        rows.iter().map(|(t, d)| (t.trim().to_string(), *d)).collect()
    }

    fn row(text: &str, dim: bool) -> (String, bool) {
        (text.to_string(), dim)
    }

    #[test]
    fn a_hole_row_is_dim_centred_and_never_wraps() {
        let label = "12,340 lines not received";
        let want = format!("··· {label} ···");
        let mut bytes = hole_row(label, 40);
        bytes.extend_from_slice(b"x");
        let rows = readback(&bytes, 3, 40);
        let pad = (40 - want.chars().count()) / 2;
        assert_eq!(rows[0], (format!("{}{want}", " ".repeat(pad)), true), "centred and dim");
        assert_eq!(rows[1], row("x", false), "the dim pen does not bleed into the next row");

        let rows = readback(&hole_row(label, 10), 3, 10);
        assert_eq!(rows[0].0.chars().count(), 10, "truncated to the width: {:?}", rows[0]);
        assert!(rows[0].1, "the truncated row is dim");
        assert_eq!(rows[1], row("", true), "never wrapped onto a second row");
    }

    #[test]
    fn hole_labels_group_digits_and_agree_in_number() {
        assert_eq!(hole_label(1, false), "1 line not received");
        assert_eq!(hole_label(12_340, false), "12,340 lines not received");
        assert_eq!(hole_label(3_200, true), "3,200 lines arriving");
        assert_eq!(hole_label(1, true), "1 line arriving");
    }

    #[test]
    fn history_view_len_counts_holes_and_the_arriving_row() {
        let ring = ring_of(&[Ok("a"), Ok("b"), Err(3), Ok("c")]);
        assert_eq!(ring.len(), 3);
        assert_eq!(history_view_len(&ring, 0), 4, "rows plus the hole");
        assert_eq!(history_view_len(&ring, 7), 5, "plus one arriving row");
        assert_eq!(history_view_len(&ring_of(&[]), 1), 1, "an arriving row alone");
        assert_eq!(history_view_len(&ring_of(&[]), 0), 0);
    }

    #[test]
    fn compose_draws_a_not_received_hole_mid_history() {
        // `z` is the oldest row; at the top of history it sits under the bar.
        let ring = ring_of(&[Ok("z"), Ok("a"), Ok("b"), Err(5), Ok("c"), Ok("d")]);
        let rows = readback(&compose_fresh(&ring, 0, 6, 6, 40), 6, 40);
        assert!(rows[0].0.starts_with("-- SCROLLBACK"), "{:?}", rows[0]);
        assert_eq!(
            trimmed(&rows[1..]),
            vec![
                row("a", false),
                row("b", false),
                row("··· 5 lines not received ···", true),
                row("c", false),
                row("d", false),
            ]
        );
    }

    #[test]
    fn compose_draws_the_arriving_row_above_the_live_screen() {
        let ring = ring_of(&[Ok("a"), Ok("b")]);
        // Offset 2: `b` under the bar, then the arriving row, then the screen.
        let rows = readback(&compose_fresh(&ring, 300, 2, 6, 120), 6, 120);
        assert!(rows[0].0.contains("· 300 lines still arriving"), "{:?}", rows[0]);
        assert_eq!(
            trimmed(&rows[1..3]),
            vec![row("··· 300 lines arriving ···", true), row("live0", false)]
        );
    }

    #[test]
    fn compose_draws_both() {
        let ring = ring_of(&[Ok("z"), Ok("a"), Err(5), Ok("b")]);
        let top = history_view_len(&ring, 300);
        assert_eq!(top, 5);
        let rows = readback(&compose_fresh(&ring, 300, top, 8, 120), 8, 120);
        assert!(rows[0].0.contains("· 300 lines still arriving"), "{:?}", rows[0]);
        assert_eq!(
            trimmed(&rows[1..6]),
            vec![
                row("a", false),
                row("··· 5 lines not received ···", true),
                row("b", false),
                row("··· 300 lines arriving ···", true),
                row("live0", false),
            ]
        );
    }

    #[test]
    fn compose_caught_up_draws_no_hole_and_no_suffix() {
        let ring = ring_of(&[Ok("a"), Ok("b"), Ok("c")]);
        let rows = readback(&compose_fresh(&ring, 0, 3, 6, 120), 6, 120);
        assert_eq!(
            rows[0].0,
            "-- SCROLLBACK · 3 lines up · scroll down or press a key to resume --"
        );
        assert!(rows.iter().all(|(t, _)| !t.contains("···")), "{rows:?}");
    }

    #[test]
    fn the_memo_repaints_when_the_arriving_count_changes() {
        let ring = ring_of(&[Ok("a"), Ok("b")]);
        let server_term = Terminal::with_scrollback(6, 40, 0);
        let mut memo: ScrollMemo = None;
        let mut initialized = false;
        let mut last_drawn = Snapshot::blank(6, 40);
        let mut compose = |arriving| {
            compose_scroll_frame(
                2,
                &ring,
                arriving,
                &server_term,
                6,
                40,
                &mut memo,
                &mut initialized,
                &mut last_drawn,
                true,
            )
        };
        assert!(!compose(300).is_empty(), "the first compose paints");
        assert!(compose(300).is_empty(), "nothing changed");
        assert!(!compose(200).is_empty(), "the arriving count fell: repaint");
    }

    /// A mark merged into the tail hole relabels it without adding a view
    /// row: the memo must still move, or the old count stays on screen.
    #[test]
    fn the_memo_repaints_when_a_tail_hole_merges() {
        let mut ring = ring_of(&[Ok("a"), Ok("b")]);
        ring.mark_not_received(10);
        let server_term = Terminal::with_scrollback(6, 40, 0);
        let mut memo: ScrollMemo = None;
        let mut initialized = false;
        let mut last_drawn = Snapshot::blank(6, 40);
        let mut compose = |ring: &ScrollbackRing| {
            compose_scroll_frame(
                3,
                ring,
                0,
                &server_term,
                6,
                40,
                &mut memo,
                &mut initialized,
                &mut last_drawn,
                true,
            )
        };
        assert!(!compose(&ring).is_empty(), "the first compose paints");
        assert!(compose(&ring).is_empty(), "nothing changed");
        let view_rows = ring.view_total();
        ring.mark_not_received(5);
        assert_eq!(ring.view_total(), view_rows, "a merge adds no view row");
        // The repaint is a cell diff against the last draw: `10` → `15`
        // changes one cell, the `5`.
        let repaint = compose(&ring);
        assert!(!repaint.is_empty(), "the merged hole relabels: repaint");
        assert!(repaint.contains(&b'5'), "the new count's digit is drawn: {:?}", String::from_utf8_lossy(&repaint));
    }
}
