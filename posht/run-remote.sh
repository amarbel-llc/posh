#!/usr/bin/env bash
# Cross-compile posht for a remote host, scp it over, and run it there —
# through posh's transport when posh is available, plain ssh -t otherwise.
#
#   ./run-remote.sh [--via ssh|session[=NAME]] <[user@]host> [posht args...]
#   ./run-remote.sh --deploy DEST <[user@]host>
#
# --deploy builds posht and scp's it to host:DEST, then prints DEST and exits
# WITHOUT launching — for callers that drive the launch themselves (e.g. the
# debug-record-posht recipe, which records the launch through posh-rec). The
# remote file persists at DEST.
#
# --via selects how posht is launched on the host:
#   ssh             (default) over `posh ssh` — the plain-SSH wrapper path
#                   (posh#3). posht is the foreground command of a one-shot
#                   roaming shell.
#   session[=NAME]  over `posh host:NAME` — a PERSISTENT roaming session
#                   (RFC 0001 §2; the posh#28 1007-sync path). Use this to
#                   reproduce the wheel→arrows bug, which lives in the
#                   per-frame mode sync that the ssh wrapper does not run.
#                   NAME defaults to "posht".
#   plain           over plain `ssh -t` with NO posh in the loop, even when
#                   posh is installed — the ground-truth render to diff a posh
#                   recording against when chasing drawing bugs.
#
# posht is pure Go (CGO_ENABLED=0), so the binary is static and needs
# nothing on the remote beyond a UTF-8 locale.
set -euo pipefail

via=ssh
session=posht
deploy_dest=
case ${1:-} in
--via)
  # ${2:-} not $2: a bare trailing `--via` must reach the clean usage error
  # below, not abort with a raw `set -u` unbound-variable message.
  via=${2:-}
  shift
  shift || true
  ;;
--via=*)
  via=${1#--via=}
  shift
  ;;
--deploy)
  deploy_dest=${2:-}
  if [ -z "$deploy_dest" ]; then
    echo "$0: --deploy requires a DEST path" >&2
    exit 64
  fi
  shift
  shift || true
  ;;
--deploy=*)
  deploy_dest=${1#--deploy=}
  if [ -z "$deploy_dest" ]; then
    echo "$0: --deploy= requires a DEST path" >&2
    exit 64
  fi
  shift
  ;;
esac
case $via in
session=*)
  session=${via#session=}
  via=session
  ;;
ssh | session | plain) ;;
*)
  echo "$0: --via must be ssh, session[=NAME], or plain, got: $via" >&2
  exit 64
  ;;
esac

if [ $# -lt 1 ]; then
  echo "usage: $0 [--via ssh|session[=NAME]] <[user@]host> [posht args...]" >&2
  exit 64
fi
host=$1
shift

# Ask the remote what to build for (one ssh round-trip).
read -r kernel machine < <(ssh "$host" uname -sm)
kernel=$(tr '[:upper:]' '[:lower:]' <<<"$kernel")
case $machine in
x86_64) arch=amd64 ;;
aarch64 | arm64) arch=arm64 ;;
*)
  echo "$0: unmapped remote arch: $machine" >&2
  exit 1
  ;;
esac

src=$(cd "$(dirname "$0")" && pwd)
bin=$(mktemp -d)/posht
trap 'rm -rf "$(dirname "$bin")"' EXIT

echo ">> building posht for $kernel/$arch" >&2
CGO_ENABLED=0 GOOS=$kernel GOARCH=$arch \
  go -C "$src" build -trimpath -ldflags='-s -w' -o "$bin" .

# --deploy uses the caller-chosen path (stable across runs, so a recorder can
# re-launch it); otherwise a unique per-invocation path, since a fixed /tmp/posht
# would clobber (and run) another user's binary on a shared host.
if [ -n "$deploy_dest" ]; then
  dest=$deploy_dest
else
  dest="/tmp/posht.$$"
fi
echo ">> copying to $host:$dest" >&2
scp -q "$bin" "$host:$dest"

# --deploy: the caller drives the launch (e.g. records it through posh-rec).
# Report the remote path on stdout and stop here.
if [ -n "$deploy_dest" ]; then
  echo "$dest"
  exit 0
fi

# Launch posht on the host. Both posh forms put the roaming transport in
# the loop posht judges; --via session adds session persistence and, more
# importantly, exercises the per-frame mode sync where the 1007 bug lives.
# --via plain (or no posh on PATH) is the non-posh baseline.
if [ "$via" = plain ]; then
  echo ">> launching over plain ssh -t (no posh in the loop)" >&2
  exec ssh -t "$host" "$dest" "$@"
fi
if command -v posh >/dev/null 2>&1; then
  case $via in
  session) exec posh "$host:$session" -- "$dest" "$@" ;;
  ssh) exec posh ssh "$host" -- "$dest" "$@" ;;
  esac
else
  echo ">> posh not on PATH; falling back to plain ssh -t (no posh in the loop)" >&2
  exec ssh -t "$host" "$dest" "$@"
fi
