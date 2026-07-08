#!/usr/bin/env bash
# posh#131/#134 ground-truth capture: what bytes does a terminal emit for a key
# once the kitty keyboard protocol is negotiated?
#
# Pushes the kitty flag stack (CSI > flags u) requesting all enhancements
# (0b11111 = 31: disambiguate + report-events + alternate-keys + all-keys-as-
# escape + associated-text), reads each keypress and prints its hex + cat -v
# rendering, then POPS the stack (CSI < u) and restores the tty on exit.
#
# Two ways to run it:
#   * DIRECTLY in a real terminal (no posh) — the terminal's own encoding, the
#     ground truth for what Ctrl-^ etc. become under kitty.
#   * As the SESSION COMMAND over a posh roaming pair (see the
#     debug-verify-remote-kittycap justfile recipe) — proves whether the kitty
#     enable reaches the CLIENT terminal over the transport and what the client
#     sends. `posh attach` can't be the capture (it needs a tty stdin that also
#     EOF-exits); this script IS a tty program, so it rides the session PTY.
#
# posh#132: the `stty` calls below deliberately DO NOT combine `raw` with an
# explicit `min`/`time` (`raw` already implies `min 1 time 0`). The redundant
# combination trips BSD `stty`'s "unable to perform all requested operations"
# partial-apply warning over a roaming session PTY, garbling capture. Set `raw`
# once for the blocking read; adjust only `min`/`time` (not `raw` again) for the
# drain, on the already-raw tty.
#
# Press keys to see their encoding (Ctrl-^ is the posh#130/#131 key). Press q to
# quit cleanly.

set -u

saved=$(stty -g)
cleanup() {
  printf '\033[<u' # pop the kitty flag stack we pushed
  stty "$saved"    # restore the saved tty settings
  printf '\n-- restored; kitty stack popped --\n'
}
trap cleanup EXIT INT TERM

hexdump_stdin() { od -An -tx1 | tr '\n' ' ' | tr -s ' ' | sed 's/^ //;s/ $//'; }

# 1) Query current flags (CSI ? u) + DA (CSI c) sentinel. A kitty-capable
#    terminal replies CSI ? flags u before the DA; one that doesn't answers only
#    the DA. Read on a short timer (this is a terminal reply, not a keypress).
#    `raw` implies min 1 time 0; override to a timed poll via min/time WITHOUT
#    re-passing `raw` (posh#132).
printf '\033[?u\033[c'
stty raw -echo
stty min 0 time 3
query=$(dd bs=1 count=64 2>/dev/null | hexdump_stdin)
stty "$saved"
printf 'kitty query reply (raw hex): %s\n' "${query:-(none)}"
printf '  (only ...63[c] = protocol UNSUPPORTED; a ...75[u] group present = supported)\n\n'

# 2) Push all-enhancements and capture keypresses.
printf '\033[>31u'
printf 'kitty ALL-enhancements pushed. Press keys (q to quit).\r\n'
printf 'Ctrl-^: raw "1e" => legacy C0; "1b 5b .. 75" (ESC [ .. u) => a CSI-u form.\r\n\r\n'

stty raw -echo # raw once; the loop only nudges min/time below.
while :; do
  # Block for the first byte of a keypress (raw already = min 1 time 0)...
  stty min 1 time 0
  first=$(dd bs=1 count=1 2>/dev/null)
  # ...then drain the rest of a multi-byte sequence on a 100ms quiet timer,
  # so a CSI-u sequence is captured whole without stealing the next keypress.
  stty min 0 time 1
  rest=$(dd bs=1 count=31 2>/dev/null)

  seq="$first$rest"
  hex=$(printf '%s' "$seq" | hexdump_stdin)
  caret=$(printf '%s' "$seq" | cat -v)
  printf 'hex: %-28s  cat-v: %s\r\n' "$hex" "$caret"
  [ "$hex" = "71" ] && break # lone q quits
done
