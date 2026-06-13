//! The `posh-rec` command-line surface (also reachable as `posh rec`).
//!
//! `replay` feeds a whole recording through the in-process emulator and dumps
//! the final screen; `step` advances by discrete emulator-defined steps (the
//! [`crate::player::Player`]) and dumps an intermediate screen. Timing is
//! recorded but never replayed as sleeps — the screen is a pure function of
//! the bytes.

use crate::castx::{EventCode, Reader};
use crate::player::{Granularity, Player};
use crate::Replay;
use std::io::Write;

/// How `replay --dump` renders the final screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dump {
    /// Plain text (`Terminal::dump_text`).
    Text,
    /// A full VT reconstruction stream (`Terminal::dump_vt`).
    Vt,
    /// A single-screen VT stream, active grid only (`Terminal::dump_vt_flat`).
    Flat,
}

impl Dump {
    fn parse(s: &str) -> Result<Dump, String> {
        match s {
            "text" => Ok(Dump::Text),
            "vt" => Ok(Dump::Vt),
            "flat" => Ok(Dump::Flat),
            other => Err(format!("--dump expects text|vt|flat, got {other:?}")),
        }
    }
}

const USAGE: &str = "\
usage:
  posh-rec replay <file> [--to-marker NAME] [--dump text|vt|flat]
  posh-rec step <file> (--by byte|escape|write|change|frame|marker [--n N]
                        | --to-marker NAME) [--frame-gap SECS] [--dump ...]

Replay a .castx / asciinema .cast v2 recording through the in-process
posh-term emulator. `replay` prints the final screen; `step` advances by
discrete steps and prints the intermediate screen (the step position goes
to stderr). Default --dump text. Timing is never replayed as sleeps.";

/// Run the posh-rec CLI over `args` — the arguments after the program name
/// (for the `posh-rec` bin) or after the `rec` subcommand (for `posh rec`).
/// Returns a human-readable error string on failure.
pub fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("replay") => run_replay(&args[1..]),
        Some("step") => run_step(&args[1..]),
        Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("version" | "-V" | "--version") => {
            println!("posh-rec {}", posh_term::version());
            Ok(())
        }
        Some(other) => Err(format!("unknown subcommand {other:?}\n\n{USAGE}")),
        None => Err(format!("missing subcommand\n\n{USAGE}")),
    }
}

fn run_replay(args: &[String]) -> Result<(), String> {
    let mut file: Option<&str> = None;
    let mut dump = Dump::Text;
    let mut to_marker: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dump" => {
                dump = Dump::parse(args.get(i + 1).ok_or("--dump requires a value")?)?;
                i += 2;
            }
            "--to-marker" => {
                to_marker = Some(args.get(i + 1).ok_or("--to-marker requires a value")?);
                i += 2;
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown flag {flag:?}\n\n{USAGE}"));
            }
            positional => {
                if file.is_some() {
                    return Err(format!("unexpected extra argument {positional:?}"));
                }
                file = Some(positional);
                i += 1;
            }
        }
    }
    let path = file.ok_or_else(|| format!("replay requires a <file>\n\n{USAGE}"))?;
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let out = match to_marker {
        // Stop at a named marker instead of feeding the whole stream.
        Some(name) => {
            let mut player = Player::from_source(&src)?;
            if !player.step_to_marker(name) {
                return Err(format!("no marker {name:?} in the recording"));
            }
            dump_terminal(player.terminal(), dump)
        }
        None => replay_source(&src, dump)?,
    };
    std::io::stdout()
        .write_all(&out)
        .map_err(|e| format!("write: {e}"))
}

