# Options for posh's control exchange without a fresh ssh login, 2026-11-04

Design note, no code. The operator asked: *"can we also examine using a UDP
protocol / channel like the ssh-agent forwarding?"* Read as: could posh carry
its **control exchange** (the picker's session listing, session discovery,
the bootstrap handshake) over a channel that does not need a fresh ssh login
each time?

Context: `2026-11-04-slow-connect-poor-links.md` found that the serial fresh
ssh logins (each about 7-9 round trips) dominate a `ph host:` connect on a
poor link. `POSH_SSH_SHARE` (2026-11-04) already cuts a cold `ph host:` from
two logins to one, but every `ph host:` still runs at least one ssh session
for the listing.

## What already exists to build on

- **The mux endpoint** (FDR 0014, RFC 0011): one local daemon per
  destination holds one encrypted, roaming UDP connection to a remote
  `posh-server agent --client-id <id>`. It already carries agent channels
  (the "ssh-agent forwarding" the operator means) and M2 session channels.
  The key is a fresh AEAD key that `posh-server` prints on the ssh-protected
  bootstrap (`POSH CONNECT`). It lingers locally for `POSH_MUX_PERSIST`
  (60 s by default), and the remote side exits after its peer timeout
  (`PEER_TIMEOUT` 60 s, `remote/server.rs:52`).
- **The UDP port range** 60001-60999 (`datagram.rs:12`) is what every
  posh-server already binds, so a tailnet-only host like flic already
  accepts posh UDP on it from tailnet peers. That is the existing network
  surface, and options that stay on it need no firewall change.
- **The remote session directory** `<base>/<group>/` with per-session
  sockets and status sockets is what `posh list` reads. Any listing answer
  is a read of that directory as the user.

## Options

### A. OpenSSH connection reuse (ControlMaster), driven by posh — shipped

posh passes `ControlMaster=auto`, `ControlPath=<base>/ssh/%C` and
`ControlPersist=60` to its control ssh spawns (`POSH_SSH_SHARE`).

- **Server side:** nothing. sshd must allow session multiplexing
  (`MaxSessions`, default 10).
- **Security:** unchanged; it is ssh. The control socket is a live
  authenticated login, so it sits in the private `<base>/ssh/` directory
  (0700, self-owned, hardened like `mux/` and `agent/`).
- **Cost after the first login:** one ssh session open (about 2 RTT)
  plus a remote exec, which may start a login shell and nix profile.
- **Limits:** it is TCP, so it does not roam. A master stranded by a
  network change makes its slaves hang until the keepalive kills it (about
  30 s). It overrides a user's own ControlMaster setup for those spawns.
- **Verdict:** a good stopgap, already shipped, with no protocol work.

### B. A control channel kind on the mux endpoint (RFC 0011) — recommended

Add a request/response channel kind, for example `control`, beside agent
and session channels. The local mux daemon sends `list [-g G] --json` (and
later `kill`, `start --detach`, the probe for `ph host:+`). The remote
`posh-server agent` answers by reading its own session directory, the same
code path as `posh list --json`. The picker asks the endpoint instead of
spawning ssh whenever the endpoint is warm.

- **Server side:** nothing new on the network (the existing per-client
  UDP port). One new handler in the remote `posh-server agent`, which
  already runs as the user on the host.
- **Security:** the same as agent forwarding. Only someone holding the
  connection's AEAD key can send an authentic datagram, and that key was
  delivered over ssh to a local daemon whose IPC socket is the user's
  private `<base>/mux/<key>.sock`. "Who may ask for the session list" is
  "a local process of the same user, through that 0700 socket", exactly
  the trust boundary that already lets that process attach sessions through
  M2. Nothing new is exposed to the network.
- **Cost:** 1 RTT over UDP, retransmitted on the endpoint's RTO, and it
  survives a roam. No remote shell start.
- **Needs:** an RFC 0011 amendment (new channel kind and its message
  shape, plus a capability bit so an older remote is detected and the
  client falls back to ssh) and an RFC 0001 capability registry entry. A
  cold endpoint still needs one ssh login (the bootstrap).
- **Verdict:** the best steady-state answer. It turns the warm `ph host:`
  listing from an ssh session into one UDP round trip on a channel that
  already exists.

