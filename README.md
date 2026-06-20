# POSH: the portable shell

posh makes a shell session *portable*: it roams across networks and
persists across disconnects, so the session you start follows you — survive
a sleep, change networks, detach here and reattach from another machine,
and the shell and its running processes are exactly where you left them.

It delivers this as a single Rust tool that combines two lineages:

- **zmx** (terminal session persistence — attach/detach from sessions without
  killing the underlying processes, window management delegated to the OS
  window manager), and
- **mosh** (roaming remote terminal over encrypted UDP that survives sleep,
  network changes, and intermittent connectivity).

Portability is the two together: mosh-style roaming carries a session across
the *network*, zmx-style persistence carries it across *time and machines*,
and the unified `host:session` namespace addresses both with one grammar.

This repository hosts the rewrite as a Cargo workspace. The original C++
mosh tree is kept under `zz-mosh/` as the porting reference (with its own
justfile for the host-lane build: `just zz-mosh/<recipe>`); the original
zmx (Zig) lives in its own repository.

## Layout

```
crates/
  posh-term/   standalone terminal emulation library (no dependencies)
  posh/        the posh binary
  poshterity/    deterministic terminal recorder/replayer (lib + poshterity bin; posh rec)
  mosh-ffi/    C++ FFI oracle: drives mosh's terminal + predictor for differential tests (dev/test only; ADR 0004)
doc/           scdoc man-page sources (man posh, posh-server, posh-client, poshterity, posh(7))
docs/          ADRs, RFCs, feature records (FDRs), plans, and the manual test plan
posht/         interactive terminal-capability test (Go; nix build .#posht)
zz-mosh/       the C++ mosh reference tree (buildable: nix build .#mosh)
```

### crates/posh-term

A from-scratch Rust rewrite of the ghostty-vt terminal core as a completely
independent, dependency-free library crate. It is pure state: feed it PTY
bytes with `Terminal::process`, query the resulting screen, and drain query
replies (DA/DSR/OSC/kitty) with `take_responses`.

Feature set targets kitty parity, with the sequences fish shell integration
relies on fully supported:

- Williams DEC parser state machine; incremental UTF-8 with malformed-input
  replacement; C1 controls; colon SGR subparams.
- Primary screen with scrollback ring + alternate screen; wide chars,
  combining marks, pending-wrap semantics, BCE, margin-aware scrolling,
  origin mode, tab stops, DECALN, REP.
- Full SGR including styled underlines (single/double/curly/dotted/dashed),
  underline color (58/59), 256-color and truecolor in both `;` and `:` forms.
- Modes: DECCKM, DECOM, DECAWM, DECTCEM, alt screen 47/1047/1049, bracketed
  paste 2004, mouse 9/1000/1002/1003 with SGR/SGR-pixel protocols, focus
  reporting 1004, synchronized output 2026, IRM, LNM.
- OSC: 0/1/2 title, 4/10/11/12 palette and dynamic colors (set + query),
  7 (cwd), 8 (hyperlinks), 52 (clipboard), 133 (shell-integration prompt
  marks, as emitted by fish), 9/99 (notifications), 22 (pointer shape).
- Kitty keyboard protocol: full flag stack (push/pop/set/query) plus a
  client-side `encode_key` covering legacy and CSI u encodings.
- Kitty graphics protocol: APC G parsing, transmit/place/delete/query,
  RGB/RGBA/PNG formats, chunked transmission, 320 MB quota, spec ACKs.
- DCS: DECRQSS, XTGETTCAP; queries: DA1/DA2, DSR, DECRQM, XTVERSION,
  XTWINOPS 14/16/18.
- Serialization: `dump_text()` (plain text including scrollback) and
  `dump_vt()` (an escape stream that reconstructs contents, attributes,
  cursor, modes, title, scroll region, and kitty graphics — images,
  placements, animation frames — on a real terminal, verified by roundtrip
  tests). This is what powers session replay on attach and remote state
  sync.

Also implemented: reflow on resize (logical lines rewrap via wrap flags,
wide-char aware, scrollback included; alt screen truncates/pads like kitty),
DECCOLM/DECNCSM column switching, kitty graphics relative placements and
animation frame storage with the full delete-specifier set, file-based
graphics transmission, OSC 52 per-selection slots, the xterm color stack
(XTPUSHCOLORS/XTPOPCOLORS/XTREPORTCOLORS), DECSTR soft reset, selective
erase (DECSCA/DECSED/DECSEL), and a client-side `encode_mouse` covering
X10/normal/UTF-8/SGR/SGR-pixel.

Graphics payloads are fully decoded in-crate with no dependencies: a
hand-written RFC 1950/1951 inflate (for `o=z`) and a PNG decoder (8-bit
gray/gray+alpha/RGB/RGBA/indexed, all filters, PLTE/tRNS, CRC-verified)
feed `f=100` images and animation frames as RGBA, with frame compositing
(`a=c`, blend or replace) and `composed_frame()` for renderers.

Known simplifications: Adam7-interlaced and 16-bit PNGs are rejected;
shared-memory graphics transmission answers EUNSUPPORTED (sandbox-safe);
OSC 66 text sizing is parsed but scale is not rendered.

