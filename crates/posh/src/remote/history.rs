//! RFC 0009 v2 history: the send cursor of the addressed, cumulatively
//! acknowledged scrollback stream. Shared by the single-peer `server_loop`
//! and the session daemon's paced viewports (posh#225 Stage 3).
//!
//! The session daemon opens a cursor per paced viewport (posh#225 Task 3.3);
//! `server_loop` uses the subset it always used. An item marked
//! `allow(dead_code)` has only test callers.

use posh_term::Terminal;

use crate::remote::caps::Scrollback2Client;
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
        true
    }

    /// Record the viewport's reported `size`. A change while active
    /// invalidates the epoch's row space (RFC 0009 §1.1: reflow; the viewport
    /// cleared its ring and awaits a fresh epoch), so the epoch bumps.
    pub(crate) fn on_client_size(&mut self, size: (u16, u16), total: u64) {
        if size != self.size {
            self.size = size;
            self.bump_epoch(total);
        }
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
    }

    /// Re-anchor the row space at the terminal's (reflowed) `total` without
    /// a new epoch: the viewport's ring and acks stay valid, and the next
    /// row offered is `sent_upto` — rows the reflow renumbered before they
    /// were sent become a forward jump. For a session width change seen by
    /// a viewport whose own size did not change (posh#225 Stage 3); a no-op
    /// when inactive.
    pub(crate) fn reanchor(&mut self, total: u64) {
        if !self.active {
            return;
        }
        self.anchor_abs = total;
        self.anchor_rel = self.sent_upto;
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
    pub(crate) fn resend_due(&self, now: u64, rto: u64) -> bool {
        self.acked_rows < self.sent_upto && now.saturating_sub(self.last_send) >= rto
    }

    /// A body is owed: fresh rows beyond the send cursor, or a resend from
    /// the ack.
    pub(crate) fn wants(&self, total: u64, now: u64, rto: u64) -> bool {
        self.active && (self.avail(total) > self.sent_upto || self.resend_due(now, rto))
    }

    pub(crate) fn in_flight(&self) -> u64 {
        self.sent_upto.saturating_sub(self.acked_rows)
    }

    #[allow(dead_code)]
    pub(crate) fn acked_rows(&self) -> u64 {
        self.acked_rows
    }

    pub(crate) fn sent_upto(&self) -> u64 {
        self.sent_upto
    }

    pub(crate) fn last_send(&self) -> u64 {
        self.last_send
    }

    /// The next body (RFC 0009 §2): rows of this epoch from the send cursor
    /// (or from the ack when a resend is due), never below what the ring
    /// retains — an evicted gap becomes a forward jump the viewport accepts
    /// as permanently-lost history — nor below `anchor_rel`, and at most
    /// `cap` rows. Advances the send cursor and stamps `now` as the last send.
    pub(crate) fn next_body(&mut self, term: &Terminal, now: u64, rto: u64, cap: u64) -> FrameBody {
        let total = term.primary_scrollback_total();
        let ring_len = term.primary_scrollback_len() as u64;
        let avail = self.avail(total);
        let floor_rel = self.anchor_rel.max(avail - ring_len.min(avail));
        let cursor = if self.resend_due(now, rto) {
            self.acked_rows
        } else {
            self.sent_upto
        };
        let start = cursor.max(floor_rel);
        let count = (avail - start).min(cap) as usize;
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
        FrameBody::Scrollback2 {
            epoch: self.epoch,
            row_offset: start,
            rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTO: u64 = 100;

    /// A 5x20 terminal with a 50-row ring, its screen filled so every
    /// further line scrolls exactly one row into the ring.
    fn term() -> Terminal {
        let mut t = Terminal::with_scrollback(5, 20, 50);
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

    fn split(body: FrameBody) -> (u8, u64, Vec<Vec<u8>>) {
        match body {
            FrameBody::Scrollback2 {
                epoch,
                row_offset,
                rows,
            } => (epoch, row_offset, rows),
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
        assert!(!c.wants(total(&t), 0, RTO));
        assert!(!c.wants(total(&t), 10 * RTO, RTO));
        c.on_client_size((6, 20), total(&t));
        assert_eq!(c.epoch(), None, "an inactive cursor never bumps");
        c.bump_epoch(total(&t));
        assert_eq!(c.epoch(), None);
        c.activate(HistoryStart::Fresh, total(&t));
        assert_eq!(c.epoch(), Some(1));
        // The size was recorded while inactive: the same size is no change.
        c.on_client_size((6, 20), total(&t));
        assert_eq!(c.epoch(), Some(1));
    }

    #[test]
    fn a_fresh_cursor_numbers_rows_from_its_opening_total() {
        let mut t = term();
        scroll(&mut t, 7);
        let mut c = active(&t);
        scroll(&mut t, 10);
        assert_eq!(c.avail(total(&t)), 10);
        assert!(c.wants(total(&t), 0, RTO));
        let (epoch, row_offset, rows) = split(c.next_body(&t, 0, RTO, 4));
        assert_eq!((epoch, row_offset), (1, 0));
        let len = ring_len(&t);
        let expected: Vec<Vec<u8>> = (0..4)
            .map(|k| t.dump_scrollback_row((len - 10 + k) as usize).unwrap())
            .collect();
        assert_eq!(rows, expected);
        assert_eq!(c.sent_upto(), 4);
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, 4));
        assert_eq!(row_offset, 4);
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn a_body_carries_at_most_the_cap() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 600);
        let mut lens = Vec::new();
        let mut next = None;
        while c.wants(total(&t), 0, RTO) {
            let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, 16));
            if let Some(n) = next {
                assert_eq!(row_offset, n, "bodies are contiguous");
            }
            next = Some(row_offset + rows.len() as u64);
            lens.push(rows.len());
        }
        assert_eq!(lens, vec![16, 16, 16, 2]);
        assert_eq!(c.sent_upto(), 600);
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
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, 256));
        assert_eq!((row_offset, rows.len()), (0, 8));
        assert_eq!(c.last_send(), 0);
        c.on_ack(1, 3);
        assert_eq!(c.in_flight(), 5);
        assert!(!c.resend_due(10, RTO));
        assert!(!c.wants(total(&t), 10, RTO), "caught up and not yet due");
        assert!(c.resend_due(100, RTO));
        assert!(c.wants(total(&t), 100, RTO));
        let (_, row_offset, rows) = split(c.next_body(&t, 100, RTO, 256));
        assert_eq!((row_offset, rows), (3, newest_rows(&t, 5)));
        assert_eq!(c.last_send(), 100);
    }

    #[test]
    fn evicted_rows_become_one_forward_jump() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, 256));
        assert_eq!((row_offset, rows.len()), (0, 10));
        scroll(&mut t, 100);
        let avail = c.avail(total(&t));
        assert_eq!(avail, 110);
        let (_, row_offset, rows) = split(c.next_body(&t, 1, RTO, 20));
        assert_eq!(row_offset, avail - 50, "a jump of avail - 50 - 10 rows");
        assert!(row_offset > 10);
        assert_eq!(rows, newest_rows(&t, 50)[..20].to_vec());
        let (_, row_offset, rows) = split(c.next_body(&t, 1, RTO, 20));
        assert_eq!(row_offset, avail - 50 + 20, "contiguous after the jump");
        assert_eq!(rows, newest_rows(&t, 50)[20..40].to_vec());
    }

    #[test]
    fn a_size_change_bumps_the_epoch_and_reanchors() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        c.next_body(&t, 0, RTO, 256);
        c.on_ack(1, 4);
        c.on_client_size((6, 20), total(&t));
        assert_eq!(c.epoch(), Some(2));
        assert_eq!(c.avail(total(&t)), 0);
        assert_eq!(c.acked_rows(), 0);
        assert_eq!(c.sent_upto(), 0);
        c.on_client_size((6, 20), total(&t));
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
    fn reanchor_keeps_the_epoch_and_continues_from_the_send_cursor() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        c.next_body(&t, 0, RTO, 6);
        c.on_ack(1, 4);
        assert_eq!(c.sent_upto(), 6);
        // A reflow renumbers the ring: the total jumps, and rows 6..10 were
        // never sent.
        scroll(&mut t, 30);
        c.reanchor(total(&t));
        assert_eq!(c.epoch(), Some(1), "no new epoch");
        assert_eq!(c.acked_rows(), 4, "the viewport's ack stays valid");
        assert_eq!(c.sent_upto(), 6);
        assert_eq!(c.avail(total(&t)), 6, "avail continues from sent_upto");
        scroll(&mut t, 3);
        assert_eq!(c.avail(total(&t)), 9);
        let (epoch, row_offset, rows) = split(c.next_body(&t, 1, RTO, 256));
        assert_eq!((epoch, row_offset, rows), (1, 6, newest_rows(&t, 3)));

        let mut idle = HistoryCursor::new((5, 20));
        idle.reanchor(total(&t));
        assert_eq!(idle.epoch(), None, "an inactive cursor is untouched");
    }

    #[test]
    fn bump_epoch_reanchors_unconditionally_when_active() {
        let mut t = term();
        let mut c = active(&t);
        scroll(&mut t, 10);
        c.next_body(&t, 0, RTO, 256);
        c.on_ack(1, 4);
        c.bump_epoch(total(&t));
        assert_eq!(c.epoch(), Some(2));
        assert_eq!(c.avail(total(&t)), 0);
        assert_eq!(c.acked_rows(), 0);
        assert_eq!(c.sent_upto(), 0);
        c.bump_epoch(total(&t));
        assert_eq!(c.epoch(), Some(3), "no size change needed");
        scroll(&mut t, 2);
        let (epoch, row_offset, rows) = split(c.next_body(&t, 0, RTO, 256));
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
        let (epoch, row_offset, rows) = split(c.next_body(&t, 0, RTO, 256));
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
        let (_, row_offset, rows) = split(c.next_body(&t, 0, RTO, 256));
        assert_eq!((row_offset, rows.len()), (40, 5));
        assert!(c.resend_due(RTO, RTO), "no ack: the RTO passes");
        let (_, row_offset, rows) = split(c.next_body(&t, RTO, RTO, 256));
        assert_eq!((row_offset, rows), (40, newest_rows(&t, 5)));
        // The anchor, not the ack, is the floor: even an ack below it (which
        // the API cannot produce) resends from the anchor, though the ring
        // holds 45 older rows.
        c.acked_rows = 0;
        let (_, row_offset, _) = split(c.next_body(&t, 3 * RTO, RTO, 256));
        assert_eq!(row_offset, 40);
    }
}