### C. Fold the listing into the endpoint bootstrap (cold path only)

When the endpoint is cold, `ph host:` runs the endpoint bootstrap ssh
**first**. The remote `posh-server agent` prints its session listing on the
bootstrap stream before `POSH CONNECT` (for example a `POSH LIST <json>`
line), and the picker draws from it.

- **Server side:** a flag on `posh-server agent`; no network change.
- **Security:** the ssh session that already carries the key.
- **Compatibility:** an older client already withholds and ignores unknown
  `POSH ` lines (`sshwrap.rs` `LineScraper` / `ServerReport::feed`), so the
  extra line is skew-safe. It is still a bootstrap-stream contract change,
  so it would need an RFC entry.
- **Verdict:** with A shipped, it saves the remaining list session on a cold
  connect (about 2 RTT). Worth doing only together with B, since B covers
  the warm case and C the cold one.

### D. Resume the endpoint without ssh (a resumption ticket)

Keep the remote `posh-server agent` alive longer than the local daemon, and
let a new local daemon reuse the saved key and port to contact it directly.
This skips the ssh bootstrap entirely while the remote side lives.

- **Server side:** a longer remote linger, so a per-client process sits on
  one UDP port per client host for that time.
- **Security:** the AEAD key must then be **persisted** on the client (a
  long-lived credential at rest, in a private file). That credential grants
  session attach, so it is as sensitive as an ssh key without a passphrase.
  It needs expiry, a defined rotation, and replay handling for the first
  packet. The trust boundary moves from "this ssh login" to "this file".
- **Verdict:** a real latency win (zero ssh on a resumed connect), but a
  new security surface. Consider it only after B, and only with explicit
  expiry.

### E. UDP discovery/ping to posh-server on a known port, ssh as fallback

A persistent per-user (or system) posh listener on a fixed UDP port answers
"what sessions do you have" and "start an endpoint for me"; ssh is used only
when it does not answer.

- **Server side:** a long-running listener that does not exist today (posh
  servers are per-connection), a fixed port per user (or a multi-user
  daemon running as root, which posh avoids), and that port opened on the
  host firewall for the tailnet. flic being tailnet-only limits exposure
  but does not remove the need to authenticate.
- **Security:** the hard part. Without ssh, the server needs a
  pre-provisioned client credential: a key enrolled by an earlier ssh
  bootstrap (which is D), or the user's ssh public keys verified by a
  signature over a server challenge (reimplementing ssh userauth, with the
  agent on the client). Tailscale identity (WhoIs on the peer address) could
  be an outer gate but ties posh to tailscaled. A listing leaks session
  names and activity labels, which are sensitive (commands, paths, titles).
- **Verdict:** not recommended. It needs a new daemon, a new port and a new
  authentication scheme to save what B already saves on a warm endpoint.

### F. Others considered

- **Tailscale SSH or tsnet** as the bootstrap: removes key prompts, not
  round trips, and ties posh to Tailscale. Out of scope.
- **Serve the picker from the completion cache** (FDR 0015 already keeps an
  out-of-band cache of remote names): draw at once, refresh in the
  background. No protocol change, and it removes the listing from the
  critical path entirely at the cost of staleness. A cheap UX win
  independent of A-E.

## Recommendation

1. Keep **A** (shipped) as the default and the universal fallback.
2. Build **B**, a control channel kind on the mux endpoint, as the main
   answer. It reuses the agent-forwarding trust model the operator pointed
   at, needs no network or firewall change, roams, and makes a warm
   `ph host:` listing one UDP round trip. Version skew falls back to A.
3. Add **C** with B, so a cold `ph host:` needs exactly one ssh login for
   both the listing and the endpoint.
4. Draw the picker from the completion cache (F) at once, so even the one
   remaining round trip is off the visible path.
5. Do not build **E**. Revisit **D** only with persisted-key expiry and a
   security review.

## Open questions

- B's message shape: reuse the existing `posh list --json` JSON verbatim,
  so the picker parser is shared?
- Should B's `list` be limited to the client's own group, or show all
  groups, as `posh list` does today?
- How should B behave while the endpoint is `Reconnecting`: wait, or fall
  back to ssh at once?
