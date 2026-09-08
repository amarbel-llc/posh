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

/// The FDR 0006 local-echo hot path, per predictor: what ONE keystroke costs
/// from `on_user_byte` to the escape stream the tty would get, at steady-state
/// typing on a fast link (the server echoes and acks each byte before the next
/// key, a new prompt line every 60 keys). The phases mirror the live
/// `time-to-paint` gauge's split — `predict` (`on_user_byte`), `compose`
/// (`Snapshot::from_term` + `cull` + the overlay render), and the `new_frame`
/// diff — minus the tty write, which only the live gauge can see. `never`
/// is the floor: the compose + diff every model pays whether or not it
/// predicts; a model's `predict` figure is its own cost above that.
#[test]
#[ignore = "perf probe; run via `just debug-perf-echo` (--ignored --nocapture)"]
fn perf_echo_time_to_paint() {
    use crate::remote::predict::{self, PredictionModel, RenderStyle};
    const MODELS: [PredictionModel; 7] = [
        PredictionModel::Never,
        PredictionModel::Always,
        PredictionModel::Adaptive,
        PredictionModel::Experimental,
        PredictionModel::Optimistic,
        PredictionModel::Controller,
        PredictionModel::FromScratch,
    ];
    const WARMUP: u32 = 50;
    const ITERS: u32 = 2000;
    let text = b"echo hello world ";
    for &(rows, cols) in &[(24u16, 80u16), (50, 212)] {
        for model in MODELS {
            let (mut predictor, renderer) = predict::build(model, RenderStyle::Replace, false);
            let mut term = build_screen(rows, cols);
            // A prompt on the last row, cursor after it: where typing lands.
            term.process(format!("\x1b[{rows};1H\x1b[2K$ ").as_bytes());
            let mut last_drawn = Snapshot::from_term(&term);
            let (mut predict_ns, mut compose_ns, mut diff_ns) = (0u128, 0u128, 0u128);
            let mut paint_bytes = 0usize;
            let mut offset = 0u64;
            let mut now = 0u64;
            for i in 0..(WARMUP + ITERS) {
                let b = text[i as usize % text.len()];
                now += 30;
                offset += 1;
                let t0 = Instant::now();
                predictor.set_frame_sent(offset);
                predictor.on_user_byte(b, &last_drawn, now);
                let t1 = Instant::now();
                let base = Snapshot::from_term(&term);
                predictor.cull(&base, now);
                let mut next = base;
                black_box(predictor.render(&mut next, &*renderer));
                let t2 = Instant::now();
                let bytes = display::new_frame_opt(true, &last_drawn, &next, false, false, true);
                let t3 = Instant::now();
                last_drawn = next;
                if i >= WARMUP {
                    predict_ns += (t1 - t0).as_nanos();
                    compose_ns += (t2 - t1).as_nanos();
                    diff_ns += (t3 - t2).as_nanos();
                    paint_bytes += bytes.len();
                }
                // The server echoes the byte and acks before the next key; a
                // fresh prompt line keeps the typing on-screen.
                term.process(&[b]);
                if i % 60 == 59 {
                    term.process(b"\r\n$ ");
                }
                predictor.on_server_frame(offset, offset, 30);
            }
            let per = |ns: u128| ns as f64 / ITERS as f64 / 1000.0;
            eprintln!(
                "[perf] {rows}x{cols}  model={:<12} predict={:.1}us  compose={:.1}us  \
                 diff={:.1}us  per-key≈{:.1}us  paint≈{}B",
                model.name(),
                per(predict_ns),
                per(compose_ns),
                per(diff_ns),
                per(predict_ns + compose_ns + diff_ns),
                paint_bytes / ITERS as usize,
            );
        }
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
