//! RFC 0009 v2 history: the send cursor of the addressed, cumulatively
//! acknowledged scrollback stream. Shared by the single-peer `server_loop`
//! and the session daemon's paced viewports (posh#225 Stage 3).
//!
//! The session daemon opens a cursor per paced viewport (posh#225 Task 3.3);
//! `server_loop` uses the subset it always used, with an unbounded window.

use posh_term::Terminal;

use crate::remote::caps::{Scrollback2Client, Scrollback2Extent};
use crate::remote::sync::FrameBody;

/// Max rows per v2 body (RFC 0009 §2): chunks a long-disconnect resend into
/// fragmentation-friendly frames; the cumulative repeat loop carries the
/// rest forward as acks advance.
pub(crate) const SB2_ROWS_PER_BODY: u64 = 256;

/// Where a cursor's row space starts (decision 10: a per-viewport input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryStart {
    /// The cursor's own epoch; row 0 is the next row scrolled.
    Fresh,
    /// The viewport's epoch, continued: row `rows` is the next row scrolled.
    /// Forward-only — rows scrolled while it was elsewhere are not this
    /// cursor's (Stage 5 passes an earlier position from the resume cursor).
    Continue { epoch: u8, rows: u64 },
}

/// The send side of one viewport's v2 history stream (RFC 0009): an epoch,
/// an epoch-relative row space anchored to the terminal's monotonic
/// primary-scrollback total, the viewport's cumulative ack, and the send
/// cursor. Inactive (no epoch, no bodies, no ack entry) until activated — by
/// the viewport's first `SCROLLBACK2` advertisement in `server_loop` — while
/// the viewport's size is tracked from construction, so a size change that
/// arrives with (or before) activation is judged against the right size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryCursor {
    active: bool,
    epoch: u8,
    /// The monotonic primary-scrollback total at which relative row
    /// `anchor_rel` begins (relative row r is absolute row
    /// `anchor_abs + r - anchor_rel`).
    anchor_abs: u64,
    /// The first relative row this cursor may offer: 0 for its own epoch,
    /// the viewport's count for a continued one.
    anchor_rel: u64,
    acked_rows: u64,
    sent_upto: u64,
    last_send: u64,
    /// Resend bodies since the ack last advanced (the caller's backoff
    /// exponent); zeroed with every new row space.
    resends: u32,
    size: (u16, u16),
}

impl HistoryCursor {
    /// An inactive cursor for a viewport of `size`; its epoch, once
    /// activated fresh, is 1.
    pub(crate) fn new(size: (u16, u16)) -> Self {
        HistoryCursor {
            active: false,
            epoch: 1,
            anchor_abs: 0,
            anchor_rel: 0,
            acked_rows: 0,
            sent_upto: 0,
            last_send: 0,
            resends: 0,
            size,
        }
    }

    /// Open the stream at the terminal's current primary-scrollback `total`.
    /// A no-op when already active.
    pub(crate) fn activate(&mut self, start: HistoryStart, total: u64) {
        if self.active {
            return;
        }
        self.active = true;
        self.anchor_abs = total;
        let rows = match start {
            HistoryStart::Fresh => 0,
            HistoryStart::Continue { epoch, rows } => {
                self.epoch = epoch;
                rows
            }
        };
        self.anchor_rel = rows;
        self.acked_rows = rows;
        self.sent_upto = rows;
    }

    /// `server_loop`'s per-message `SCROLLBACK2` entry (RFC 0009 §1): the
    /// first advertisement opens a fresh epoch at the current monotonic
    /// total; every entry carries the viewport's cumulative ack, valid only
    /// when its epoch byte matches ours (a stale-epoch ack is ignored).
    pub(crate) fn on_client_entry(&mut self, entry: &Scrollback2Client, total: u64) {
        self.activate(HistoryStart::Fresh, total);
        self.on_ack(entry.epoch, entry.acked_rows);
    }

    /// A cumulative ack of `acked_rows` in `epoch`; ignored when inactive or
    /// in another epoch, and never moves backwards. Returns whether it
    /// advanced.
    pub(crate) fn on_ack(&mut self, epoch: u8, acked_rows: u64) -> bool {
        if !self.active || epoch != self.epoch || acked_rows <= self.acked_rows {
            return false;
        }
        self.acked_rows = acked_rows;
        self.resends = 0;
        true
    }

