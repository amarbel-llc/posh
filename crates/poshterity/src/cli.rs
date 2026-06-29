//! The `poshterity` command-line surface (also reachable as `posh rec`).
//!
//! `replay` feeds a whole recording through the in-process emulator and dumps
//! the final screen; `step` advances by discrete emulator-defined steps (the
//! [`crate::player::Player`]) and dumps an intermediate screen. Timing is
//! recorded but never replayed as sleeps — the screen is a pure function of
//! the bytes.

use crate::castx::{EventCode, Reader};
use crate::golden::{self, GoldenKind};
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
  poshterity replay <file> [--to-marker NAME] [--dump text|vt|flat]
  poshterity replay <file> --raw [--size COLSxROWS] [--dump text|vt|flat]
  poshterity step <file> (--by byte|escape|write|change|frame|marker [--n N]
                        | --to-marker NAME) [--frame-gap SECS] [--dump ...]
  poshterity bless <file> --golden <path> [--at MARKER] [--kind grid|vt|flat]
  poshterity assert <file> --golden <path> [--at MARKER] [--kind ...]
                  [--check-emu-rev]

Replay a .castx / asciinema .cast v2 recording through the in-process
posh-term emulator. `replay` prints the final screen; `step` advances by
discrete steps; `bless` writes a golden-frame snapshot and `assert` checks
one (the CI gate). Default --dump text, --kind grid. Timing is never
replayed as sleeps.

With --raw, <file> is a bare terminal-output byte stream (no .castx header) —
e.g. a script(1) capture of a posh client's STDOUT. A raw stream carries no
dimensions, so pass --size COLSxROWS (default 80x24); EL/erase clear to that
width, so the size must match the captured terminal. --dump vt re-serializes
SGR so background runs are visible.";

/// Run the poshterity CLI over `args` — the arguments after the program name
/// (for the `poshterity` bin) or after the `rec` subcommand (for `posh rec`).
/// Returns a human-readable error string on failure.
pub fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("replay") => run_replay(&args[1..]),
        Some("step") => run_step(&args[1..]),
        Some("bless") => run_bless(&args[1..]),
        Some("assert") => run_assert(&args[1..]),
        Some("help" | "-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("version" | "-V" | "--version") => {
            // poshterity's own provenance (version + git sha), flowed by build.rs
            // (github #71). Distinct from the emulator's emu_rev stamped into
            // recordings. See eng-versioning(7).
            println!("poshterity {} ({})", env!("POSH_VERSION"), env!("POSH_GIT_SHA"));
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
    let mut raw = false;
    let mut size: Option<(u16, u16)> = None;
    let mut max_bytes: Option<usize> = None;
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
            "--raw" => {
                raw = true;
                i += 1;
            }
            "--size" => {
                let v = args.get(i + 1).ok_or("--size requires COLSxROWS")?;
                size = Some(parse_resize(v).ok_or("--size expects COLSxROWS, e.g. 120x40")?);
                i += 2;
            }
            "--bytes" => {
                max_bytes = Some(
                    args.get(i + 1)
                        .ok_or("--bytes requires a count")?
                        .parse()
                        .map_err(|_| "--bytes expects a non-negative integer")?,
                );
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

    // Raw mode: a bare byte stream (e.g. a script(1) capture of a client's
    // STDOUT), no .castx header and so no markers and no embedded dimensions.
    if raw {
        if to_marker.is_some() {
            return Err("--to-marker is not supported with --raw (a raw stream has no markers)".into());
        }
        let mut bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        // --bytes N replays only the first N bytes, to inspect a transient
        // mid-stream state (e.g. a bled frame before the session's exit/rmcup
        // wipes the alt screen). Bisect N to localize when an artifact appears.
        if let Some(n) = max_bytes {
            bytes.truncate(n);
        }
        let (cols, rows) = size.unwrap_or((80, 24));
        let out = replay_raw(&bytes, cols, rows, dump);
        return std::io::stdout().write_all(&out).map_err(|e| format!("write: {e}"));
    }
    if size.is_some() {
        return Err("--size only applies to --raw (a .castx carries its own dimensions)".into());
    }
    if max_bytes.is_some() {
        return Err("--bytes only applies to --raw".into());
    }

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
        "poshterity: byte {} · gen {} · marker {} · t {:.3}",
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

/// Shared flags for `bless` / `assert`.
struct GoldenArgs<'a> {
    file: &'a str,
    golden: &'a str,
    at: Option<&'a str>,
    kind: GoldenKind,
    check_emu_rev: bool,
}

fn parse_golden_args<'a>(args: &'a [String]) -> Result<GoldenArgs<'a>, String> {
    let mut file: Option<&str> = None;
    let mut golden: Option<&str> = None;
    let mut at: Option<&str> = None;
    let mut kind = GoldenKind::Grid;
    let mut check_emu_rev = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--golden" => {
                golden = Some(args.get(i + 1).ok_or("--golden requires a value")?);
                i += 2;
            }
            "--at" => {
                at = Some(args.get(i + 1).ok_or("--at requires a value")?);
                i += 2;
            }
            "--kind" => {
                kind = GoldenKind::parse(args.get(i + 1).ok_or("--kind requires a value")?)?;
                i += 2;
            }
            "--check-emu-rev" => {
                check_emu_rev = true;
                i += 1;
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
    Ok(GoldenArgs {
        file: file.ok_or_else(|| format!("a <file> is required\n\n{USAGE}"))?,
        golden: golden.ok_or_else(|| format!("--golden <path> is required\n\n{USAGE}"))?,
        at,
        kind,
        check_emu_rev,
    })
}

