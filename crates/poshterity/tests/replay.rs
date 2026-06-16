//! Integration tests for poshterity phase 1: parse a `.castx`/`.cast` recording
//! and replay it to a deterministic screen through the public API.

use poshterity::castx::{write_event, write_header, Event, EventCode, Header, Reader};
use poshterity::cli::{replay_source, Dump};
use poshterity::Replay;

/// A recording built from the phase-0 fixture bytes round-trips through the
/// writer, reader, and replay to the known screen: "hello " + SGR-red "red",
/// then a second line.
#[test]
fn castx_fixture_replays_to_known_screen() {
    let header = Header {
        version: 2,
        width: 20,
        height: 5,
        poshterity: None,
    };
    let event = Event {
        time: 0.0,
        code: EventCode::Output,
        // ESC[31m red ESC[0m, CRLF, second line — same bytes as the phase-0
        // lib test, here carried through .castx JSON escaping and back.
        data: "hello \u{1b}[31mred\u{1b}[0m\r\nsecond line".to_string(),
    };
    let doc = format!("{}\n{}\n", write_header(&header), write_event(&event));

    let text = String::from_utf8(replay_source(&doc, Dump::Text).unwrap()).unwrap();
    assert!(text.contains("hello red"), "{text:?}");
    assert!(text.contains("second line"), "{text:?}");
}

/// A stock asciinema `.cast` v2 (no `poshterity` block) replays, and `i` (input)
/// events are recorded-but-ignored on replay.
#[test]
fn plain_cast_replays_and_ignores_input() {
    let doc = "{\"version\":2,\"width\":20,\"height\":3}\n\
               [0.1,\"o\",\"hi\"]\n\
               [0.2,\"i\",\"secret\"]\n";
    let text = String::from_utf8(replay_source(doc, Dump::Text).unwrap()).unwrap();
    assert!(text.contains("hi"), "{text:?}");
    assert!(!text.contains("secret"), "input must not reach the screen: {text:?}");
}

/// An unknown event letter is skipped (read-compat), not an error, and does
/// not perturb the screen.
#[test]
fn unknown_event_letter_is_ignored() {
    let doc = "{\"version\":2,\"width\":20,\"height\":3}\n\
               [0.1,\"o\",\"AB\"]\n\
               [0.2,\"z\",\"from the future\"]\n\
               [0.3,\"o\",\"C\"]\n";
    let text = String::from_utf8(replay_source(doc, Dump::Text).unwrap()).unwrap();
    assert!(text.starts_with("ABC"), "{text:?}");
}

/// A resize event changes the emulated dimensions, mapping asciinema's
/// `"COLSxROWS"` onto posh-term's rows-first `resize`.
#[test]
fn resize_event_maps_cols_then_rows() {
    let doc = "{\"version\":2,\"width\":80,\"height\":24}\n[0.1,\"r\",\"40x10\"]\n";
    let mut reader = Reader::new(doc);
    let header = reader.header().unwrap();
    let mut replay = Replay::new(header.height, header.width);
    assert_eq!((replay.terminal().cols(), replay.terminal().rows()), (80, 24));

    while let Some(ev) = reader.next_event() {
        let ev = ev.unwrap();
        if ev.code == EventCode::Resize {
            let (w, h) = ev.data.split_once('x').unwrap();
            // "40x10" = cols x rows; Replay::resize is rows-first.
            replay.resize(h.parse().unwrap(), w.parse().unwrap());
        }
    }
    assert_eq!((replay.terminal().cols(), replay.terminal().rows()), (40, 10));
}

/// `--dump vt` yields a raw VT byte stream (contains ESC), not printable text.
#[test]
fn dump_vt_emits_raw_escape_bytes() {
    let doc = "{\"version\":2,\"width\":20,\"height\":3}\n[0.0,\"o\",\"hi\"]\n";
    let out = replay_source(doc, Dump::Vt).unwrap();
    assert!(out.contains(&0x1b), "vt dump should carry ESC bytes");
}
