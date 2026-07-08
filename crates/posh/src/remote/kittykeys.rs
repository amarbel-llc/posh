//! Kitty keyboard control-key matching shared by both clients.
//!
//! Under the kitty keyboard protocol a control key like Ctrl-^ (the command
//! palette summon) is NOT sent as its raw C0 byte (`0x1e`) but as a CSI-u
//! sequence (`\x1b[54;5u` — base key `6`=54, ctrl modifier `;5`; with an
//! optional `:1` explicit-press suffix). posh#130 taught the LOCAL client's
//! `EscapeKeyMatcher` to recognize these forms; posh#131 does the same for the
//! ROAMING client via [`PaletteKeyNormalizer`]. Both need the same primitive
//! match against a fixed set of complete CSI-u sequences, so it lives here —
//! `session/client.rs` already depends on the `remote` module, so this is an
//! importable shared home with no new dependency edge.

/// Ctrl-^, the command-palette summon key, in its kitty keyboard CSI-u forms:
/// base key `6` (54), ctrl modifier (`;5`), with and without the explicit `:1`
/// press-event suffix. We deliberately do NOT list the `:3` (release) or `:2`
/// (repeat) event variants — under the report-all-events kitty flag every key
/// (bare modifiers included) emits press+release CSI-u, and a release must not
/// re-trigger the palette.
pub(crate) const KITTY_PALETTE_SEQS: [&[u8]; 2] = [b"\x1b[54;5u", b"\x1b[54;5:1u"];

/// The palette summon key Ctrl-^ as its raw C0 byte, the form both clients'
/// escape handling ultimately keys off (the normalizer rewrites the CSI-u forms
/// to this).
pub(crate) const ESCAPE_KEY: u8 = 0x1e;

pub(crate) enum KittyMatch {
    /// The slice begins with a full candidate sequence of this many bytes (the
    /// caller needs the length to route the bytes after the key).
    Full(usize),
    /// The slice is a proper prefix of some candidate sequence (need more bytes).
    Partial,
    /// No candidate sequence starts here.
    No,
}

/// Match the start of `s` against a set of complete CSI-u sequences.
pub(crate) fn match_kitty_seqs(s: &[u8], seqs: &[&[u8]]) -> KittyMatch {
    let mut partial = false;
    for seq in seqs {
        if s.len() >= seq.len() {
            if &s[..seq.len()] == *seq {
                return KittyMatch::Full(seq.len());
            }
        } else if seq.starts_with(s) {
            partial = true;
        }
    }
    // A bare `\x1b` is a prefix of every CSI-u sequence, but it is far more
    // often a real Escape keypress (interrupt/clear in a TUI). Holding it back
    // to disambiguate stalls Escape until the next keystroke arrives — there is
    // no flush timer — so a lone Escape is only forwarded when the user presses
    // another key, and a double-Escape registers as one (posh#126).
    // Require at least the `\x1b[` CSI introducer before treating the buffer as
    // a control-key-in-progress. The cost is that a sequence whose kitty
    // encoding is split by the tty *between* the `\x1b` and the `[` (vanishingly
    // rare — a terminal emits a CSI-u sequence atomically) is missed that once;
    // the user simply presses the key again. Escape latency is the common case.
    if partial && s.len() >= 2 {
        KittyMatch::Partial
    } else {
        KittyMatch::No
    }
}

/// Rewrites the roaming client's stdin batch so a kitty-CSI-u palette key
/// (`\x1b[54;5u` or `\x1b[54;5:1u`) is collapsed to a single raw `0x1e` BEFORE
/// the byte-state-machine in `remote::client::process_user_input` sees it
/// (posh#131). Raw `0x1e` and every other byte pass through untouched, so the
/// existing `ESCAPE_KEY` / `quit_pending` / `Ctrl-^ .` / `Ctrl-^ ^` handling is
/// unchanged.
///
/// The carry holds a trailing partial that could still complete a palette
/// sequence across reads (a torn CSI-u), exactly as the local `EscapeKeyMatcher`
/// does. Bytes that begin `\x1b[` but resolve to a non-palette CSI (release
/// `:3`, repeat `:2`, a bare modifier like `\x1b[57442;5u`) pass through verbatim
/// once disambiguated.
///
/// Unlike the local matcher there is no `palette_enabled` gate and no detach set:
/// over roaming the palette is always available (spawned lazily) and detach is
/// the `Ctrl-^ .` byte sequence, not a key. On a renderer-can't-spawn fallback
/// the collapsed `0x1e` drives the existing quit-prefix machinery — the original
/// CSI-u bytes are not preserved, matching the local client's own fallback
/// (`session/client.rs`, which likewise forwards raw `0x1e`).
#[derive(Default)]
pub(crate) struct PaletteKeyNormalizer {
    carry: Vec<u8>,
}

