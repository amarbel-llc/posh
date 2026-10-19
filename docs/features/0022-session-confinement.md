---
status: experimental
date: 2026-10-19
---

# Session confinement (refuse attach from an agent scope)

## Problem Statement

A session's unix socket has no notion of a client. Anything that can reach the
socket's `0700` directory is a full client: it can write the PTY (`Tag::Input`,
what `posh run` does with no tty), replay the screen, and kill the session. The
only protection is that directory's mode, and a program an agent runs is the
operator's uid.

That matters once the operator reads a security prompt in a posh session: a
signing prompt that shows the text being signed is only trustworthy if nothing
the agent controls can write to the surface it is drawn on (spinclass FDR 0034;
`piggy-agent(1)` SCOPES). Probed on a throwaway session
(`just debug-confinement-probe`): a tty-less, same-uid, `setsid`'d process runs
`posh run` and its command executes in the session shell.

## Interface

Default OFF; nothing changes until the operator sets, in the environment of the
process that CREATES the session (the daemon inherits it):

- `POSH_CONFINE=1` — enable. Any other value (or unset) is off.
- `POSH_CONFINE_DENY_CGROUP=<prefix>[:<prefix>…]` — cgroup-v2 path prefixes that
  are agent scope. A peer inside one is refused.
- `POSH_CONFINE_ALLOW_CGROUP=<prefix>[:<prefix>…]` — optional. When set, a peer
  must be inside one of these (deny still wins).

A refused connection is closed before it is a client: no input, no replay, no
kill, no `Tag::Info`. The observed side sees it three ways: the daemon log
(`attach refused: <why>`), the refused client's failed attach, and `posh
status`, whose session line gains
`confine=on confine_deny=N confine_allow=N confine_refused=N confine_last="…"`
(absent when off, so the off-path line is byte-identical to before).

## Design

- `session/confine.rs`: `Confine::admit(fd)` reads the peer pid with
  `SO_PEERCRED` at accept, reads `/proc/<pid>/cgroup` (the `0::` unified line),
  and `judge`s it — a pure function over a `Peer`, unit-tested without a
  socket. The verdict is `Allow | Deny(why)`, not a bool, so the read-only
  observer attach of posh#224 can add an `Observe` outcome at the one call site
  (`daemon_loop`'s accept) without reworking it.
- **Why a cgroup and not a secret.** The agent has the operator's uid, so any
  token the operator's frontend can read it can read, and a socket-mode or
  uid check sees the same uid. A process cannot lie about its cgroup (the
  kernel, not the peer, supplies the pid) and cannot leave a non-delegated
  one. A per-session attach secret remains a possible second factor but is only
  as strong as the mount-hiding that keeps the agent from reading it.
- **Fail closed.** A peer that cannot be placed (pid gone before the read,
  foreign pid namespace, no cgroup v2, non-Linux) is refused, even with an empty
  deny list. `ALLOW` is the stronger shape: with deny only, an agent that can
  reach the user manager can start a unit OUTSIDE the deny prefix and attach
  from there; with allow set to the operator frontend's scope it cannot.
- Prefix match is by path segment (`/a/b` covers `/a/b/c`, not `/a/bc`).
- Scope: the per-session daemon's socket. The roaming `posh-server` owns its PTY
  with no local socket (nothing to confine here); its remote peers authenticate
  by the session key. The mux endpoint and the agent-forwarding socket are
  separate sockets and are NOT covered by this record.

## Limits (stated plainly)

- This closes the socket. It does NOT close writes to the session's PTY slave
  (`/dev/pts/N`, mode `0620`, owned by the operator uid): a same-uid process
  that can open it paints on the operator's screen. That needs the agent unit
  not to see the operator's `/dev/pts` (private devpts / `PrivateDevices=`), and
  is the launcher's to arrange.
- It also depends on the launcher keeping the agent out of the user manager's
  bus, otherwise it can start a unit in an allowed cgroup.
- `SO_PEERCRED` is read after `accept`; a pid recycled between the peer's exit
  and the `/proc` read is the (narrow) residual race. `SO_PEERPIDFD` (Linux
  6.5+) would remove it.
- macOS: no cgroups; with confinement on every peer is refused there
  (`macOS gap (posh#214)` in `confine.rs`).

## Testing

- Unit (`confine.rs`): deny/allow/segment matching, fail-closed on an unknown
  peer, deny-wins, the `0::` parse among v1 lines, refusal counting, and
  `peer_of` against a real socketpair (the kernel path).
- Unit (`daemon.rs`): the status line carries `confine…` only when on.
- Native: `just debug-confinement-probe <posh> 1` starts a confined throwaway
  session and runs the agent step in a transient `systemd-run --user --scope`
  unit; expect `WRITABLE ATTACH: no`, `confine_refused>0`, and `attach refused`
  in the log. Without `1` it prints the unconfined baseline (`yes`).