fn run_step(args: &[String]) -> Result<(), String> {
    let mut file: Option<&str> = None;
    let mut by: Option<Granularity> = None;
    let mut n: usize = 1;
    let mut to_marker: Option<&str> = None;
    let mut frame_gap: Option<f64> = None;
    let mut dump = Dump::Text;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--by" => {
                by = Some(Granularity::parse(args.get(i + 1).ok_or("--by requires a value")?)?);
                i += 2;
            }
            "--n" => {
                n = args
                    .get(i + 1)
                    .ok_or("--n requires a value")?
                    .parse()
                    .map_err(|_| "--n expects a non-negative integer")?;
                i += 2;
            }
            "--to-marker" => {
                to_marker = Some(args.get(i + 1).ok_or("--to-marker requires a value")?);
                i += 2;
            }
            "--frame-gap" => {
                frame_gap = Some(
                    args.get(i + 1)
                        .ok_or("--frame-gap requires a value")?
                        .parse()
                        .map_err(|_| "--frame-gap expects seconds (a number)")?,
                );
                i += 2;
            }
            "--dump" => {
                dump = Dump::parse(args.get(i + 1).ok_or("--dump requires a value")?)?;
                i += 2;
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown flag {flag:?}\n\n{USAGE}"));
            }
            positional => {
                if file.is_some() {
                    return Err(format!("unexpected extra argument {positional:?}"));
                }
                file = Some(positional);
                i += 1;
            }
        }
    }
    let path = file.ok_or_else(|| format!("step requires a <file>\n\n{USAGE}"))?;
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut player = Player::from_source(&src)?;
    if let Some(g) = frame_gap {
        player = player.with_frame_gap(g);
    }

    if let Some(name) = to_marker {
        if !player.step_to_marker(name) {
            return Err(format!("no marker {name:?} ahead of the start"));
        }
    } else {
        let g = by.ok_or("step requires --by <granularity> or --to-marker <name>")?;
        player.step(g, n);
    }

    // Report where the step landed on stderr, leaving stdout for the dump.
    let pos = player.position();
    eprintln!(
        "posh-rec: byte {} · gen {} · marker {} · t {:.3}",
        pos.byte_offset,
        pos.generation,
        pos.marker.as_deref().unwrap_or("-"),
        pos.time
    );
    let out = dump_terminal(player.terminal(), dump);
    std::io::stdout()
        .write_all(&out)
        .map_err(|e| format!("write: {e}"))
}

/// Render a terminal per the chosen dump mode.
fn dump_terminal(term: &posh_term::Terminal, dump: Dump) -> Vec<u8> {
    match dump {
        Dump::Text => term.dump_text().into_bytes(),
        Dump::Vt => term.dump_vt(),
        Dump::Flat => term.dump_vt_flat(),
    }
}

/// Replay an entire recording's text and render the final screen. Filesystem-
/// free so tests drive it directly. `o` output is fed to the emulator, `r`
/// resizes are honored, and `i`/`m`/unknown events are ignored in phase 1.
pub fn replay_source(src: &str, dump: Dump) -> Result<Vec<u8>, String> {
    let mut reader = Reader::new(src);
    let header = reader.header()?;
    let mut replay = Replay::new(header.height, header.width);
    while let Some(ev) = reader.next_event() {
        let ev = ev?;
        match ev.code {
            EventCode::Output => replay.feed(ev.data.as_bytes()),
            EventCode::Resize => {
                if let Some((cols, rows)) = parse_resize(&ev.data) {
                    // asciinema "WxH" is COLSxROWS; Replay::resize is rows-first.
                    replay.resize(rows, cols);
                }
            }
            EventCode::Input | EventCode::Marker | EventCode::Unknown(_) => {}
        }
    }
    Ok(dump_terminal(replay.terminal(), dump))
}

/// Parse an asciinema resize payload `"COLSxROWS"` into `(cols, rows)`.
fn parse_resize(data: &str) -> Option<(u16, u16)> {
    let (w, h) = data.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_parse_accepts_known_rejects_unknown() {
        assert_eq!(Dump::parse("text"), Ok(Dump::Text));
        assert_eq!(Dump::parse("vt"), Ok(Dump::Vt));
        assert_eq!(Dump::parse("flat"), Ok(Dump::Flat));
        assert!(Dump::parse("grid").is_err());
    }

    #[test]
    fn parse_resize_reads_cols_x_rows() {
        assert_eq!(parse_resize("100x40"), Some((100, 40)));
        assert_eq!(parse_resize("80x24"), Some((80, 24)));
        assert_eq!(parse_resize("nope"), None);
        assert_eq!(parse_resize("80x"), None);
    }

    #[test]
    fn replay_source_renders_a_known_screen() {
        let src = "{\"version\":2,\"width\":20,\"height\":3}\n\
                   [0.0,\"o\",\"hello\"]\n";
        let out = replay_source(src, Dump::Text).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("hello"), "{text:?}");
    }
}
