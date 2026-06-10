# posh: persistent, roaming terminal sessions

posh is a single Rust tool that combines:

- **zmx** (terminal session persistence — attach/detach from sessions without
  killing the underlying processes, window management delegated to the OS
  window manager), and
- **mosh** (roaming remote terminal over encrypted UDP that survives sleep,
  network changes, and intermittent connectivity).

This repository (the mosh fork, to be renamed `posh`) hosts the rewrite as a
Cargo workspace. The original C++ mosh tree remains in `src/` as the porting
reference; the original zmx (Zig) lives in its own repository.

## Layout

```
crates/
  posh-term/   standalone terminal emulation library (no dependencies)
  posh/        the posh binary
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
  cursor, modes, title, and scroll region on a real terminal — verified by
  roundtrip tests). This is what powers session replay on attach and remote
  state sync.

Known simplifications: resize truncates/pads rather than reflowing; DECCOLM
is ignored; graphics animation and shared-memory transmission answer with
error ACKs.

### crates/posh

The combined CLI. No async runtime; `poll()`-based event loops like both
originals.

**Session persistence (zmx port):**

```
posh attach <name> [command...]    # or bare: posh <name>; detach: Ctrl-\
posh list [--short]
posh run <name> [--] <command...>
posh detach [<name>]
posh kill <name>
```

Daemon-per-session over Unix sockets with zmx's binary IPC framing (1-byte
tag + u32 LE length; Input/Output/Resize/Detach/DetachAll/Kill/Info/Init/
History/Run/Ack). Each daemon feeds PTY output through a `posh_term::Terminal`
so re-attaching clients receive a full state replay via `dump_vt()`. Session
groups via `-g/--group` or `POSH_GROUP`; socket directory resolution:
`POSH_DIR` > `XDG_RUNTIME_DIR/posh` > `TMPDIR/posh-{uid}` > `/tmp/posh-{uid}`.
Sessions export `POSH_SESSION`/`POSH_GROUP`.

**Remote roaming (mosh port):**

```
posh ssh [user@]host [-- command]  # bootstrap over ssh, like mosh(1)
posh server [new] [-p PORT[:PORT2]] [-- command...]
posh client <host> <port>          # key via POSH_KEY, never on argv
```

Encrypted UDP datagrams using AES-128-GCM with mosh's nonce layout
(direction bit + 63-bit sequence number), replay protection with a reorder
window, timestamp echo for RFC 6298 RTT estimation, fragmentation for large
frames, and server-side roaming by adopting the source address of the last
authenticated datagram. State sync sends complete `dump_vt()` frames (or a
prefix/suffix diff against the last acked frame); user input is delivered
reliably via cumulative offsets and retransmission.

Known simplifications relative to mosh: no speculative local echo /
prediction overlay, no mosh SSP protobuf instructions or zlib, IPv4 only,
no port hopping or utmp integration.

## Building and testing

```
cargo build --workspace
cargo test  --workspace
```

The workspace builds warning-free and carries ~195 tests (parser state
machine, UTF-8 and wide-char edge cases, SGR colon forms, kitty keyboard
encode vectors, graphics ACK paths, dump_vt roundtrips, IPC framing, crypto
seal/open/replay/tamper, fragmentation, RTT, and daemon lifecycle
integration tests).
