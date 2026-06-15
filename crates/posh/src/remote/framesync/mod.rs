//! Swappable frame-sync codecs (#15): the encode-vs-apply logic for a visible
//! server frame lives behind a selectable seam rather than scattered across
//! `server.rs`/`client.rs` `match FrameBody` arms.
//!
//! A codec owns both halves of the visible-frame wire:
//!   * server-side **encode** — given the client-acked baseline and the
//!     current screen state, produce a [`FrameBody`];
//!   * client-side **apply** — given a received body, update the client's model.
//!
//! Two impls ship today:
//!   * [`DumpDiff`] — today's behavior verbatim (full `dump_vt` + prefix/suffix
//!     byte-diff on the server; `apply_diff` + a fresh-`Terminal` reparse on the
//!     client). The default and the keyframe path; no behavior change.
//!   * [`MorphDelta`] — the prototype. The server emits a minimal forward
//!     escape-delta (`display::new_frame`) as [`FrameBody::Morph`]; the client
//!     applies it with `server_term.process(&escapes)` on its **existing**
//!     model — no fresh `Terminal`, no full reparse. A `Full` keyframe is the
//!     fallback (no baseline, alt-screen toggle, resize).
//!
//! A third codec, **`CellDelta`** (structured Snapshot ops; the client model
//! becomes a persistent `Snapshot`, applied by assignment with no escape
//! parsing), is the documented back-pocket alternate. The seam is intentionally
//! shaped so it slots in as a drop-in third impl: the encoder sees the acked
//! baseline + the current state, the applier owns the client model — neither
//! trait hard-codes "escapes" or "dump bytes", so a Snapshot-op body fits the
//! same shape.

use posh_term::Terminal;

use crate::remote::display::Snapshot;
use crate::remote::sync::FrameBody;

mod dumpdiff;
mod morphdelta;

pub use dumpdiff::DumpDiff;
pub use morphdelta::MorphDelta;

/// Client-side codec selection (`POSH_FRAMESYNC`): which frame-sync codec the
/// client uses and, equivalently, whether it advertises `CAP_MORPH`. Defaults
/// to [`FrameSync::DumpDiff`] for any unset/empty/unrecognized value, so a
/// default session negotiates nothing new and the byte stream is unchanged
/// (#15 default-off gating).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSync {
    DumpDiff,
    Morph,
}

impl FrameSync {
    /// Parses `$POSH_FRAMESYNC`: `morph` opts into [`MorphDelta`]; anything
    /// else (including unset/empty) stays on [`DumpDiff`]. Lenient by design —
    /// the new path is opt-in and a typo must never silently change the wire.
    pub fn parse(value: Option<&str>) -> FrameSync {
        match value {
            Some("morph") => FrameSync::Morph,
            _ => FrameSync::DumpDiff,
        }
    }

    /// Whether the client advertises `CAP_MORPH` to the server. Only the Morph
    /// codec does; the server then selects [`MorphDelta`] as its encoder.
    pub fn advertises_morph(self) -> bool {
        matches!(self, FrameSync::Morph)
    }

    /// The client-side applier for this selection. Both impls handle
    /// `Full`/`Diff` identically; the Morph applier additionally applies a
    /// `Morph` body to the existing model.
    pub fn applier(self) -> Box<dyn FrameApplier> {
        match self {
            FrameSync::DumpDiff => Box::new(DumpDiff),
            FrameSync::Morph => Box::new(MorphDelta::default()),
        }
    }
}

/// The client-acked baseline a server-side encoder may build an incremental
/// body against. `num` is the acked frame number (the body's `base`); `dump`
/// is that frame's `dump_vt` bytes (the byte-diff base); `snapshot` is the
/// rendered screen state at that frame (the morph base). `alt_screen`/`rows`/
/// `cols` capture the parts of the terminal state that `Snapshot` does **not**
/// carry, so an encoder can detect a transition a morph cannot express and
/// fall back to a keyframe.
pub struct Baseline {
    pub num: u64,
    pub dump: Vec<u8>,
    pub snapshot: Snapshot,
    pub alt_screen: bool,
    pub rows: u16,
    pub cols: u16,
}

/// The current server screen state an encoder turns into a frame body, paired
/// with the same off-Snapshot fields the keyframe rule reads.
pub struct CurrentFrame<'a> {
    pub dump: &'a [u8],
    pub snapshot: &'a Snapshot,
    pub alt_screen: bool,
    pub rows: u16,
    pub cols: u16,
}

