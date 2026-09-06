# FDR 0012 remote retarget — implementation plan (2026-09-04)

The in-session switch (`posh attach <sibling>` inside a session) works for a
LOCAL viewport (session/client.rs re-dials) but is a silent no-op for a
viewport reaching the daemon through a relay or M2 mux channel: the daemon
routes `Tag::Switch` to the bridge, whose daemon-frame reader has a `_ => {}`
arm (server.rs `mux_peer_loop`, relay.rs `relay_loop`) that drops it. Symptom:
`switching to X` prints (the sender ran) but the viewport never repaints. This
is the FDR 0012 §3.1 remote half.

## Mechanism (decided)

Bridge-local re-home + a one-way client target-update notice — NOT a
client-mediated re-OPEN. The immediate switch is handled where the daemon
signal lands (the bridge), reusing the frame-continuity machinery from the
reconnect fix; the client is told the new target only so a LATER wire
reconnect re-OPENs the switched session, not the original (the RFC 0008 §3.1
"update the stored target before the re-drive" rule).

On `Tag::Switch{group\0session}` from the daemon read, the bridge:

1. Converts the payload to the wire target `connect_named_daemon` parses:
   `session` for the default group, else `group/session`.
2. Re-homes its `DaemonLink`: drop the old stream, `connect_daemon(target)`,
   build a fresh link with `frame_offset = last_frame_num` (the client's frame
   ceiling — the SAME continuity the reconnect fix carries, so the new
   session daemon's low frame numbers rewrap above the client's applied_num
   and its `Full` is not dropped as stale), re-`Init` with the client's stored
   content caps + size, reset the per-channel fresh-attach state (held, inbox,
   acked_forwarded, ack_due), and KEEP last_frame_num + frame_flags.
3. Sends `SESSION_WIRE_SWITCH{wire target}` to the client on the channel.

The client (`mux_loop`), on `SESSION_WIRE_SWITCH`, updates `IpcSession.target`
so a subsequent reconnect re-drives the OPEN with the switched target. It does
NOT re-OPEN (the bridge already re-homed).

## Pieces

- mux.rs: `SESSION_WIRE_SWITCH = 3` micro-kind (bridge → client, body = wire
  target). `mux_loop`: on receipt for a channel, `IpcSession.target = target`.
- server.rs `SessionBridge`: store the client's content caps (`content:
  Vec<Cap>`) at link so the re-home can re-`Init`. A `rehome(bridge, target,
  connect_daemon)` helper. `mux_peer_loop` frame reader: `ipc::Tag::Switch`
  arm → convert target, re-home, send `SESSION_WIRE_SWITCH`.
- relay.rs `relay_loop`: the per-invocation (`POSH_MUX_SESSIONS=0`) path —
  same bridge-local re-home on `Tag::Switch`, no client notice (a relay has
  no mux-daemon reconnect; it dies with its attach). DONE (2026-09-06, #180):
  relay_loop gained a `content` param (the negotiated caps, threaded from
  `run`), captures the switch after its daemon-read match, and re-homes via
  `Config::new(group)` + `connect_or_create` + a fresh `DaemonLink` with
  `frame_offset = last_frame_num`, resetting held/inbox.

## Coverage (closed 2026-09-06, #185)

The first deploy of the switch surfaced two bugs the helper-only tests could
not see, both in the SWEEP around the re-home rather than the re-home itself:
the endpoint drained the new link's Init through the OLD link's captured fd
and closed the channel (#184), and both bridges reset the input inbox, which
the viewport's outbox keeps counting past, so every post-switch keystroke was
dropped as a gap (#186).

Two structural changes followed. `SessionBridge` and `relay_loop` now hold a
shared `DaemonLeg` (`relay.rs`: link + held frame + forwarded ack + owed ack)
whose ONE constructor seeds both offset translations from the client's
ceiling; a resume-base Link and a re-home each replace the leg wholesale, so
there is no per-field reset list to get wrong, and the viewport-side state
(inbox, echo maturity, size, ceiling, flags, caps) persists by construction.
`DaemonLeg::forward_ack` owns the `acked_forwarded - frame_offset`
translation with a checked subtraction (loud in debug builds).

And the drives now exist: `mux_peer_switch_rehomes_channel_with_frame_and_
input_continuity` (server.rs, on the `start_peer` harness) and
`relay_switch_rehomes_with_frame_and_input_continuity` (relay.rs, on the
`Harness` rig via a new switch-connector seam on `relay_loop`). Each OPENs to
A, delivers a daemon `Tag::Switch` to B, and asserts the channel survives the
sweep, B is Init'd lossy, B's frames land above A's ceiling carrying the
persisted input ack, post-switch input at the continuing offset reaches B,
and an A-numbered ack is not forwarded while a B-numbered one is translated.
Either drive fails on #184 or #186 by construction.

## Key facts (verified)

- `connect_named_daemon` (server.rs) parses `[group/]session`; default group
  is the bare `session`. Matches `remote_list_line` / `switch_in_place`'s
  group handling.
- `relay::rewrap` forwards `frame_offset`-adjusted `frame_num` and daemon
  flags verbatim; the client's ack is un-offset by `- frame_offset` in the
  bridge, so seeding `frame_offset` fixes both directions (as in the reconnect
  fix). SessionMsg stays opaque.
- The client stale gate (client.rs:2436) drops `frame_num < applied_num`
  before body dispatch — the reason the offset is load-bearing.

## Tests

- `SESSION_WIRE_SWITCH` const + target-conversion roundtrip.
- The re-home helper: after re-home the link points at the new target and
  `frame_offset == last_frame_num`.
- Ideally an in-process `mux_peer_loop` drive: OPEN a channel to A, deliver a
  daemon `Tag::Switch` to B, assert the client sees B's frames (numbered above
  A's ceiling) and a `SESSION_WIRE_SWITCH` reaches the client.

## Reconnect-race

Covered by step 3 + the client target-update: a reconnect after a switch
re-drives OPEN with the updated `IpcSession.target` and the resume base, so it
reattaches to the switched session, not the original.
