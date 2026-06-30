# Unified session transport — approved design

Design trail for the unification of local and remote posh sessions into one
durable, attach-anywhere shape. The normative artifacts this produced are
**FDR 0011** (the user-facing feature) and **RFC 0008** (the protocol and the
RFC 0001 grammar amendments) — read those first. This document records the
decision trail and the alternatives weighed.

## What

Today a posh session has three runtime shapes with two protocols:

| Shape | PTYs / models | Persistence | Protocol to client |
|---|---|---|---|
| local session (`posh dev`) | 1 | daemon | `Tag` frames over a Unix socket |
| plain roaming shell (`posh box`) | 1 | **none** (ephemeral) | `ServerFrame` over AEAD-UDP |
| remote session (`posh box:dev`) | **2** | daemon | daemon→`Tag`→inner `posh attach`→posh-server→`ServerFrame`→UDP |

The remote-session shape is FDR 0001's "Architecture A" (transport
composition): `posh-server new -- posh attach dev` runs the inner attach in a
second PTY and **models the terminal twice**. The session daemon already
models the terminal server-side, but it speaks a poorer `Tag::Output` raw-dump
protocol over its socket than the wire's diffed `ServerFrame`.

The goal: **one shape.** Every shell is a durable daemon owning a PTY +
`posh_term::Terminal`; "local" vs "remote" collapses to *which transport you
dial*. A session started anywhere is attachable from anywhere, durable across
disconnect. The concrete driver is reattaching spinclass/clown remote workers
through one unified interface.

## Decisions (with rationale)

1. **The daemon becomes the single frame producer.** The `FrameState` +
   diff/morph send loop moves down from `posh-server` beside the daemon's
   existing `Terminal`; the codecs already live in `posh-proto` (#75). The
   daemon emits one `ServerFrame` stream consumed by both transports. It stops
   emitting `Tag::Output`. ("Push the local side to map to the remote side.")
2. **The reliable Unix socket is the degenerate lossless case of the lossy
   datagram protocol.** Same send loop; over the socket the acked base is
   always the last frame, so retransmit/RTO, fragmentation, and AEAD never
   fire. No special "local Full-only" path — the loss machinery just idles.
   Local attach inherits morph, scrollback-sync, and base-integrity for free.
3. **Architecture A3 — B now, C-ready core, not C.** `posh-server` becomes a
   thin disposable **relay** (Unix-socket frames ⇄ AEAD-UDP), with no second
   PTY or model. The durable daemon stays **off the network** (Unix socket
   only); the crypto/roaming/fragmentation blast radius stays in the throwaway
   relay. Daemon-native UDP (C) is left as a clean later option (FDR 0001:
   "only worth it if B's proxy hop ever matters"). Rejected C: it puts the
   riskiest code in the process that must never die and enlarges every
   daemon's security surface.
4. **One capability vocabulary for both transports.** The client appends the
   RFC 0001 §3 TLV capability table on `Tag::Init`; the daemon answers in
   kind. A cap added for the wire lights up locally, and vice versa. Absence
   of the frame cap → the daemon falls back to `Tag::Output` (the
   dual-architecture window).
5. **The client unifies symmetrically; the palette falls out locally.** Once
   the local client consumes frames it must hold a client-side `Snapshot` +
   `FrameApplier` — exactly the substrate the palette composites onto. The
   palette compositor + `posh-palette` renderer lift out of `remote/` into a
   shared client; both clients become "consume frames → Snapshot → composite
   palette → render," one client over two transports. Palette-in-local-sessions
   is an intended consequence, not a separate feature.
6. **Content caps end-to-end; the relay is transparent to them.**
   `EXIT_STATUS`, `SCROLLBACK`, `MORPH`, `BASE_SUM`, `TERM_FEATURES`,
   `PROTOCOL_VERSION` are negotiated client↔daemon; the relay forwards their
   table entries and the opaque frame body. `BASE_SUM` is naturally dormant on
   the reliable transport.
7. **Agent forwarding stays relay-owned and is scoped OUT of the core.** The
   per-pid `srv-<pid>.sock` + newest-wins `agent/sock` symlink (FDR 0004) is
   unchanged; the daemon never brokers keys. The genuinely hard part —
   `SSH_AUTH_SOCK` is bound once at shell spawn, which the unification promotes
   the #53 edge case into the common case for local-origin/detached-spawn
   sessions — is deferred. Near-term O1: `Setenv` IPC / `posh setenv` (#53).
   End-state O2: a host-global filesystem rendezvous
   (`~/.local/state/ssh/ssh_client-agent.sock`, env-set-once at login),
   tracked as **#103**. Core ships with today's agent behavior intact for
   sessions created through a forwarding connection.
8. **One explicit interface: `attach` + a picker, not a default-shell special
   case.** `posh attach <uri>` is attach-or-create. No URI → a TTY-gated
   charmbracelet picker over unified local+remote rows (the spinclass
   `internal/sessionpick` pattern; non-TTY → error-with-candidates, never a
   hang). A host-only URI (`box:`, `user@box:`, `:` for local) scopes the
   picker to that host. **Everything is durable**; the nameless ephemeral
   shell is removed, with `posh attach --ephemeral` as the opt-in throwaway
   (deferrable). This **revises RFC 0001 §1**: `box:` and `:` flip from
   plain-shell / `LocalSession{":"}` to host-scoped / local-scoped pickers.
9. **`posh list` is durable + unified + spinclass-styled.** Lists local +
   remote sessions; **hides remotely-spawned detached workers** by default
   (`--workers` to include them), mirroring spinclass's `running-detached`
   filtering. Render modes mirror `sc list` (`list_view.go`): plain
   (non-TTY/pipe), pretty lipgloss table (TTY default), JSON, and a watch
   Bubble Tea view — all TTY-gated so pipes stay clean. Sessions gain a
   settable description/label (spinclass parity) that feeds both `list` and
   the picker.

## Tuning levers

- Idle-reap timeout for auto/abandoned durable shells: **off** (durable by
  default). Signal: abandoned-default-shell sprawl across hosts becomes real
  clutter.
- Capability table on every datagram/Init: inherited from RFC 0001; revisit to
  send-until-first-ack if `TERM_FEATURES`-scale payloads grow.

## Rollback

- **Socket protocol:** single `POSH_FRAMESYNC`-style build/env switch forces
  the daemon to emit `Tag::Output` only and the bootstrap to use the
  inner-attach composition — today's Architecture A, intact. Negotiated, so
  mixed-version peers degrade, never corrupt; durable daemons that predate the
  upgrade are handled by the same negotiation.
- **Grammar:** a single parser revert restores RFC 0001 §1. The picker is
  TTY-gated, so non-TTY/scripted use never hits it regardless.
- **Durability:** `--ephemeral` is the live escape hatch to the old throwaway
  shell.
- **Promotion criterion** (retire the double-model): both fleet hosts on a
  frame-protocol build, two weeks of daily cross-host use, zero observed
  fallback to the `Tag::Output` / inner-attach path.

## Testing

- Daemon frame production reuses the `framereplay` deterministic harness (#75)
  — the local path becomes deterministically testable for the first time.
- Reliable-as-degenerate property test: identical input through the Unix and a
  lossless UDP transport yields identical client `Snapshot`s.
- 4-way socket version-skew matrix (old/new daemon × old/new client),
  mirroring RFC 0001's UDP skew matrix.
- e2e (sandbox-safe, loopback): local diff/morph/scrollback/exit-status;
  palette over a local session; `list` worker-filtering + picker
  attach-or-create.
- Cross-host (real sshd, agent forwarding, roam) stays in
  `docs/manual-testing.md`.