/// Build a Player and advance it to `--at MARKER` (or the end of the stream).
fn player_at(file: &str, at: Option<&str>) -> Result<Player, String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    let mut player = Player::from_source(&src)?;
    match at {
        Some(name) => {
            if !player.step_to_marker(name) {
                return Err(format!("no marker {name:?} in the recording"));
            }
        }
        None => player.step_to_end(),
    }
    Ok(player)
}

fn run_bless(args: &[String]) -> Result<(), String> {
    let a = parse_golden_args(args)?;
    let player = player_at(a.file, a.at)?;
    let emu = player.emu_rev().unwrap_or("unknown");
    let rendered = golden::render(player.terminal(), a.kind, emu);
    std::fs::write(a.golden, rendered).map_err(|e| format!("{}: {e}", a.golden))?;
    eprintln!("poshterity: blessed {} ({:?})", a.golden, a.kind);
    Ok(())
}

fn run_assert(args: &[String]) -> Result<(), String> {
    let a = parse_golden_args(args)?;
    let player = player_at(a.file, a.at)?;
    let emu = player.emu_rev().unwrap_or("unknown");
    let fresh = golden::render(player.terminal(), a.kind, emu);
    let stored = std::fs::read_to_string(a.golden).map_err(|e| format!("{}: {e}", a.golden))?;

    if a.check_emu_rev {
        if let Some(golden_emu) = golden::golden_emu_rev(&stored) {
            if golden_emu != emu {
                eprintln!(
                    "poshterity: warning: golden blessed under emu_rev {golden_emu:?}, \
                     recording is {emu:?} — regen may be due"
                );
            }
        }
    }

    if fresh == stored {
        Ok(())
    } else {
        eprint!(
            "poshterity: golden mismatch ({}):\n{}",
            a.golden,
            golden::diff(&stored, &fresh, player.terminal(), a.kind)
        );
        Err("golden assertion failed".to_string())
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

/// Replay a bare terminal-output byte stream (no .castx header) through the
/// emulator at a fixed size and dump the final screen. For inspecting a capture
/// taken with `script(1)` or any tool that records raw tty bytes — e.g. a posh
/// client's STDOUT (github #100). A raw stream carries no dimensions, so the
/// caller supplies them; EL/erase clear to `cols`, so a wrong width misplaces
/// trailing-cell clears. `--dump vt` re-serializes SGR so background runs show.
pub fn replay_raw(bytes: &[u8], cols: u16, rows: u16, dump: Dump) -> Vec<u8> {
    let mut replay = Replay::new(rows, cols);
    replay.feed(bytes);
    dump_terminal(replay.terminal(), dump)
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
    fn replay_raw_feeds_a_headerless_stream() {
        // No .castx header: raw bytes go straight to the emulator at the given
        // size. An EL under a background pen fills to the supplied width, which
        // is exactly what a raw capture needs to reproduce (github #100).
        let out = replay_raw(b"\x1b[2J\x1b[Hhello\r\nworld", 20, 3, Dump::Text);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("hello"), "{text:?}");
        assert!(text.contains("world"), "{text:?}");
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