/// Server side: produce the body for a freshly produced visible frame, given
/// the client-acked baseline (None when the server no longer holds the acked
/// frame's state — first frame, post-loss). The returned body is anchored to
/// `acked.num` when incremental, or self-contained (`Full`) as a keyframe.
pub trait FrameEncoder {
    fn encode(&mut self, acked: Option<&Baseline>, cur: &CurrentFrame<'_>) -> FrameBody;
}

/// What applying a body did to the client model, so the caller can keep its
/// `applied_num`/`applied_data` bookkeeping in step.
pub enum ApplyOutcome {
    /// The body advanced the client to `frame_num`. `dump` is the refreshed
    /// `dump_vt` of the updated model — the valid byte-diff base for a later
    /// `Diff` anchored at this frame. The caller stores both. (The keyframe
    /// path: `Full`/`Diff` apply.)
    Advanced { dump: Vec<u8> },
    /// The body advanced the client to `frame_num` WITHOUT refreshing the
    /// byte-diff base (`applied_data`). MorphDelta returns this: a `Morph`
    /// anchors on the frame number plus the server's own acked snapshot, never
    /// the client's dump bytes, and a MorphDelta session emits no `Diff` body —
    /// so re-`dump_vt`ing the whole model every frame would reintroduce the
    /// O(whole-screen) cost #15 exists to remove. `applied_data` harmlessly
    /// stays at the last `Full` keyframe's dump (nothing reads it here).
    AdvancedNoDump,
    /// The body could not be applied at the client's current state (base
    /// mismatch, undecodable). The caller re-acks its newer state and waits
    /// for the server to fall back to a `Full` keyframe — exactly today's
    /// `Diff`-base-mismatch handling.
    ReackAndWait,
    /// An `Empty` body: no visible change, nothing to apply, no re-ack needed.
    NoChange,
}

/// Client side: apply a received visible-frame body to the client's model.
/// Implementations own how a body maps onto the model (reparse vs morph vs a
/// future Snapshot assignment); the caller threads the existing `applied_data`
/// (the last dump base) and `server_term` (the live model) through so an impl
/// can read or rebuild whichever it needs.
pub trait FrameApplier {
    fn apply(
        &mut self,
        rows: u16,
        cols: u16,
        applied_data: &[u8],
        server_term: &mut Terminal,
        body: &FrameBody,
    ) -> ApplyOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::sync::FrameBody;
    use morphdelta::{baseline_from, morph_expressible};
    use posh_term::Terminal;

    const ROWS: u16 = 24;
    const COLS: u16 = 80;

    /// A terminal at `state` reached by feeding `prologue` then `delta`, both
    /// from a fresh model at the standard size.
    fn term_after(prologue: &[u8], delta: &[u8]) -> Terminal {
        let mut t = Terminal::with_scrollback(ROWS, COLS, 0);
        t.process(prologue);
        t.process(delta);
        t
    }

    /// Reparse of a dump into a fresh model — exactly how the client arrives at
    /// a `Full` keyframe's state (DumpDiff apply of a Full).
    fn reparse(dump: &[u8]) -> Terminal {
        let mut t = Terminal::with_scrollback(ROWS, COLS, 0);
        t.process(dump);
        if t.rows() != ROWS || t.cols() != COLS {
            t.resize(ROWS, COLS);
        }
        t
    }

    /// Lists the Snapshot fields that differ between two snapshots, with a
    /// per-cell count for `cells` (so a failure says how badly it diverged, like
    /// the spike). Empty == byte-identical render state.
    fn diverging_fields(a: &Snapshot, b: &Snapshot) -> Vec<String> {
        let mut bad: Vec<String> = Vec::new();
        if a.cells != b.cells {
            let n = (0..b.rows.min(a.rows))
                .flat_map(|r| (0..b.cols.min(a.cols)).map(move |c| (r, c)))
                .filter(|&(r, c)| a.cell(r, c) != b.cell(r, c))
                .count();
            bad.push(format!("cells({n} diff)"));
        }
        macro_rules! check {
            ($field:ident) => {
                if a.$field != b.$field {
                    bad.push(stringify!($field).to_string());
                }
            };
        }
        check!(rows);
        check!(cols);
        check!(wrapped);
        check!(cursor_row);
        check!(cursor_col);
        check!(cursor_visible);
        check!(title);
        check!(reverse_video);
        check!(bracketed_paste);
        check!(focus_reporting);
        check!(alternate_scroll);
        check!(app_cursor_keys);
        check!(app_keypad);
        check!(mouse_mode);
        check!(mouse_encoding);
        check!(hyperlinks);
        check!(bell_count);
        check!(clipboard_seq);
        bad
    }

