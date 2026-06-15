//! Perf probe (debug-only, `#[ignore]`d): quantifies the two per-frame client
//! costs flagged as perf followups, so the optimization work is data-driven
//! rather than speculative (CLAUDE.md "verify before optimizing"):
//!
//!   * `apply_frame`'s full-dump re-parse (`Terminal::with_scrollback` +
//!     `process(dump_vt)`) — the suspected main gap vs mosh's incremental model.
//!   * `compose_frame`'s `Snapshot::from_term` (O(rows*cols) clone + per-cell
//!     hyperlink scan), run every render tick while predictions are live.
//!
//! Not a benchmark suite and not run in CI — `cargo test` skips `#[ignore]`d
//! tests. Run via `just debug-perf-compose` (release build; debug timings are
//! meaningless). Numbers are wall-clock per op on the dev box, for relative
//! comparison and order-of-magnitude — not absolute guarantees.

use std::hint::black_box;
use std::time::Instant;

use posh_term::Terminal;

use crate::remote::display::Snapshot;

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