### crates/posh

The combined CLI. No async runtime; `poll()`-based event loops like both
originals.

**Session persistence (zmx port):**

```
posh attach <name> [command...]    # or bare: posh <name>; detach: Ctrl-\
posh list [--short|--json]
posh run <name> [--] <command...>
posh fork [<name>]                 # fork current session (same cmd + cwd)
posh detach [<name>] | detach-all
posh kill <name>
posh groups
posh history <name> [--vt]
posh completions <bash|zsh|fish>
```

Attaching takes over the outer terminal's alternate screen (terminfo
smcup/rmcup for $TERM via a built-in term(5) reader, hardcoded 1049 when
no database answers, skipped under `--no-init`/$POSH_NO_TERM_INIT or for
terminals without an alternate screen) and detaching restores it, so the
shell prompt you attached from comes back exactly as you left it
(FDR 0002: `docs/features/`). The daemon virtualizes the
session's own alt-screen switches and RIS in the broadcast — replaced
with model-generated repaints — so full-screen apps inside the session
can never flip the outer terminal off posh's screen. Session scrollback is
reached by scrolling the wheel at a bare prompt (FDR 0005) — or as text via
`posh history`; the outer terminal's native scrollback stays the shell's own.

Daemon-per-session over Unix sockets with zmx's binary IPC framing (1-byte
tag + u32 LE length; Input/Output/Resize/Detach/DetachAll/Kill/Info/Init/
History/Run/Ack/Exit — Exit carries the shell's status so an attached
client exits with the session's real code). Each daemon feeds PTY output
through a `posh_term::Terminal`
so re-attaching clients receive a full state replay via `dump_vt_flat()`. Session
groups via `-g/--group` or `POSH_GROUP`; socket directory resolution:
`POSH_DIR` > `XDG_RUNTIME_DIR/posh` > `TMPDIR/posh-{uid}` > `/tmp/posh-{uid}`.
Sessions export `POSH_SESSION`/`POSH_GROUP`.

**Remote roaming (mosh port) and the unified namespace:**

```
posh [user@]host [-- command]      # like mosh(1): plain roaming shell
posh [user@]host:[group/]session   # persistent session on the host over
                                   # the roaming transport — attach-or-
                                   # create, detach here, reattach from
                                   # anywhere; [fe80::1]:dev for IPv6
posh :[group/]session              # explicit local attach
posh list host:                    # remote listing, host-prefixed names
posh ssh [-4|-6] [-p RANGE] [user@]host [-- command]
posh server [new] [-p PORT[:PORT2]] [-4|-6] [-- command...]
posh client [-4|-6] <host> <port>  # key via POSH_KEY, never on argv
```

The grammar is scp-style and total (RFC 0001: `docs/rfcs/`); every
pre-namespace form keeps its meaning. The remote session's exit status
propagates: `posh box:dev; echo $?` reports the session shell's code.

Tailscale peers are first-class hosts: shell completion offers tailnet
peer names (MagicDNS) alongside `~/.ssh/config` aliases and session names
(`posh tailnet` lists them), and the roaming transport falls back to a
peer's tailnet IP when the system resolver can't reach its MagicDNS name.
Both degrade silently without `tailscale`.

The ssh bootstrap runs `posh-server new` on the remote host (mosh-server
parity); the package installs `posh-server` as an alias of `posh`, so the
server host just needs the package on its non-interactive PATH.

Encrypted UDP datagrams using AES-128-GCM with mosh's nonce layout
(direction bit + 63-bit sequence number), replay protection with a reorder
window, timestamp echo for RFC 6298 RTT estimation, fragmentation for large
frames, and server-side roaming by adopting the source address of the newest
authenticated datagram (late reorders never re-target the stream). State sync sends complete `dump_vt()` frames (or a
prefix/suffix diff against the last acked frame); a client that advertises
SCROLLBACK also accumulates the primary-screen scrollback incrementally
(append-only rows, per-frame cost bounded by inter-frame growth rather than
ring depth — RFC 0002); user input is delivered reliably via cumulative
offsets and retransmission.