    /// Record the viewport's reported `size`. A change while active
    /// invalidates the epoch's row space (RFC 0009 §1.1: reflow; the viewport
    /// cleared its ring and awaits a fresh epoch), so the epoch bumps.
    /// Returns whether it bumped.
    pub(crate) fn on_client_size(&mut self, size: (u16, u16), total: u64) -> bool {
        if size == self.size {
            return false;
        }
        self.size = size;
        self.bump_epoch(total);
        self.active
    }

    /// Open the next epoch at `total` (the byte wraps past 0, which a
    /// viewport advertises to mean "no epoch held": 255 → 1); a no-op when
    /// inactive. In-flight bodies of the old epoch are discarded by the
    /// viewport, and its acks of them ignored here.
    pub(crate) fn bump_epoch(&mut self, total: u64) {
        if !self.active {
            return;
        }
        self.epoch = match self.epoch.wrapping_add(1) {
            0 => 1,
            next => next,
        };
        self.anchor_abs = total;
        self.anchor_rel = 0;
        self.acked_rows = 0;
        self.sent_upto = 0;
        self.resends = 0;
    }

    /// Re-anchor the row space at the terminal's renumbered `total` without
    /// a new epoch, for a session resize seen by a viewport whose own size
    /// did not change (posh#225 Stage 3): a width reflow, or a height grow
    /// that popped ring rows back onto the grid — neither moves the total,
    /// so the old mapping would offer other rows under the old numbers. The
    /// viewport's ring and acks stay valid; numbering continues at the count
    /// as of the resize, so rows not yet sent, and rows sent but lost (a
    /// resend from the ack is floored at the anchor: a 0-row body), become
    /// ONE forward jump the viewport records — nothing is silently skipped.
    /// A no-op when inactive.
    pub(crate) fn reanchor(&mut self, total: u64) {
        if !self.active {
            return;
        }
        let at = self.avail(total);
        self.anchor_abs = total;
        self.anchor_rel = at;
        self.resends = 0;
    }

    /// The current epoch, or `None` while inactive (no ack entry is owed).
    pub(crate) fn epoch(&self) -> Option<u8> {
        self.active.then_some(self.epoch)
    }

    /// Relative rows available in this epoch at the monotonic `total`.
    pub(crate) fn avail(&self, total: u64) -> u64 {
        self.anchor_rel + total.saturating_sub(self.anchor_abs)
    }

    /// Rows are sent but unacked and the last body is at least `rto` old.
    fn resend_due(&self, now: u64, rto: u64) -> bool {
        self.in_flight() > 0 && now.saturating_sub(self.last_send) >= rto
    }

    /// When the next body is due, for a viewport allowed `window` rows in
    /// flight: fresh rows with room in the window are due at once (the last
    /// send, never later than now); otherwise rows in flight are due their
    /// resend at `rto` past the last send. `None` while inactive, or with
    /// nothing fresh and nothing in flight.
    pub(crate) fn next_due(&self, total: u64, rto: u64, window: u64) -> Option<u64> {
        if !self.active {
            return None;
        }
        if self.avail(total) > self.next_fresh() && self.in_flight() < window {
            return Some(self.last_send);
        }
        (self.in_flight() > 0).then(|| self.last_send + rto)
    }

    /// A body is owed at `now` ([`Self::next_due`]).
    pub(crate) fn wants(&self, total: u64, now: u64, rto: u64, window: u64) -> bool {
        self.next_due(total, rto, window).is_some_and(|at| now >= at)
    }

    /// Resend bodies since the ack last advanced.
    pub(crate) fn resends(&self) -> u32 {
        self.resends
    }

    fn in_flight(&self) -> u64 {
        self.sent_upto.saturating_sub(self.acked_rows)
    }

    /// Where a fresh body starts: past everything sent and everything acked
    /// (a late ack for a wider window can pass a resend-rewound cursor).
    fn next_fresh(&self) -> u64 {
        self.sent_upto.max(self.acked_rows)
    }

    #[cfg(test)]
    pub(crate) fn acked_rows(&self) -> u64 {
        self.acked_rows
    }

    #[cfg(test)]
    pub(crate) fn sent_upto(&self) -> u64 {
        self.sent_upto
    }

    #[cfg(test)]
    pub(crate) fn last_send(&self) -> u64 {
        self.last_send
    }

