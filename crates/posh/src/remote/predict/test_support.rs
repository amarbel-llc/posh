//! Shared test harness for the prediction models. Drives a predictor through
//! the *real* client cycle (predict locally, server echoes, dump_vt round-trip,
//! cull, render) so the moved engine tests pin the same behaviors regardless
//! of which model is under test.

#![cfg(test)]

use posh_term::Terminal;

use crate::remote::display::Snapshot;

use super::{PredictionModel, Predictor, ReplaceRenderer};

/// Test-only accessors the harness needs beyond the `Predictor` trait
/// (confirmed_epoch / shown_cells are model-internal gauges).
pub trait TestPredictor: Predictor {
    fn confirmed_epoch(&self) -> u64;
    fn shown_cells(&self) -> u64;
}

impl TestPredictor for super::MoshPredictor {
    fn confirmed_epoch(&self) -> u64 {
        super::MoshPredictor::confirmed_epoch(self)
    }
    fn shown_cells(&self) -> u64 {
        self.shown_cells_count()
    }
}

impl TestPredictor for super::OptimisticPredictor {
    fn confirmed_epoch(&self) -> u64 {
        super::OptimisticPredictor::confirmed_epoch(self)
    }
    fn shown_cells(&self) -> u64 {
        self.shown_cells_count()
    }
}

/// Re-parse a `dump_vt()` byte stream into a Terminal, mirroring
/// client.rs::apply_frame (fresh terminal, clamp DECCOLM back to tty size).
pub fn reparse(rows: u16, cols: u16, dump: &[u8]) -> Terminal {
    let mut t = Terminal::with_scrollback(rows, cols, 0);
    t.process(dump);
    if t.rows() != rows || t.cols() != cols {
        t.resize(rows, cols);
    }
    t
}

pub struct PredictHarness {
    pub eng: Box<dyn TestPredictor>,
    pub server: Terminal,  // authoritative server screen
    pub display: Snapshot, // what the client currently shows (predictions applied)
    input_off: u64,        // reliable input stream offset (outbox.end_offset())
    echo_off: u64,         // server echo-ack offset
    now: u64,
    rows: u16,
    cols: u16,
}

impl PredictHarness {
    pub fn new(rows: u16, cols: u16, init: &[u8]) -> PredictHarness {
        Self::with_pref(rows, cols, init, PredictionModel::Adaptive)
    }

    pub fn with_pref(rows: u16, cols: u16, init: &[u8], pref: PredictionModel) -> PredictHarness {
        let mut server = Terminal::with_scrollback(rows, cols, 0);
        server.process(init);
        let server_term = reparse(rows, cols, &server.dump_vt());
        let display = Snapshot::from_term(&server_term);
        let eng: Box<dyn TestPredictor> = match pref {
            PredictionModel::Optimistic => Box::new(super::OptimisticPredictor::new(false)),
            other => Box::new(super::MoshPredictor::new(other, false)),
        };
        PredictHarness {
            eng,
            server,
            display,
            input_off: 0,
            echo_off: 0,
            now: 1000,
            rows,
            cols,
        }
    }

    /// Predict one keystroke locally, exactly as client.rs::process_user_input
    /// (set_frame_sent at the pre-push offset, then on_user_byte).
    pub fn type_byte(&mut self, b: u8) {
        self.eng.set_frame_sent(self.input_off);
        self.eng.on_user_byte(b, &self.display, self.now);
        self.input_off += 1;
        self.now += 5;
    }

    /// The shell echoes bytes onto the authoritative server screen; the
    /// echo-ack catches up to the input the echo consumed (past ECHO_TIMEOUT).
    pub fn server_echo(&mut self, b: &[u8]) {
        self.server.process(b);
        self.echo_off = self.input_off;
        self.now += 60;
    }

    /// Deliver a server frame: rebuild server_term from dump_vt, feed the
    /// acks, cull, and re-render the display — process_frame + compose_frame.
    pub fn deliver(&mut self) {
        let server_term = reparse(self.rows, self.cols, &self.server.dump_vt());
        // send_interval 50 > SRTT_TRIGGER_HIGH so the adaptive shown() is on.
        self.eng.on_server_frame(self.input_off, self.echo_off, 50);
        let base = Snapshot::from_term(&server_term);
        self.eng.cull(&base, self.now);
        let mut next = base.clone();
        self.eng.render(&mut next, &ReplaceRenderer);
        self.display = next;
        self.now += 5;
    }
}

pub fn shown_char(fb: &Snapshot, row: u16, col: u16) -> char {
    let c = fb.cell(row, col).unwrap();
    if c.ch == '\0' {
        ' '
    } else {
        c.ch
    }
}