    /// THE linchpin (#15): for a morph-expressible state-pair a→b, the MorphDelta
    /// codec end-to-end (server encode → client apply onto the EXISTING model
    /// reparsed at a) must reproduce b's render state exactly. Runs a table of
    /// representative transitions and reports every diverging field.
    #[test]
    fn morph_roundtrip_reproduces_state_over_a_table() {
        // A shared prologue every pair starts from (a Full keyframe's worth):
        // a cleared screen with a couple of prompt lines.
        let prologue: &[u8] = b"\x1b[2J\x1b[H\x1b[1;32muser@host\x1b[0m:~$ ls -la\r\n\
                                total 42\r\ndrwxr-xr-x 3 user user 4096 dir\r\n";

        // (name, delta-from-prologue applied to reach state b). State a is the
        // prologue alone in every case; the morph carries a→b.
        let cases: &[(&str, &[u8])] = &[
            ("plain text edit", b"a longer line of brand new output here\r\n"),
            ("colour / SGR change", b"\x1b[1;34;41mDIR\x1b[0m normal again\r\n"),
            ("cursor move", b"\x1b[6;12Hx"),
            (
                "mode toggles (bracketed paste, mouse, app-cursor)",
                b"\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1h\x1b=",
            ),
            ("wide + combining chars", "世界 e\u{0301}a\u{0300}\r\n".as_bytes()),
            (
                "hyperlink (OSC 8)",
                b"\x1b]8;;https://example.com/path\x1b\\link text\x1b]8;;\x1b\\ after\r\n",
            ),
            // Enough newlines to push the prologue lines off the top of the
            // 24-row screen (scrolling within the visible grid).
            (
                "scroll pushes lines off the top",
                b"L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7\r\nL8\r\nL9\r\nL10\r\nL11\r\nL12\r\n\
                  L13\r\nL14\r\nL15\r\nL16\r\nL17\r\nL18\r\nL19\r\nL20\r\nL21\r\nL22\r\nL23\r\nL24\r\n",
            ),
        ];

        for (name, delta) in cases {
            let term_a = term_after(prologue, b"");
            let term_b = term_after(prologue, delta);
            let snap_b = Snapshot::from_term(&term_b);

            // Server: encode the a→b transition with MorphDelta.
            let baseline = baseline_from(1, &term_a);
            let cur = CurrentFrame {
                dump: &term_b.dump_vt(),
                snapshot: &snap_b,
                alt_screen: term_b.is_alt_screen(),
                rows: term_b.rows(),
                cols: term_b.cols(),
            };
            let mut enc = MorphDelta::default();
            let body = enc.encode(Some(&baseline), &cur);
            let escapes_len = match &body {
                FrameBody::Morph { escapes, .. } => escapes.len(),
                other => panic!("[{name}] expected Morph for an expressible pair, got {other:?}"),
            };

            // Client: arrive at a via a Full keyframe (reparse), then apply the
            // morph onto that SAME model — MorphDelta's apply.
            let mut client = reparse(&term_a.dump_vt());
            let mut applier = MorphDelta::default();
            let outcome = applier.apply(ROWS, COLS, &term_a.dump_vt(), &mut client, &body);
            assert!(
                matches!(outcome, ApplyOutcome::AdvancedNoDump),
                "[{name}] morph apply should advance the model in place (no dump refresh)"
            );

            let snap_c = Snapshot::from_term(&client);
            let bad = diverging_fields(&snap_c, &snap_b);
            assert!(
                bad.is_empty(),
                "[{name}] morph delta ({escapes_len}B) diverged from state b: {bad:?}"
            );

            // The morphed model must also serialize back to state b, so a later
            // `Full` keyframe (or attach replay) of it stays faithful: reparse
            // its `dump_vt` and re-check. (The `dump_vt` here is the test's own
            // check — MorphDelta no longer pays it per frame.)
            let snap_redump = Snapshot::from_term(&reparse(&client.dump_vt()));
            let bad = diverging_fields(&snap_redump, &snap_b);
            assert!(
                bad.is_empty(),
                "[{name}] morphed model's dump_vt diverged from state b: {bad:?}"
            );
        }
    }

