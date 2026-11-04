#!/usr/bin/env bash
# posh-ph-flic-timing.bash -- time ONE real `ph <host>:` connect, for the
# before/after comparison of POSH_SSH_SHARE (docs/plans/
# 2026-11-04-slow-connect-poor-links.md, "Operator timing script").
#
# Usage:
#   posh-ph-flic-timing.bash before [host]   # POSH_SSH_SHARE=0: old behaviour
#   posh-ph-flic-timing.bash after  [host]   # the new default (sharing on)
#
#   host defaults to flic. Run it from the CLIENT machine on the bad link (not
#   on flic itself: from flic, `ph flic:` is a local hop). Pick a session in the
#   picker as soon as it appears; once the session screen is up, detach
#   (Ctrl-^ then "Detach", or close the terminal tab) -- the script then writes
#   its results.
#
# Environment:
#   POSH_PH   the ph binary to run (default: `ph` on PATH). Point it at the
#             build under test, e.g. POSH_PH=~/src/posh/result/bin/ph.
#
# Output: ~/posh-timing/<UTC stamp>-<label>-<host>/
#   ssh.log     one line per ssh invocation: start/end epoch ms + its argv
#   posh.log    POSH_DEBUG_LOG (client side; the remote side writes the same
#               path on the remote host, if writable there)
#   mux.log     the lines the local mux endpoint logged during the run
#   summary.txt ready-to-send summary for the coordinator (the script prints
#               its path; it cannot send chat itself)
set -uo pipefail

label="${1:-}"
host="${2:-flic}"
case "$label" in
  before) share=0 ;;
  after) share=1 ;;
  *)
    echo "usage: $0 before|after [host]" >&2
    exit 2
    ;;
esac

ph="${POSH_PH:-$(command -v ph || true)}"
real_ssh="$(command -v ssh || true)"
if [ -z "$ph" ] || [ -z "$real_ssh" ]; then
  echo "need ph (or POSH_PH) and ssh on PATH" >&2
  exit 2
fi

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
out="$HOME/posh-timing/$stamp-$label-$host"
mkdir -p "$out/bin"
chmod 700 "$HOME/posh-timing" "$out"

# A logging ssh first on PATH: records when each ssh posh spawns starts and
# ends, then runs the real ssh with the same argv (prompts still work: stdio
# is inherited). `ssh -G` (posh's dial resolver) is logged too. A shared
# master that ssh backgrounds outlives its `end` line; that is expected.
cat >"$out/bin/ssh" <<EOF
#!/usr/bin/env bash
echo "\$(date +%s%3N) start \$*" >>"$out/ssh.log"
"$real_ssh" "\$@"
rc=\$?
echo "\$(date +%s%3N) end rc=\$rc" >>"$out/ssh.log"
exit \$rc
EOF
chmod 755 "$out/bin/ssh"

# posh's socket base, resolved the way posh does (POSH_DIR, then
# XDG_RUNTIME_DIR/posh, then TMPDIR/posh-UID, then /tmp/posh-UID).
uid="$(id -u)"
if [ -n "${POSH_DIR:-}" ]; then
  base="$POSH_DIR"
elif [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  base="$XDG_RUNTIME_DIR/posh"
elif [ -n "${TMPDIR:-}" ]; then
  base="$TMPDIR/posh-$uid"
else
  base="/tmp/posh-$uid"
fi
mux_lines_before="$(cat "$base"/mux/*.log 2>/dev/null | wc -l)"

echo ">> $label: ph $host: (POSH_SSH_SHARE=$share), results in $out"
t0="$(date +%s%3N)"
PATH="$out/bin:$PATH" POSH_SSH_SHARE="$share" POSH_DEBUG_LOG="$out/posh.log" \
  "$ph" "$host:"
rc=$?
t1="$(date +%s%3N)"

cat "$base"/mux/*.log 2>/dev/null | tail -n "+$((mux_lines_before + 1))" >"$out/mux.log"

# Re-stamp epoch-ms lines as +ms from the start of the run.
rel() {
  awk -v t0="$t0" '{ts=$1; gsub(/[^0-9]/,"",ts); if (ts=="") next; $1=""; printf "  +%7d ms%s\n", ts - t0, $0}'
}
{
  echo "posh ph timing: $label ($host, POSH_SSH_SHARE=$share), from $(hostname), $stamp"
  echo
  echo "ph ran $((t1 - t0)) ms in total (includes the picker choice and the time attached)."
  echo "ssh invocations: $(grep -c ' start ' "$out/ssh.log" 2>/dev/null || echo 0)" \
    "(of which ssh -G config lookups: $(grep -c ' start -G ' "$out/ssh.log" 2>/dev/null || echo 0))"
  echo
  echo "ssh start/end (non -G):"
  grep -v ' start -G ' "$out/ssh.log" 2>/dev/null |
    awk '{ if ($2=="start") { l=$0; sub(/^[0-9]+ start /,"",l); if (length(l)>110) l=substr(l,1,110) "..."; print $1, "start", l } else print $0 }' |
    rel
  echo
  echo "mux endpoint (start / session open / confirm / fallback):"
  grep -E 'mux daemon started|session OPEN sent|session confirmed|timed out|fallback|reconnect' "$out/mux.log" 2>/dev/null |
    sed 's/^\[\([0-9]*\)\]/\1/' | rel
  echo
  echo "client: first stats line with frames received (the first frame is at or before it):"
  grep -E '\[stats\]: client .*frames rx=[1-9]' "$out/posh.log" 2>/dev/null | head -n 1 |
    sed 's/^\[\([0-9]*\)\]/\1/' | cut -c1-120 | rel
  grep -E 'mux-fallback' "$out/posh.log" 2>/dev/null | sed 's/^\[\([0-9]*\)\]/\1/' | cut -c1-160 | rel
  echo
  echo "ph exit status: $rc"
} >"$out/summary.txt"

cat "$out/summary.txt"
echo
echo "summary to relay: $out/summary.txt"
