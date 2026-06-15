//! Perf probe (debug-only, `#[ignore]`d): quantifies the two per-frame client
//! costs flagged as perf followups, so the optimization work is data-driven
//! rather than speculative (CLAUDE.md "verify before optimizing"):
//!
//!   * `apply_frame`'s full-dump re-parse (`Terminal::with_scrollback` +
//!     `process(dump_vt)`) — today's DumpDiff apply, the suspected main gap vs
//!     mosh's incremental model — against MorphDelta's incremental apply
//!     (`process(escapes)` on the EXISTING model), the #15 optimization.
//!   * `compose_frame`'s `Snapshot::from_term` (O(rows*cols) clone + per-cell
//!     hyperlink scan), run every render tick while predictions are live.
//!
//! The MorphDelta↔DumpDiff round-trip *correctness* gate (the #15 linchpin)
//! lives in `framesync`'s tests (`morph_roundtrip_reproduces_state_over_a_table`),
//! not here — this file is timing only.
//!
//! Not a benchmark suite and not run in CI — `cargo test` skips `#[ignore]`d
//! tests. Run via `just debug-perf-compose` (release build; debug timings are
//! meaningless). Numbers are wall-clock per op on the dev box, for relative
//! comparison and order-of-magnitude — not absolute guarantees.

use std::hint::black_box;
use std::time::Instant;

use posh_term::Terminal;

use crate::remote::display::{self, Snapshot};

/// A full visible grid of mixed content: every cell printable, a per-row SGR
/// colour change so the dump_vt stream carries realistic escape runs. Scrollback
/// 0 mirrors `apply_frame`'s reparse target exactly.
fn build_screen(rows: u16, cols: u16) -> Terminal {
    let mut t = Terminal::with_scrollback(rows, cols, 0);
    for r in 0..rows {
        let fg = 31 + (r % 7) as u32; // rotate ANSI fg 31..37
        t.process(format!("\x1b[1;{fg}m").as_bytes());
        let body: String = (0..cols as usize)
            .map(|c| char::from(b'!' + (((c + r as usize) % 90) as u8)))
            .collect();
        t.process(body.as_bytes());
        t.process(b"\x1b[0m");
        if r + 1 < rows {
            t.process(b"\r\n");
        }
    }
    t
}

/// Mirror of `client::apply_frame`'s reparse (fresh terminal + process, clamp
/// DECCOLM back to tty size). Replicated here so the probe does not depend on
/// `apply_frame`'s private signature.
fn reparse(rows: u16, cols: u16, dump: &[u8]) -> Terminal {
    let mut t = Terminal::with_scrollback(rows, cols, 0);
    t.process(dump);
    if t.rows() != rows || t.cols() != cols {
        t.resize(rows, cols);
    }
    t
}

#[test]
#[ignore = "perf probe; run via `just debug-perf-compose` (--ignored --nocapture)"]
fn perf_reparse_and_from_term() {
    // Representative sizes: classic 24x80 and a wide modern terminal.
    for &(rows, cols) in &[(24u16, 80u16), (50, 212)] {
        let term = build_screen(rows, cols);
        let dump = term.dump_vt();
        let base = reparse(rows, cols, &dump);

        let iters = 2000u32;
        // Warm caches / branch predictors.
        for _ in 0..50 {
            black_box(reparse(rows, cols, &dump));
            black_box(Snapshot::from_term(&base));
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            black_box(reparse(rows, cols, &dump));
        }
        let reparse_us = t0.elapsed().as_nanos() as f64 / iters as f64 / 1000.0;

        let t1 = Instant::now();
        for _ in 0..iters {
            black_box(Snapshot::from_term(&base));
        }
        let from_term_us = t1.elapsed().as_nanos() as f64 / iters as f64 / 1000.0;

        eprintln!(
            "[perf] {rows}x{cols}  dump={dump}B  reparse(apply_frame)={reparse:.1}us  \
             from_term(compose)={from_term:.1}us  per-frame≈{total:.1}us",
            dump = dump.len(),
            reparse = reparse_us,
            from_term = from_term_us,
            total = reparse_us + from_term_us,
        );
    }
}

/// The #15 win, measured: MorphDelta's incremental apply (`process(escapes)` on
/// an existing model that is already at state a) vs DumpDiff's full-dump reparse
/// of state b, on the same workloads. The morph delta is a realistic per-frame
/// edit (one new line of output + a colour change + a cursor move), so this
/// times the steady-state typing/output frame, not a keyframe.
#[test]
#[ignore = "perf probe; run via `just debug-perf-compose` (--ignored --nocapture)"]
fn perf_morph_apply_vs_dumpdiff_reparse() {
    for &(rows, cols) in &[(24u16, 80u16), (50, 212)] {
        // State a: a full screen of content. State b: a one-row delta on top.
        let term_a = build_screen(rows, cols);
        let snap_a = Snapshot::from_term(&term_a);
        let mut term_b = build_screen(rows, cols);
        term_b.process(b"\x1b[1;1H\x1b[1;36mfresh line of output replacing row 0\x1b[0m");
        let snap_b = Snapshot::from_term(&term_b);
        let dump_b = term_b.dump_vt();

        // The forward and inverse escape-deltas the server would ship; applying
        // both returns the model to state a (MorphDelta encode, both directions),
        // so each iteration can repeat the a->b apply faithfully without Clone.
        let fwd = display::new_frame(true, &snap_a, &snap_b, false);
        let inv = display::new_frame(true, &snap_b, &snap_a, false);

        // A live model parked at state a, morphed forward and back each iter —
        // the client's standing server_term, mutated in place.
        let mut model = reparse(rows, cols, &term_a.dump_vt());

        let iters = 2000u32;
        for _ in 0..50 {
            black_box(reparse(rows, cols, &dump_b));
            model.process(black_box(&fwd));
            model.process(black_box(&inv));
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            black_box(reparse(rows, cols, &dump_b));
        }
        let dumpdiff_us = t0.elapsed().as_nanos() as f64 / iters as f64 / 1000.0;

        // Two morph applies (fwd + inv) per iteration, each just
        // `process(escapes)` on the existing model — MorphDelta's real per-frame
        // cost (no dump_vt refresh; #15). Halve for the per-delta figure.
        let t1 = Instant::now();
        for _ in 0..iters {
            model.process(black_box(&fwd));
            model.process(black_box(&inv));
        }
        let morph_us = t1.elapsed().as_nanos() as f64 / iters as f64 / 1000.0 / 2.0;

        eprintln!(
            "[perf] {rows}x{cols}  dump={dump}B  delta={delta}B  \
             dumpdiff_reparse={dd:.1}us  morph_apply={mu:.1}us  speedup≈{x:.1}x",
            dump = dump_b.len(),
            delta = fwd.len(),
            dd = dumpdiff_us,
            mu = morph_us,
            x = dumpdiff_us / morph_us.max(0.001),
        );
    }
}