    /// The keyframe rule (#15): the encoder MUST choose `Full`, not `Morph`, for
    /// a transition a Snapshot cannot express — alt-screen enter, alt-screen
    /// exit, and resize. (The other keyframe trigger, no baseline, is covered by
    /// `encoder_chooses_full_with_no_baseline`.)
    #[test]
    fn encoder_chooses_full_for_alt_screen_and_resize() {
        let prologue: &[u8] = b"\x1b[2J\x1b[Hprompt$ ";

        // Helper: encode the transition from term_a to term_b and report whether
        // the encoder produced a Full keyframe.
        let chose_full = |term_a: &Terminal, term_b: &Terminal| -> bool {
            let baseline = baseline_from(1, term_a);
            let snap_b = Snapshot::from_term(term_b);
            let cur = CurrentFrame {
                dump: &term_b.dump_vt(),
                snapshot: &snap_b,
                alt_screen: term_b.is_alt_screen(),
                rows: term_b.rows(),
                cols: term_b.cols(),
            };
            let mut enc = MorphDelta::default();
            matches!(enc.encode(Some(&baseline), &cur), FrameBody::Full(_))
        };

        // Alt-screen ENTER: a (primary) -> b (alt screen).
        let mut a = Terminal::with_scrollback(ROWS, COLS, 0);
        a.process(prologue);
        let mut b = Terminal::with_scrollback(ROWS, COLS, 0);
        b.process(prologue);
        b.process(b"\x1b[?1049h\x1b[2Jin the editor");
        assert!(!a.is_alt_screen() && b.is_alt_screen());
        assert!(chose_full(&a, &b), "alt-screen enter must be a Full keyframe");
        assert!(
            !morph_expressible(&baseline_from(1, &a), &CurrentFrame {
                dump: &b.dump_vt(),
                snapshot: &Snapshot::from_term(&b),
                alt_screen: b.is_alt_screen(),
                rows: b.rows(),
                cols: b.cols(),
            }),
            "alt-screen toggle is not morph-expressible"
        );

        // Alt-screen EXIT: a (alt) -> b (primary).
        assert!(chose_full(&b, &a), "alt-screen exit must be a Full keyframe");

        // RESIZE: a (24x80) -> b (24x100).
        let mut wide = Terminal::with_scrollback(ROWS, 100, 0);
        wide.process(prologue);
        assert!(chose_full(&a, &wide), "resize must be a Full keyframe");
    }

    /// With no acked baseline (first frame / post-loss) the encoder must produce
    /// a self-contained `Full` — there is nothing to morph against.
    #[test]
    fn encoder_chooses_full_with_no_baseline() {
        let mut term = Terminal::with_scrollback(ROWS, COLS, 0);
        term.process(b"\x1b[2J\x1b[Hhello");
        let snap = Snapshot::from_term(&term);
        let cur = CurrentFrame {
            dump: &term.dump_vt(),
            snapshot: &snap,
            alt_screen: term.is_alt_screen(),
            rows: term.rows(),
            cols: term.cols(),
        };
        let mut enc = MorphDelta::default();
        assert!(matches!(enc.encode(None, &cur), FrameBody::Full(_)));
    }

    /// FrameSync env parsing: only `morph` opts in; everything else stays on
    /// DumpDiff (default-off gating).
    #[test]
    fn framesync_env_parse_defaults_off() {
        assert_eq!(FrameSync::parse(None), FrameSync::DumpDiff);
        assert_eq!(FrameSync::parse(Some("")), FrameSync::DumpDiff);
        assert_eq!(FrameSync::parse(Some("dumpdiff")), FrameSync::DumpDiff);
        assert_eq!(FrameSync::parse(Some("on")), FrameSync::DumpDiff);
        assert_eq!(FrameSync::parse(Some("morph")), FrameSync::Morph);
        assert!(!FrameSync::DumpDiff.advertises_morph());
        assert!(FrameSync::Morph.advertises_morph());
    }

    /// A base mismatch surfaces as `ReackAndWait` (the caller re-acks and the
    /// server falls back to a Full) — the same handling as today's Diff base
    /// mismatch. The caller checks base == applied_num before dispatch, so the
    /// applier itself only ever sees a body it can apply; this asserts the
    /// DumpDiff path returns ReackAndWait for an undecodable diff.
    #[test]
    fn undecodable_diff_reacks_and_waits() {
        let mut term = Terminal::with_scrollback(ROWS, COLS, 0);
        let mut applier = DumpDiff;
        // A diff claiming a prefix longer than the (empty) base cannot apply.
        let mut bad_diff = Vec::new();
        bad_diff.extend_from_slice(&100u32.to_le_bytes());
        bad_diff.extend_from_slice(&100u32.to_le_bytes());
        let body = FrameBody::Diff {
            base: 0,
            diff: bad_diff,
        };
        let outcome = applier.apply(ROWS, COLS, b"", &mut term, &body);
        assert!(matches!(outcome, ApplyOutcome::ReackAndWait));
    }
}