    /// Rows available in this epoch at `term`, and the floor below which no
    /// body starts — the ring's eviction floor, never below `anchor_rel`
    /// (a re-anchor's count, a continued viewport's count). One computation
    /// for `next_body` and `extent`, so the marker and the jump agree.
    fn avail_and_floor(&self, term: &Terminal) -> (u64, u64) {
        let avail = self.avail(term.primary_scrollback_total());
        let ring_len = term.primary_scrollback_len() as u64;
        (avail, self.anchor_rel.max(avail - ring_len.min(avail)))
    }

    /// What a viewport needs to tell eviction from loss (RFC 0009 §3.1,
    /// posh#225 Stage 4): `None` while inactive.
    pub(crate) fn extent(&self, term: &Terminal) -> Option<Scrollback2Extent> {
        self.active.then(|| {
            let (avail_rows, evicted_upto) = self.avail_and_floor(term);
            Scrollback2Extent {
                epoch: self.epoch,
                avail_rows,
                evicted_upto,
            }
        })
    }

    /// The next body and its epoch (RFC 0009 §2): rows of this epoch from
    /// the send cursor (or from the ack when a resend is due), never below
    /// what the ring retains — an evicted gap becomes a forward jump the
    /// viewport accepts as permanently-lost history — nor below
    /// `anchor_rel`. A resend takes a whole body ([`SB2_ROWS_PER_BODY`]); a
    /// fresh body at most the room left in `window`. Advances the send
    /// cursor and stamps `now` as the last send.
    pub(crate) fn next_body(&mut self, term: &Terminal, now: u64, rto: u64, window: u64) -> (u8, FrameBody) {
        let ring_len = term.primary_scrollback_len() as u64;
        let (avail, floor_rel) = self.avail_and_floor(term);
        let (cursor, cap) = if self.resend_due(now, rto) {
            self.resends += 1;
            (self.acked_rows, SB2_ROWS_PER_BODY)
        } else {
            let room = window.saturating_sub(self.in_flight());
            (self.next_fresh(), SB2_ROWS_PER_BODY.min(room))
        };
        let start = cursor.max(floor_rel);
        let count = avail.saturating_sub(start).min(cap) as usize;
        let rows = (0..count)
            .map(|k| {
                let r = start + k as u64;
                // Relative row r sits at ring index ring_len - (avail - r):
                // the newest row (avail - 1) is the newest ring row.
                let idx = (ring_len - (avail - r)) as usize;
                term.dump_scrollback_row(idx).unwrap_or_default()
            })
            .collect();
        self.sent_upto = start + count as u64;
        self.last_send = now;
        let body = FrameBody::Scrollback2 {
            epoch: self.epoch,
            row_offset: start,
            rows,
        };
        (self.epoch, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTO: u64 = 100;
    /// `server_loop`'s window: no limit.
    const ANY: u64 = u64::MAX;

    /// A 5x20 terminal with a 50-row ring, its screen filled so every
    /// further line scrolls exactly one row into the ring.
    fn term() -> Terminal {
        term_with_ring(50)
    }

    fn term_with_ring(ring: usize) -> Terminal {
        let mut t = Terminal::with_scrollback(5, 20, ring);
        t.process(b"a\r\nb\r\nc\r\nd\r\n");
        assert_eq!(t.primary_scrollback_total(), 0);
        t
    }

    /// Scroll `n` distinct rows into the ring.
    fn scroll(t: &mut Terminal, n: u64) {
        let first = t.primary_scrollback_total();
        for i in first..first + n {
            t.process(format!("{i:04}\r\n").as_bytes());
        }
        assert_eq!(t.primary_scrollback_total(), first + n);
    }

    fn total(t: &Terminal) -> u64 {
        t.primary_scrollback_total()
    }

    fn ring_len(t: &Terminal) -> u64 {
        t.primary_scrollback_len() as u64
    }

    /// The newest `n` ring rows, oldest first.
    fn newest_rows(t: &Terminal, n: u64) -> Vec<Vec<u8>> {
        let len = ring_len(t);
        (len - n..len)
            .map(|i| t.dump_scrollback_row(i as usize).unwrap())
            .collect()
    }

    fn split((epoch, body): (u8, FrameBody)) -> (u8, u64, Vec<Vec<u8>>) {
        match body {
            FrameBody::Scrollback2 {
                epoch: in_body,
                row_offset,
                rows,
            } => {
                assert_eq!(epoch, in_body, "the returned epoch is the body's");
                (epoch, row_offset, rows)
            }
            other => panic!("not a v2 body: {other:?}"),
        }
    }

    fn active(t: &Terminal) -> HistoryCursor {
        let mut c = HistoryCursor::new((5, 20));
        c.activate(HistoryStart::Fresh, total(t));
        c
    }

    #[test]
    fn a_cursor_is_inert_until_activated() {
        let mut t = term();
        let mut c = HistoryCursor::new((5, 20));
        scroll(&mut t, 10);
        assert_eq!(c.epoch(), None);
        assert_eq!(c.next_due(total(&t), RTO, ANY), None);
        assert!(!c.wants(total(&t), 10 * RTO, RTO, ANY));
        assert!(!c.on_client_size((6, 20), total(&t)), "an inactive cursor never bumps");
        assert_eq!(c.epoch(), None);
        c.bump_epoch(total(&t));
        assert_eq!(c.epoch(), None);
        c.activate(HistoryStart::Fresh, total(&t));
        assert_eq!(c.epoch(), Some(1));
        // The size was recorded while inactive: the same size is no change.
        assert!(!c.on_client_size((6, 20), total(&t)));
        assert_eq!(c.epoch(), Some(1));
    }

    #[test]
    fn a_fresh_cursor_numbers_rows_from_its_opening_total() {
        let mut t = term();
        scroll(&mut t, 7);
        let mut c = active(&t);
        scroll(&mut t, 10);
        assert_eq!(c.avail(total(&t)), 10);
        assert_eq!(c.next_due(total(&t), RTO, 4), Some(0), "fresh rows: due at once");
        let (epoch, row_offset, rows) = split(c.next_body(&t, 0, RTO, 4));
        assert_eq!((epoch, row_offset), (1, 0));
        let len = ring_len(&t);
        let expected: Vec<Vec<u8>> = (0..4)
            .map(|k| t.dump_scrollback_row((len - 10 + k) as usize).unwrap())
            .collect();
        assert_eq!(rows, expected);
        assert_eq!(c.sent_upto(), 4);
        assert_eq!(c.next_due(total(&t), RTO, 4), Some(RTO), "the window is full");
        c.on_ack(1, 4);
        assert_eq!(c.next_due(total(&t), RTO, 4), Some(0), "room again");
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, 4));
        assert_eq!(row_offset, 4);
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn a_body_carries_at_most_sb2_rows_per_body() {
        let mut t = term_with_ring(1000);
        let mut c = active(&t);
        scroll(&mut t, 600);
        let mut lens = Vec::new();
        let mut next = None;
        while c.wants(total(&t), 0, RTO, ANY) {
            let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, ANY));
            if let Some(n) = next {
                assert_eq!(row_offset, n, "bodies are contiguous");
            }
            next = Some(row_offset + rows.len() as u64);
            lens.push(rows.len());
        }
        assert_eq!(lens, vec![256, 256, 88]);
        assert_eq!(c.sent_upto(), 600);
    }

    #[test]
    fn a_fresh_body_takes_at_most_the_room_in_the_window() {
        let mut t = term_with_ring(1000);
        let mut c = active(&t);
        scroll(&mut t, 600);
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, 300));
        assert_eq!((row_offset, rows.len()), (0, 256));
        assert_eq!(c.next_due(total(&t), RTO, 300), Some(0), "room for 44 more");
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, 300));
        assert_eq!((row_offset, rows.len()), (256, 44));
        assert_eq!(c.next_due(total(&t), RTO, 300), Some(RTO), "full: only the resend is due");
        c.on_ack(1, 256);
        let (_, row_offset, rows) = split(c.next_body(&t, 1, RTO, 300));
        assert_eq!((row_offset, rows.len()), (300, 256));
    }

    #[test]
    fn an_ack_moves_only_forward_and_only_in_its_epoch() {
        let t = term();
        let mut c = active(&t);
        assert!(c.on_ack(1, 5));
        assert_eq!(c.acked_rows(), 5);
        assert!(!c.on_ack(1, 3));
        assert_eq!(c.acked_rows(), 5);
        assert!(!c.on_ack(2, 9), "a stale-epoch ack is ignored");
        assert_eq!(c.acked_rows(), 5);
    }

    #[test]
    fn a_resend_starts_at_the_ack_and_waits_for_the_rto() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 8);
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, ANY));
        assert_eq!((row_offset, rows.len()), (0, 8));
        assert_eq!(c.last_send(), 0);
        c.on_ack(1, 3);
        assert_eq!(c.in_flight(), 5);
        assert!(!c.resend_due(10, RTO));
        assert!(!c.wants(total(&t), 10, RTO, ANY), "caught up and not yet due");
        assert_eq!(c.next_due(total(&t), RTO, ANY), Some(RTO));
        assert!(c.resend_due(100, RTO));
        assert!(c.wants(total(&t), 100, RTO, ANY));
        let (_, row_offset, rows) = split(c.next_body(&t, 100, RTO, ANY));
        assert_eq!((row_offset, rows), (3, newest_rows(&t, 5)));
        assert_eq!(c.last_send(), 100);
        assert_eq!(c.resends(), 1, "a resend counts");
        split(c.next_body(&t, 200, RTO, ANY));
        assert_eq!(c.resends(), 2);
        assert!(!c.on_ack(1, 3), "a repeated ack does not advance");
        assert_eq!(c.resends(), 2, "nor reset the count");
        assert!(c.on_ack(1, 5));
        assert_eq!(c.resends(), 0, "an advancing ack resets it");
    }

    #[test]
    fn a_late_ack_past_a_rewound_resend_is_not_sent_again() {
        let mut t = term_with_ring(1000);
        let mut c = active(&t);
        scroll(&mut t, 600);
        while c.wants(total(&t), 0, RTO, ANY) {
            c.next_body(&t, 0, RTO, ANY);
        }
        c.on_ack(1, 3);
        let (_, row_offset, rows) = split(c.next_body(&t, RTO, RTO, ANY));
        assert_eq!((row_offset, rows.len()), (3, 256), "the resend rewinds the cursor");
        assert_eq!(c.sent_upto(), 259);
        // The ack of the first window's bodies lands after the resend.
        assert!(c.on_ack(1, 400));
        assert_eq!(c.next_due(total(&t), RTO, ANY), Some(RTO), "rows past the ack: due");
        let (_, row_offset, rows) = split(c.next_body(&t, RTO + 1, RTO, ANY));
        assert_eq!((row_offset, rows), (400, newest_rows(&t, 200)), "fresh from the ack");
    }

    #[test]
    fn evicted_rows_become_one_forward_jump() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, ANY));
        assert_eq!((row_offset, rows.len()), (0, 10));
        scroll(&mut t, 100);
        let avail = c.avail(total(&t));
        assert_eq!(avail, 110);
        let (_, row_offset, rows) = split(c.next_body(&t, 1, RTO, ANY));
        assert_eq!(row_offset, avail - 50, "a jump of avail - 50 - 10 rows");
        assert!(row_offset > 10);
        assert_eq!(rows, newest_rows(&t, 50), "then the whole retained ring");
        assert_eq!(c.sent_upto(), avail);
    }

    #[test]
    fn a_size_change_bumps_the_epoch_and_reanchors() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        c.next_body(&t, 0, RTO, ANY);
        c.on_ack(1, 4);
        split(c.next_body(&t, RTO, RTO, ANY));
        assert_eq!(c.resends(), 1);
        assert!(c.on_client_size((6, 20), total(&t)), "it bumped");
        assert_eq!(c.epoch(), Some(2));
        assert_eq!(c.avail(total(&t)), 0);
        assert_eq!(c.acked_rows(), 0);
        assert_eq!(c.sent_upto(), 0);
        assert_eq!(c.resends(), 0);
        assert!(!c.on_client_size((6, 20), total(&t)));
        assert_eq!(c.epoch(), Some(2), "the same size again is no change");

        let mut wrap = HistoryCursor::new((5, 20));
        wrap.activate(
            HistoryStart::Continue {
                epoch: 255,
                rows: 0,
            },
            total(&t),
        );
        wrap.on_client_size((5, 21), total(&t));
        assert_eq!(wrap.epoch(), Some(1), "the epoch byte wraps past 0 (\"none held\")");
    }

    #[test]
    fn reanchor_keeps_the_epoch_and_jumps_to_the_count_at_the_resize() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        c.next_body(&t, 0, RTO, 8);
        c.on_ack(1, 4);
        assert_eq!(c.sent_upto(), 8);
        // A reflow renumbers the ring without moving the total; rows 8..10
        // were never sent, rows 4..8 are in flight.
        t.resize(5, 30);
        assert_eq!(total(&t), 10);
        c.reanchor(total(&t));
        assert_eq!(c.epoch(), Some(1), "no new epoch");
        assert_eq!(c.acked_rows(), 4, "the viewport's ack stays valid");
        assert_eq!(c.sent_upto(), 8);
        assert_eq!(c.in_flight(), 4);
        assert_eq!(c.avail(total(&t)), 10, "numbering continues at the count");

        // A resend from the lagging ack is floored at the anchor: an empty
        // body at the reflow count, inside the same jump.
        let mut lagging = c;
        let (_, row_offset, rows) = split(lagging.next_body(&t, RTO, RTO, ANY));
        assert_eq!((row_offset, rows.len()), (10, 0));
        assert_eq!(lagging.resends(), 1);
        lagging.reanchor(total(&t));
        assert_eq!(lagging.resends(), 0, "a new row space resets the backoff");

        scroll(&mut t, 3);
        assert_eq!(c.avail(total(&t)), 13);
        let (epoch, row_offset, rows) = split(c.next_body(&t, 1, RTO, ANY));
        assert_eq!((epoch, row_offset, rows), (1, 10, newest_rows(&t, 3)));
        // What a reader that expects the next body at its last end sees.
        let expected_next = 8;
        assert_eq!(row_offset - expected_next, 2, "a forward jump of the 2 unsent rows");

        let mut idle = HistoryCursor::new((5, 20));
        idle.reanchor(total(&t));
        assert_eq!(idle.epoch(), None, "an inactive cursor is untouched");
    }

    #[test]
    fn bump_epoch_reanchors_unconditionally_when_active() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        c.next_body(&t, 0, RTO, ANY);
        c.on_ack(1, 4);
        c.bump_epoch(total(&t));
        assert_eq!(c.epoch(), Some(2));
        assert_eq!(c.avail(total(&t)), 0);
        assert_eq!(c.acked_rows(), 0);
        assert_eq!(c.sent_upto(), 0);
        c.bump_epoch(total(&t));
        assert_eq!(c.epoch(), Some(3), "no size change needed");
        scroll(&mut t, 2);
        let (epoch, row_offset, rows) = split(c.next_body(&t, 0, RTO, ANY));
        assert_eq!((epoch, row_offset, rows), (3, 0, newest_rows(&t, 2)));
    }

    #[test]
    fn a_continued_cursor_resumes_the_viewports_count() {
        let mut t = term();
        scroll(&mut t, 20);
        let mut c = HistoryCursor::new((5, 20));
        c.activate(HistoryStart::Continue { epoch: 7, rows: 40 }, total(&t));
        scroll(&mut t, 3);
        assert_eq!(c.epoch(), Some(7));
        assert_eq!(c.avail(total(&t)), 43);
        assert_eq!(c.acked_rows(), 40);
        assert_eq!(c.sent_upto(), 40);
        let (epoch, row_offset, rows) = split(c.next_body(&t, 0, RTO, ANY));
        assert_eq!((epoch, row_offset, rows), (7, 40, newest_rows(&t, 3)));
    }

    #[test]
    fn a_continued_cursor_never_offers_rows_from_before_its_anchor() {
        let mut t = term();
        scroll(&mut t, 60);
        assert_eq!(ring_len(&t), 50, "the ring is full");
        let mut c = HistoryCursor::new((5, 20));
        c.activate(HistoryStart::Continue { epoch: 7, rows: 40 }, total(&t));
        scroll(&mut t, 5);
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, ANY));
        assert_eq!((row_offset, rows.len()), (40, 5));
        assert!(c.resend_due(RTO, RTO), "no ack: the RTO passes");
        let (_, row_offset, rows) = split(c.next_body(&t, RTO, RTO, ANY));
        assert_eq!((row_offset, rows), (40, newest_rows(&t, 5)));
        // The anchor, not the ack, is the floor: even an ack below it (which
        // the API cannot produce) resends from the anchor, though the ring
        // holds 45 older rows.
        c.acked_rows = 0;
        let (_, row_offset, _) = split(c.next_body(&t, 3 * RTO, RTO, ANY));
        assert_eq!(row_offset, 40);
    }

    // ---- posh#225 Stage 4: the extent (RFC 0009 §3.1) ----

    fn ext(epoch: u8, avail_rows: u64, evicted_upto: u64) -> Option<Scrollback2Extent> {
        Some(Scrollback2Extent {
            epoch,
            avail_rows,
            evicted_upto,
        })
    }

    #[test]
    fn the_extent_is_none_until_activated() {
        let mut t = term();
        let mut c = HistoryCursor::new((5, 20));
        scroll(&mut t, 10);
        assert_eq!(c.extent(&t), None);
        c.activate(HistoryStart::Fresh, total(&t));
        assert_eq!(c.extent(&t), ext(1, 0, 0));
    }

    #[test]
    fn the_extent_counts_the_rows_of_the_epoch() {
        let mut t = term();
        let c = active(&t);
        scroll(&mut t, 10);
        assert_eq!(c.extent(&t), ext(1, 10, 0));
    }

    /// The marker and the jump agree: the floor the extent reports is
    /// where the next body starts.
    #[test]
    fn the_extent_floor_rises_as_the_ring_evicts() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        c.next_body(&t, 0, RTO, ANY);
        scroll(&mut t, 100);
        let x = c.extent(&t).unwrap();
        assert_eq!(x.avail_rows, 110);
        assert_eq!(x.evicted_upto, x.avail_rows - 50);
        let (_, row_offset, _) = split(c.next_body(&t, 1, RTO, ANY));
        assert_eq!(row_offset, x.evicted_upto, "the jump lands on the reported floor");
    }

    #[test]
    fn the_extent_floor_is_the_count_at_a_reanchor() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        c.next_body(&t, 0, RTO, 8);
        c.on_ack(1, 4);
        t.resize(5, 30);
        c.reanchor(total(&t));
        assert_eq!(c.extent(&t).unwrap().evicted_upto, 10);
        scroll(&mut t, 3);
        assert_eq!(c.extent(&t), ext(1, 13, 10));
        let (_, row_offset, _) = split(c.next_body(&t, 1, RTO, ANY));
        assert_eq!(row_offset, 10, "the next body starts at the floor");
    }

    /// A continued cursor's floor is the viewport's own count, so it labels
    /// nothing the viewport did not already hold.
    #[test]
    fn a_continued_cursors_floor_is_the_viewports_count() {
        let mut t = term();
        scroll(&mut t, 60);
        assert_eq!(ring_len(&t), 50, "the ring is full");
        let mut c = HistoryCursor::new((5, 20));
        c.activate(HistoryStart::Continue { epoch: 7, rows: 40 }, total(&t));
        assert_eq!(c.extent(&t), ext(7, 40, 40));
    }

    #[test]
    fn the_extent_floor_never_falls_within_an_epoch() {
        let mut t = term();
        let mut c = active(&t);
        let mut floors = Vec::new();
        let mut note = |c: &HistoryCursor, t: &Terminal| floors.push(c.extent(t).unwrap().evicted_upto);
        scroll(&mut t, 10);
        note(&c, &t);
        c.next_body(&t, 0, RTO, ANY);
        note(&c, &t);
        scroll(&mut t, 120);
        note(&c, &t);
        c.next_body(&t, 1, RTO, ANY);
        note(&c, &t);
        // A height shrink pushes grid rows into the ring (the total moves);
        // a height grow pops ring rows back without moving it, which the
        // daemon follows with a re-anchor.
        t.resize(3, 20);
        note(&c, &t);
        t.resize(8, 20);
        c.reanchor(total(&t));
        note(&c, &t);
        scroll(&mut t, 70);
        note(&c, &t);
        c.next_body(&t, 2, RTO, ANY);
        note(&c, &t);
        assert!(floors.windows(2).all(|w| w[0] <= w[1]), "non-decreasing: {floors:?}");
        assert!(floors.last() > floors.first(), "and it rose: {floors:?}");
    }

    #[test]
    fn a_bump_resets_the_extent() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 100);
        c.next_body(&t, 0, RTO, ANY);
        c.bump_epoch(total(&t));
        assert_eq!(c.extent(&t), ext(2, 0, 0));
    }
}