impl PaletteKeyNormalizer {
    /// Feed one stdin batch; return the rewritten batch. A complete palette
    /// CSI-u becomes a single `0x1e`; a trailing partial is held in `carry` and
    /// prepended to the next feed. Everything else (raw `0x1e` included) is
    /// copied through byte-for-byte.
    pub(crate) fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(input);
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if data[i] == 0x1b {
                match match_kitty_seqs(&data[i..], &KITTY_PALETTE_SEQS) {
                    KittyMatch::Full(key_len) => {
                        out.push(ESCAPE_KEY);
                        i += key_len;
                        continue;
                    }
                    KittyMatch::Partial => {
                        // The rest may complete a palette key on the next read;
                        // hold it and emit only what we have resolved so far.
                        self.carry = data[i..].to_vec();
                        return out;
                    }
                    KittyMatch::No => {}
                }
            }
            out.push(data[i]);
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw Ctrl-^ (`0x1e`) is copied through unchanged — the byte loop opens the
    /// palette on it as it always did (the legacy / `cat` path).
    #[test]
    fn raw_ctrl_caret_passes_through() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"\x1e"), b"\x1e");
        let mut n2 = PaletteKeyNormalizer::default();
        assert_eq!(n2.feed(b"ab\x1ecd"), b"ab\x1ecd");
    }

    /// posh#131: the kitty CSI-u palette key (both the bare and `:1` press
    /// forms) collapses to a single raw `0x1e`.
    #[test]
    fn kitty_palette_key_becomes_escape() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"\x1b[54;5u"), b"\x1e");
        let mut n2 = PaletteKeyNormalizer::default();
        assert_eq!(n2.feed(b"\x1b[54;5:1u"), b"\x1e");
    }

    /// Bytes before and after the palette CSI-u are preserved; only the key
    /// itself is rewritten.
    #[test]
    fn kitty_palette_key_preserves_surrounding_bytes() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"hi\x1b[54;5ubye"), b"hi\x1ebye");
    }

    /// The `:3` (release) and `:2` (repeat) event variants are NOT the palette
    /// press; they pass through verbatim (the app may want them). Under
    /// report-all-events the terminal emits these for every key.
    #[test]
    fn kitty_release_and_repeat_pass_through() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"\x1b[54;5:3u"), b"\x1b[54;5:3u");
        let mut n2 = PaletteKeyNormalizer::default();
        assert_eq!(n2.feed(b"\x1b[54;5:2u"), b"\x1b[54;5:2u");
    }

    /// Exact-codepoint discipline: a bare modifier key's CSI-u (Left Ctrl =
    /// `57442`, Left Alt = `57443`, emitted under report-all-events) shares the
    /// `;5` modifier but NOT the `54` base, so it must pass through — never be
    /// misread as a palette summon.
    #[test]
    fn bare_modifier_csi_u_passes_through() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"\x1b[57442;5u"), b"\x1b[57442;5u");
        let mut n2 = PaletteKeyNormalizer::default();
        assert_eq!(n2.feed(b"\x1b[57443;1:3u"), b"\x1b[57443;1:3u");
    }

    /// A CSI-u palette key torn across two reads still collapses: the first feed
    /// holds the partial (emitting nothing), the second completes it to `0x1e`.
    #[test]
    fn kitty_palette_key_torn_across_two_feeds() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"\x1b[54"), b""); // held as partial
        assert_eq!(n.feed(b";5u"), b"\x1e");
    }

    /// Ordinary text is copied through untouched.
    #[test]
    fn plain_text_batch_unchanged() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"hello world"), b"hello world");
    }

    /// Multiple raw `0x1e` in one batch all pass through — the normalizer does
    /// not special-case the first (the byte loop handles each).
    #[test]
    fn raw_escape_mid_batch_with_surrounding_bytes() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"ab\x1ecd\x1eef"), b"ab\x1ecd\x1eef");
    }

    /// A lone `\x1b` (real Escape) is forwarded immediately, never held as a
    /// possible CSI-u prefix — Escape latency is preserved (posh#126 discipline;
    /// `match_kitty_seqs` requires len >= 2 for a Partial).
    #[test]
    fn lone_escape_forwards_immediately() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"\x1b"), b"\x1b");
        assert_eq!(n.feed(b"a"), b"a");
    }

    /// An ESC-led non-CSI sequence (Alt-b) passes through intact.
    #[test]
    fn alt_key_escape_prefix_passes_through() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"\x1bb"), b"\x1bb");
    }

    /// A held partial that resolves to a NON-palette sequence on the next read
    /// is forwarded intact — the held prefix is not lost.
    #[test]
    fn carry_then_non_palette_resolves_and_forwards() {
        let mut n = PaletteKeyNormalizer::default();
        assert_eq!(n.feed(b"\x1b[54"), b""); // held
        assert_eq!(n.feed(b"foo"), b"\x1b[54foo");
    }
}