The client takes over the alternate screen for the whole connection
(mosh's smcup/rmcup) and restores the pre-connect shell screen on exit
and around suspend. It renders mosh-style: it maintains a local `posh_term::Terminal`,
morphs the real tty with minimal per-cell diffs (a port of
`terminaldisplay.cc`), and runs a faithful port of mosh's prediction engine
(`terminaloverlay.cc`): speculative local echo with epochs, confirmation
against server echo-acks, adaptive display with mosh's SRTT/glitch/flagging
constants, and underlined predictions when the link is slow
(`POSH_PREDICTION_MODEL`: always/never/adaptive/experimental). `POSH_PREDICTION_MODEL=optimistic`
instead echoes keystrokes immediately and lets the next paint correct them, gated
on the remote PTY's ECHO flag and alt-screen (FDR 0006). The prediction model and
the render style are independent, swappable axes: `POSH_PREDICTION_RENDER` selects
how predicted cells are painted (`replace`, the default, or `dim`). `POSH_PREDICTION`
is a deprecated alias for `POSH_PREDICTION_MODEL`. A reverse-video
"Last contact N seconds ago" banner appears after 6.5s of silence; the quit
sequence is Ctrl-^ then `.` (Ctrl-^ Ctrl-Z suspends the client, Ctrl-^ s drops
to a shell on the server in the session's directory). Servers
bind dual-stack IPv6 when possible,
report `POSH IP` from `$SSH_CONNECTION` for the ssh wrapper, require UTF-8
locales on both ends (forwarding LANG/LC_* over ssh), forward TERM and
COLORTERM so the session isn't left color-blind (TERM resolved to an entry
the remote's terminfo database actually has), and honor
`POSH_SERVER_NETWORK_TMOUT` / `POSH_SERVER_SIGNAL_TMOUT`.

The renderer also ports mosh's scroll optimization (matched rows are
scrolled with `\r\n` runs or a DECSTBM region instead of being rewritten)
and emits OSC 8 hyperlinks.

Because the whole connection lives on the outer terminal's alternate screen,
the mouse wheel at a bare prompt is at the mercy of that terminal's
alt-screen wheel behavior — kitty, for one, turns it into arrow keys and
ignores DECSET 1007 (posh#3/#28). posh grabs the wheel (mouse reporting) at
a bare prompt and, by default, scrolls the session's own primary-screen
scrollback locally from the client's accumulated ring — instant, no
round-trip, frozen while scrolled, with a top status-bar indicator; any
keystroke or scrolling back to the bottom resumes the live view (FDR 0005,
posh#43). `POSH_GRAB_MOUSE=on` reverts the wheel to the legacy transform
(wheel up/down → arrow keys, normalizing scroll across terminals, posh#50)
instead of scrollback. Either way, grabbing the wheel costs the outer
terminal's click-to-select while active; a session app that tracks the mouse
itself (vim, tmux) keeps the wheel.

Known simplifications relative to mosh: frames carry `dump_vt()` state (or
a prefix/suffix diff) rather than mosh's SSP protobuf instructions with
zlib; no utmp/motd integration. The full parity contract — what is
mirrored, what is deliberately dropped, and the open gaps — is FDR 0003
(`docs/features/`), with the living checklist in issue #44.

### crates/poshterity

A deterministic terminal recorder/replayer built on `posh-term`: replay a
recorded output byte stream through the in-process emulator and inspect the
exact screen, with no live terminal and no timing to race (the
`tmux capture-pane` + `sleep` flake that motivates it). Depends only on
`posh-term`; surfaced as the standalone `poshterity` binary and as `posh rec`.

```
poshterity record [--out f.castx] -- <cmd>       # record a command under a PTY
posh --record f.castx <session>                # record a live posh session
poshterity replay <file> [--dump text|vt|flat]   # or: posh rec replay ...
poshterity step <file> --by change --n 3         # step-debug, dump each screen
poshterity bless  <file> --golden g --at K       # write a golden-frame snapshot
poshterity assert <file> --golden g --at K       # check it (CI gate)
```

The recording format is `.castx`, a strict superset of asciinema `.cast` v2
(standard `o`/`i`/`r` events plus an ignorable `m` marker and a `poshterity`
header block), so any `.cast` replays through poshterity and any `.castx` plays
in `asciinema`. `step` advances by an emulator-defined granularity
(`byte`/`escape`/`write`/`change`/`frame`/`marker`) and dumps the intermediate
screen — a deterministic VT100 frame debugger. `bless`/`assert` snapshot the
screen at a marker (`grid` is diff-friendly text + a style sidecar; `vt` is the
raw escape stream) — the deterministic analog of `tmux capture-pane`; a library
of typed assertion helpers (`find_line`, `cells_have_fg/bg`, `cells_are_*`)
renders a colored expected-vs-actual diff on mismatch. Issue #56 tracks the
epic; adoption + the `.castx` RFC land in the final phase.

## Building and testing

```
nix build                   # the full toolset: posh, posh-server, poshterity, posht (#73)
nix build .#posh            # hermetic build + cargo test (posh workspace; mosh-ffi gated separately)
just build-rust             # same, via the justfile lane
just debug-cargo test --workspace   # fast in-worktree dev-loop (includes mosh-ffi)
nix run .#poshterity -- ... # the recorder/replayer as a standalone tool
nix build .#checks.<sys>.mosh-ffi   # the C++ FFI oracle gate (just test-mosh-ffi)
nix build .#posht           # the interactive capability test (just build-go;
                            # part of `just build`/`test`). See docs/posht.md.
```

The workspace builds warning-free and carries ~400 tests (parser state
machine, UTF-8 and wide-char edge cases, reflow, SGR colon forms, kitty
keyboard encode vectors, graphics ACK paths, inflate and PNG decode
vectors, frame compositing, dump_vt roundtrips, IPC framing, crypto
seal/open/replay/tamper, fragmentation, RTT, prediction engine state
transitions with injected clocks, display-diff and scroll-optimization
morphing roundtrips, IPv6 loopback, and daemon lifecycle integration
tests).
