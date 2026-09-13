//! Visual prototype of look-alike local echo (FDR 0006): a fake shell line
//! where every character you type is shown IMMEDIATELY as a look-alike
//! glyph that changes at random every period (`posh_proto::lookalike`,
//! case swaps and symbols included) and snaps to the real
//! character once a simulated server round trip confirms it. No posh, no
//! network — just the terminal, so the rendering can be judged by eye.
//!
//! Run: `just debug-lookalike-echo [rtt_ms] [period_ms]` (defaults 800 / 150).
//! Keys: type freely; Backspace deletes — the erased cell shows the "tofu"
//! (glyph-not-found) box until a simulated round trip confirms the deletion,
//! then it vanishes (the erase analogue of the type shimmer, matching the
//! client's `LookalikeRenderer`); Enter confirms the line into the history;
//! Tab toggles the underline on unconfirmed cells; Ctrl-C, Ctrl-D, or Esc
//! quits. Raw mode is entered via `stty` (no libc here) and restored on exit.

use std::io::{Read, Write};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use posh_proto::lookalike::{cell_seed, next_lookalike};

/// The glyph a pending deletion shows: the "tofu" box a terminal draws for a
/// glyph its font cannot render. Mirrors the client `LookalikeRenderer`'s
/// `DELETED_GLYPH` (U+25AF).
const TOFU: char = '\u{25af}'; // ▯ WHITE VERTICAL RECTANGLE

struct Pending {
    ch: char,
    confirm_at: Instant,
    /// A pending DELETION (backspace) rather than an insert: it shows the tofu
    /// box until `confirm_at`, then the cell is dropped.
    erasing: bool,
    /// The look-alike on screen, and the tick it was picked at — re-picked
    /// (never the same glyph) on each tick.
    shown: Option<(char, u64)>,
}

fn stty(args: &[&str]) -> Option<String> {
    let out = Command::new("stty")
        .args(args)
        .stdin(std::fs::File::open("/dev/tty").ok()?)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rtt = Duration::from_millis(args.next().and_then(|a| a.parse().ok()).unwrap_or(800));
    let period = Duration::from_millis(args.next().and_then(|a| a.parse().ok()).unwrap_or(150));

    let Some(saved) = stty(&["-g"]) else {
        eprintln!("lookalike-echo: needs a terminal (stty -g failed)");
        std::process::exit(1);
    };
    if stty(&["raw", "-echo"]).is_none() {
        eprintln!("lookalike-echo: could not enter raw mode");
        std::process::exit(1);
    }

    // Keys arrive on a channel so the main loop can wake on the rotation
    // period without a poll(2) dependency.
    let (tx, rx) = mpsc::channel::<u8>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut b = [0u8; 1];
        while stdin.read(&mut b).map(|n| n > 0).unwrap_or(false) {
            if tx.send(b[0]).is_err() {
                break;
            }
        }
    });

    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "\r\n\x1b[1mlook-alike local echo\x1b[0m  rtt={}ms  rotate every {}ms\r\n\
         \x1b[2mtype; Backspace; Enter confirms the line; Tab toggles underline; Ctrl-C / Esc quits\x1b[0m\r\n\r\n",
        rtt.as_millis(),
        period.as_millis()
    );
    let started = Instant::now();
    let mut line: Vec<Pending> = Vec::new();
    let mut underline = false;
    let mut last_drawn = String::new();

    loop {
        let now = Instant::now();
        let tick = (now - started).as_millis() as u64 / period.as_millis().max(1) as u64;
        let mut frame = String::from("$ ");
        for (cell, p) in line.iter_mut().enumerate() {
            if p.erasing {
                // Pending deletion: the tofu box on a red background lingers
                // until the round trip confirms the erase; once confirmed it
                // renders nothing and is pruned below.
                if now < p.confirm_at {
                    frame.push_str("\x1b[48;2;95;0;0m"); // dark red background
                    if underline {
                        frame.push_str("\x1b[4m");
                    }
                    frame.push(TOFU);
                    if underline {
                        frame.push_str("\x1b[24m");
                    }
                    frame.push_str("\x1b[49m"); // reset background
                }
            } else if now >= p.confirm_at {
                frame.push(p.ch);
            } else {
                // Pending added glyph: the shimmering look-alike on a green
                // background until the round trip confirms it.
                frame.push_str("\x1b[48;2;0;95;0m"); // dark green background
                if underline {
                    frame.push_str("\x1b[4m");
                }
                let glyph = match p.shown {
                    Some((g, at)) if at == tick => g,
                    prev => next_lookalike(p.ch, prev.map(|(g, _)| g), cell_seed(tick, cell as u64)),
                };
                p.shown = Some((glyph, tick));
                frame.push(glyph);
                if underline {
                    frame.push_str("\x1b[24m");
                }
                frame.push_str("\x1b[49m"); // reset background
            }
        }
        // Drop deletions the round trip has now confirmed: their tofu has
        // served its time and the cell is gone.
        line.retain(|p| !(p.erasing && now >= p.confirm_at));
        if frame != last_drawn {
            let _ = write!(out, "\r\x1b[2K{frame}");
            let _ = out.flush();
            last_drawn = frame;
        }

        // Wake at the next rotation step, or for a key.
        let wait = period - Duration::from_millis(((now - started).as_millis() as u64) % period.as_millis().max(1) as u64);
        match rx.recv_timeout(wait) {
            Ok(b) => match b {
                0x03 | 0x04 | 0x1b => break,
                b'\t' => underline = !underline,
                b'\r' | b'\n' => {
                    let text: String = line.iter().map(|p| p.ch).collect();
                    let _ = write!(out, "\r\x1b[2K$ {text}\r\n");
                    line.clear();
                    last_drawn.clear();
                }
                0x7f | 0x08 => {
                    // Mark the last still-present cell as a pending erase: it
                    // shows the tofu box until the round trip confirms the
                    // deletion. Repeated backspaces walk leftward.
                    if let Some(p) = line.iter_mut().rev().find(|p| !p.erasing) {
                        p.erasing = true;
                        p.confirm_at = Instant::now() + rtt;
                        p.shown = None;
                    }
                }
                b if b.is_ascii_graphic() || b == b' ' => line.push(Pending {
                    ch: b as char,
                    confirm_at: Instant::now() + rtt,
                    erasing: false,
                    shown: None,
                }),
                _ => {}
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = write!(out, "\r\n");
    let _ = out.flush();
    stty(&[&saved]);
}
