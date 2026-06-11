#!/usr/bin/env bash
# Cross-compile posht for a remote host, scp it over, and run it there —
# through posh's transport when posh is available, plain ssh -t otherwise.
#
#   ./run-remote.sh <[user@]host> [posht args...]
#
# posht is pure Go (CGO_ENABLED=0), so the binary is static and needs
# nothing on the remote beyond a UTF-8 locale.
set -euo pipefail

if [ $# -lt 1 ]; then
  echo "usage: $0 <[user@]host> [posht args...]" >&2
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

# Unique per-invocation path: a fixed /tmp/posht would clobber (and run)
# another user's binary on a shared host.
dest="/tmp/posht.$$"
echo ">> copying to $host:$dest" >&2
scp -q "$bin" "$host:$dest"

# posh ssh runs the command over the roaming transport, which is the
# pipeline posht is there to judge; fall back to ssh so the tool still
# works for baseline (non-posh) runs.
if command -v posh >/dev/null 2>&1; then
  exec posh ssh "$host" -- "$dest" "$@"
else
  exec ssh -t "$host" "$dest" "$@"
fi
