//! `DumpDiff` codec (#15): today's behavior, extracted verbatim. The server
//! ships the full `dump_vt` either whole (`Full`) or as a prefix/suffix
//! byte-diff against the acked frame (`Diff`); the client reconstructs the full
//! dump and reparses it into a fresh `Terminal`. This is the default codec, the
//! keyframe path for the others, and the only one a baseline peer ever
//! negotiates — so behavior is unchanged when it is selected.

use posh_term::Terminal;

use crate::frame::{self, FrameBody};

use super::{ApplyOutcome, Baseline, CurrentFrame, FrameApplier, FrameEncoder};

#[derive(Default)]
pub struct DumpDiff;

impl FrameEncoder for DumpDiff {
    fn encode(&mut self, acked: Option<&Baseline>, cur: &CurrentFrame<'_>) -> FrameBody {
        // Verbatim from server.rs: diff against the acked dump when we hold it
        // and the diff is a net win, else a full dump. The diff-economics
        // sampling (stats.record_diff_frame / record_full_frame) stays in
        // server.rs where the Stats live; this returns only the chosen body.
        match acked {
            Some(base) => {
                let diff = frame::make_diff(&base.dump, cur.dump);
                if diff.len() + 8 < cur.dump.len() {
                    FrameBody::Diff {
                        base: base.num,
                        // The server fills the base_sum in when CAP_BASE_SUM is
                        // negotiated; the codec is oblivious to it (#94/RFC 0006).
                        base_sum: None,
                        diff,
                    }
                } else {
                    FrameBody::Full(cur.dump.to_vec())
                }
            }
            // No acked base to diff against — a forced full dump.
            None => FrameBody::Full(cur.dump.to_vec()),
        }
    }
}

impl FrameApplier for DumpDiff {
    fn apply(
        &mut self,
        rows: u16,
        cols: u16,
        applied_data: &[u8],
        server_term: &mut Terminal,
        body: &FrameBody,
    ) -> ApplyOutcome {
        // Verbatim from client.rs apply_frame: reconstruct the full dump bytes,
        // then rebuild the model from scratch by feeding them to a fresh
        // Terminal. The per-frame apply timing (record_apply_us) stays at the
        // client.rs call site so it can mirror the server's dump_vt_us.
        let bytes: Vec<u8> = match body {
            FrameBody::Empty => return ApplyOutcome::NoChange,
            FrameBody::Full(bytes) => bytes.clone(),
            FrameBody::Diff { base: _, diff, .. } => {
                // The caller has already confirmed base == applied_num before
                // dispatching here; reconstruct against the held dump.
                match frame::apply_diff(applied_data, diff) {
                    Some(bytes) => bytes,
                    None => return ApplyOutcome::ReackAndWait,
                }
            }
            // DumpDiff never receives Morph/Scrollback bodies; the caller routes
            // those to their own handlers.
            _ => return ApplyOutcome::ReackAndWait,
        };
        let mut term = Terminal::with_scrollback(rows, cols, 0);
        term.process(&bytes);
        // A DECCOLM replayed from the server dump resizes the model to 132/80
        // columns regardless of the real tty: clamp back so renders never paint
        // a wider image than the tty can show.
        if term.rows() != rows || term.cols() != cols {
            term.resize(rows, cols);
        }
        *server_term = term;
        ApplyOutcome::Advanced { dump: bytes }
    }
}
