# POSHT — interactive remote terminal-capability test

POSHT ("diff on a POSH") is a standalone, statically linked Go TUI
(Charm Bubble Tea) that exercises the terminal-capability surface posh
claims to support, one feature per screen, and asks the human at the
keyboard to confirm what they actually see. It lives in [`posht/`](../posht/).

## Why it exists

The hermetic test lanes prove the emulator's *model* is right
(`cargo test --workspace` covers parsing, grid state, sync). What they
cannot prove is the end-to-end claim: that an attribute survives the whole
pipeline — remote pty → posh-term emulation → transport sync → attach
replay → your local terminal → your eyes. That last hop is exactly what
the manual smoke pass in [`manual-testing.md`](manual-testing.md) covers
today with ad-hoc shell probes, and what gates closing
[#34](https://code.linenisgreat.com/posh/issues/34).

POSHT turns that ad-hoc pass into a guided, repeatable checklist with a
machine-readable verdict at the end. Because it's a single static binary,
posh can scp it to the remote and run it *inside* a posh session — putting
posh itself in the loop being judged. Run it three ways and diff the
reports:

1. **locally** in your terminal — baseline: what your terminal can do;
2. **inside a local posh session** (`posh demo`, then run posht) — isolates
   the emulator + attach replay;
3. **on a remote over posh** (`posh ssh box -- /tmp/posht`) — the full
   headline pipeline, roaming transport included.

A capability that passes (1) but fails (2) or (3) is a posh bug, not a
terminal limitation. That's the "diff" in "diff on a POSH".

## What it tests

The checklist (first screen, all selected, deselectable — exactly the flow
requested) maps to the capability inventory of `crates/posh-term`:

| id | exercises | posh-term surface |
|---|---|---|
| colors16 | SGR 30–37/90–97/40–47/100–107 | `cell.rs` palette |
| colors256 | SGR 38;5 / 48;5 cube + ramp | `csi.rs` extended color |
| truecolor | 38;2 *and* colon 38:2:: forms | `csi.rs` both param forms |
| attrs | bold/dim/italic/blink/reverse/hidden/strike | `cell.rs` attrs |
| underline | styled underlines 4:1–4:5, SGR 58 color | `csi.rs` underline styles |
| wide | CJK/emoji double-width alignment | `wcwidth.rs` |
| combining | combining marks stack onto one cell | `cell.rs` extras |
| boxdraw | DEC special graphics ESC ( 0 | `terminal.rs` charsets |
| wrap | deferred wrap / pending-wrap at last column | `terminal.rs` pending_wrap; cf. [#2](https://code.linenisgreat.com/posh/issues/2) |
| scrollregion | DECSTBM margins | `csi.rs` DECSTBM |
| cursor | DECSCUSR six shapes | `csi.rs` cursor styles |
| mouse | all-motion + SGR reporting, wheel | `mouse.rs`, modes 1000–1006 |
| keys | modifier/function-key round trip | input path |
| paste | bracketed paste atomicity (2004) | `modes.rs` |
| resize | SIGWINCH propagation | winsize plumbing |
| title | OSC 0/2 | `osc.rs` |
| hyperlink | OSC 8 | `osc.rs` |
| clipboard | OSC 52 | `osc.rs`; known remote gap [#27](https://code.linenisgreat.com/posh/issues/27) |
| bell | BEL | known remote gap [#27](https://code.linenisgreat.com/posh/issues/27) |
| graphics | kitty APC G inline image | `graphics.rs`; known remote gap [#29](https://code.linenisgreat.com/posh/issues/29) |

Tests carrying a known posh gap say so on screen, so a fail there is
recorded but not mistaken for a new bug (mirrors the "known gaps — do not
file as new bugs" section of the manual plan).

Three interaction shapes:

- **look-and-confirm** — static scene, user judges it (colors, attrs, …);
- **do-and-observe** — the app reacts to the user (mouse coordinates echo,
  paste atomicity, key echo, live resize) so the screen itself proves the
  capability;
- **raw interludes** — autowrap and scroll regions can't be demonstrated
  through a TUI renderer (it never touches the real last column and owns
  scrolling), so these temporarily drop out of the alternate screen
  (`tea.Exec`), run vttest-style probes on the primary screen, and return.
  This doubles as an alt-screen enter/leave/restore test every time.

Verdicts are `y`/`n`/`s` per test; `←`/`→` revisit; the summary screen and a
markdown report (stdout on exit, `-o file.md` to save) record the run with
TERM/COLORTERM/host metadata. Exit status is non-zero if anything failed,
so a wrapper can collect reports mechanically.

## CLI

```
posht                 # checklist → run → summary → prints the receipt path
posht --list          # print test ids
posht --only wide,combining
posht --skip graphics,bell
posht -o report.md    # also write the markdown report to a file
posht --json -        # machine-readable JSON receipt to stdout
posht --json out.json # JSON receipt to a named file (prints the path)
posht --auto          # non-interactive: render the static tests to stdout at a
                      # fixed 80-col width, then exit (deterministic)
```

`--auto` skips the interactive walk entirely and renders the selected *static*
tests (colors, attributes, gradients, wide chars, box drawing, hyperlinks) to
stdout at a fixed width, then exits — no Bubble Tea, no alt screen, no receipt.
The output is byte-identical across runs and terminals, so recording it over
posh vs plain ssh (the `debug-record-posht` recipe with `--auto`) yields two
frame-aligned recordings to diff, isolating posh's transport/render from the
content. Interactive/stateful tests (mouse, keys, resize, cursor, …) are
skipped — they need live input and can't self-drive deterministically.

A JSON receipt is always produced. By default it is written to
`~/.local/log/posht/<datetime>-<terminal>.json` and the **path** is printed to
stdout on exit (not the contents — `cat` the path for those). `--json -` puts
the JSON itself on stdout instead; `--json <file>` writes a named file and
prints its path. The markdown report is no longer auto-printed; pass `-o
<file>` to produce it.

The `<terminal>` label is derived from the **process tree**, not `$TERM` —
`$TERM` lies on macOS (iTerm2 and Terminal.app both inherit `xterm-kitty`),
and the receipt's embedded `process_tree` is the trustworthy identifier.

## Building

```
nix build .#posht     # hermetic build (buildGoModule), ./result-posht/bin/posht
just build-go         # the same, via the justfile lane (part of `just build`)
go build .            # fast in-posht/ dev-loop (needs Go ≥ 1.25)
```

## Getting it onto the remote

`posht/run-remote.sh <host>` does the loop today: asks the host for its
OS/arch over ssh, cross-compiles (`CGO_ENABLED=0`, pure Go — no libc
needed on the target), scp's to `/tmp/posht`, and runs it through
`posh ssh <host> -- /tmp/posht` (falling back to `ssh -t` when posh isn't
on PATH). Go's cross-compilation makes the "static binary for any remote"
requirement free: any GOOS/GOARCH pair builds from any dev machine.

### Future: `posh posht [host]` subcommand

Worth folding into posh proper once the tool settles:

- `posh posht` — run the embedded/local posht in the current session;
- `posh posht <host>` — push the right-arch binary and run it over the
  posh transport, collect `-o` report back over the wire.

Open questions for that step: where the per-arch binaries come from
(embed a few in the posh package? build lazily? download from a release?),
binary-size budget (~3 MB stripped today), and version skew between posh
and posht on opposite ends. Keeping POSHT a separate binary (not a posh
subcommand compiled in) stays deliberate: the *thing being tested* should
not be the thing rendering the test.

## Non-goals / follow-ups

- **Machine-checkable probe mode** (`posht --probe`): query DA1, XTGETTCAP
  (`colors`, `RGB`/`Tc`), DECRQSS, OSC 4/10/11 color queries, and kitty
  keyboard-protocol detection, and record the *responses* without human
  judgement. posh-term answers all of these (`dcs.rs`, `osc.rs`), so probe
  mode would regression-test the query path that fingerprinting tools
  (vim, notcurses) rely on. Needs raw response parsing outside Bubble Tea.
- **Kitty keyboard protocol coverage**: posh-term implements the
  progressive-enhancement stack (`kitty_keys.rs`), but Bubble Tea v1
  doesn't enable it; the `keys` test exercises legacy encoding only.
- **zsh-style session/host completion tie-in** ([#37](https://code.linenisgreat.com/posh/issues/37))
  is unrelated plumbing but shares the "cheap remote query" question.
