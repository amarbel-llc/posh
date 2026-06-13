posh(7)

# NAME

posh - concepts: the host:session namespace, roaming, and persistence

# DESCRIPTION

This page explains the model behind *posh*(1): the unified namespace
that addresses every attachable thing, the roaming transport that
survives network changes, and the persistence rules that decide when a
session lives and dies. It is the conceptual companion to the command
reference; the normative trail lives in the repository's RFC and
feature records (see _MORE INFORMATION_).

# THE UNIFIED NAMESPACE

posh has historically had two attach worlds: local persistent sessions
(zmx-style, *posh* _dev_) and roaming remote shells (mosh-style,
*posh* _box_). posh unifies them under one scp-style grammar so a
*persistent named session on another machine, over the roaming
transport, detachable here and reattachable there* is a single target.

A bare first argument resolves to one of four targets; the first rule
that matches wins, and resolution never errors, so every legacy form
keeps its meaning:

- _name_ — a local session (attach-or-create).
- *:*[_group_/]_session_ — an explicit local session.
- [_user_@]_host_ — a plain roaming shell.
- [_user_@]_host_*:*[_group_/]_session_ — a persistent session on a
  remote host, over the roaming transport.

A *group* is a namespace for sessions, materialized as a subdirectory
of the socket directory; *-g* or _$POSH_GROUP_ selects it. IPv6 hosts
are bracketed (*[fe80::1]:dev*) so their colons are not mistaken for
the session separator. See *posh*(1) _TARGET GRAMMAR_ for the exact
rules.

# ROAMING TRANSPORT

Remote sessions ride an encrypted UDP transport ported from mosh:
datagrams are AEAD-sealed and replay-protected, the server sends
latest-screen-*state* (re-produced, never retransmitted byte-for-byte),
and the client sends a reliable cumulative input stream. Because the
connection is identified by its key rather than its address, it
survives the client's IP changing, NAT rebinding, and laptop
sleep/resume — the client simply keeps sending and the server answers
the new address once an authentic datagram arrives.

The bootstrap is an ssh login that runs *posh-server*(1) on the host,
prints a *POSH CONNECT* line, and exits; the long-lived transport is
UDP, not the ssh connection (which is gone seconds later). This is why
posh cannot lean on *ssh -A* and similar connection-scoped facilities.

# PERSISTENCE

A *session* is owned by a daemon, not by any client or connection. The
daemon is double-forked at first attach and owns the PTY running the
shell; clients attach and detach over a Unix socket (locally) or
through the roaming transport (remotely), and several may be attached
at once, all multiplexing through the one daemon.

The persistence rules:

- *Detach, disconnect, and roam leave the session running.* *Ctrl-\\*
  detaches a local client; *Ctrl-^ .* quits the transport; a network
  drop just pauses the connection. In every case the daemon keeps the
  shell alive, so the session can be reattached later — even from a
  different machine, with a full screen replay.
- *The shell exiting ends the session.* When the daemon's shell exits,
  the daemon reaps it, sends SIGHUP/SIGKILL to the whole process group
  (so background jobs do not outlive the session), notifies any
  attached clients with the real exit code, removes the socket, and
  exits with that code. A subsequent attach to the same name creates a
  fresh session.

For remote sessions the transport pair (*posh-server*(1) plus an inner
*posh attach*) is disposable — one pair per roaming client — while the
remote daemon is what persists. The remote _$SSH_AUTH_SOCK_ and shell
environment are inherited once, at daemon spawn.

# TERMINAL TAKEOVER

On attach, posh takes over the terminal's alternate screen (so the
prompt you attached from is restored on detach, like a full-screen
application) and restores it on exit. The enter/leave sequences come
from the terminfo _smcup_/_rmcup_ capabilities for _$TERM_, falling
back to DECSET 1049; a terminal whose entry defines no alternate
screen is left alone. *--no-init* (or _$POSH_NO_TERM_INIT_) opts out,
matching mosh.

# PREDICTION

Like mosh, the client echoes keystrokes speculatively and reconciles
against authoritative server state, so typing feels local even on a
high-latency or lossy link. _$POSH_PREDICTION_ selects the display
policy (_adaptive_ by default); see *posh-client*(1).

# MORE INFORMATION

The normative design records live in the repository:

- _docs/rfcs/0001-target-grammar-and-capability-table.md_ — the total
  target grammar and the datagram capability table.
- _docs/features/_ — Feature Design Records: 0001 (the unified
  host:session namespace), 0002 (terminal takeover and restore), 0003
  (the mosh-parity surface), 0004 (ssh agent forwarding).
- _docs/decisions/_ — ADR 0001, platform constraints on the transport.

# SEE ALSO

*posh*(1), *posh-server*(1), *posh-client*(1), *mosh*(1)

# COPYING

posh is free software, licensed under the GNU General Public License
version 3 or later. It is a combined Rust rewrite of zmx and mosh.
