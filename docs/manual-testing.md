# Manual test plan: posh (Rust rewrite)

Hands-on smoke pass for the merged rewrite (Waves A–C of the
[#34](https://github.com/amarbel-llc/posh/issues/34) campaign). Follow
top-to-bottom on a fresh clone; each section ends with what you should see.

## 0. Build

posh is the flake's default package
([#33](https://github.com/amarbel-llc/posh/issues/33)):

```sh
nix build                 # or: just build-rust
P=$PWD/result/bin/posh    # bin/posh-server is installed alongside
```

For an iterative dev-loop build instead:
`just debug-cargo build --release --workspace` → `target/release/posh`.

Both ends of any remote test need a UTF-8 locale (`LC_ALL=C.UTF-8` works).

## 1. Session persistence (zmx side)

```sh
$P demo                  # bare name = attach; creates session + your shell
# type a few commands, then detach: Ctrl-\
$P list                  # demo listed, clients=0
$P attach demo           # reattach
$P kill demo
```

- [ ] Detach returns you to your shell cleanly (no raw-mode garbage).
- [ ] Takeover/restore (FDR 0002): attach from a prompt with visible
      history; after detach the *pre-attach* screen is back — old prompt,
      old output, cursor on the shell line. Same after session exit.
- [ ] Reattach replays the screen: prior output, cursor position, modes.
- [ ] Inside the session: open `vim`, quit it → session shell screen
      repaints in place; then detach → original outer screen restored
      (the inner alt-screen cycle must not leak to the outer terminal).
- [ ] Inside the session: run `reset` (RIS) → session screen resets but
      the outer terminal stays on posh's screen; detach still restores.
- [ ] `posh --no-init attach demo` → no alt-screen switch (attach clears
      in place like pre-FDR-0002); detach leaves the session's last
      screen visible, prompt usable.
- [ ] `posh list` counts clients correctly before/after.
- [ ] Exit status: `$P attach ec sh -c 'exit 7'; echo $?` prints 7 (#18).

## 2. Signal handling (#14 — newest fix, highest attention)

```sh
$P demo                          # terminal A
pgrep -f 'posh attach demo'      # terminal B
kill <client-pid>                # SIGTERM
```

- [ ] Terminal A: client exits **code 0** (`echo $?`), prompt returns with
      echo/line-editing intact, no mouse/paste modes latched.
- [ ] Session survives: `$P list` still shows it; reattach replays.
- [ ] While attached: `Ctrl-Z` then `fg` → screen repaints.
- [ ] Repeat the kill with `SIGINT` and `SIGHUP` — same clean exit.

## 3. Remote loop on localhost

```sh
$P server -- fish                # prints: POSH CONNECT <port> <key>
POSH_KEY=<key> $P client 127.0.0.1 <port>
```

- [ ] Interactive shell works; quit sequence `Ctrl-^` then `.` exits clean.
- [ ] Quit restores the pre-connect shell screen (FDR 0002); the
      `posh: [client exited]` notice prints on it. `Ctrl-^ Ctrl-Z` suspend
      shows the shell screen, `fg` returns to the session repainted.
- [ ] SIGTERM the client from another terminal → clean exit **and** the
      server winds down (`pgrep -f 'posh server'` goes empty) instead of
      lingering until the 60s peer timeout.
- [ ] `POSH_PREDICTION=always` before the client → speculative local echo
      visible (predictions underlined on a slow link).
- [ ] Optional probe (#25): in the remote shell run
      `printf '\033[?40h\033[?3h'` (132-column mode) — the client must NOT
      garble; the local render stays at your tty width.

## 4. Cross-host

```sh
$P user@otherhost                # mosh-style: plain roaming shell
$P ssh otherhost                 # explicit form (for bare ssh aliases)
$P otherhost:dev                 # persistent session over the transport
$P list otherhost:               # remote session listing
$P otherhost:work/w1 --detach -- sleep 600      # detached spawn (#67)
$P -g work list otherhost:       # group-scoped remote listing (#66)
```

All run `posh-server new` over ssh (the session form wraps an inner
`posh attach`). Server-host requirements: `posh-server` AND `posh` on the
**non-interactive ssh PATH** (the nix package installs both; for a cargo
build, symlink `target/release/posh` to `~/.local/bin/posh-server`) —
otherwise the wrapper reports "did not find posh server startup message"
— and UDP 60001–60999 reachable.

- [ ] Session comes up; typing survives suspending the laptop / switching
      networks (roaming).
- [ ] "Last contact N seconds ago" banner appears ~6.5s after cutting the
      network, clears on reconnect.
- [ ] `posh otherhost:dev`, `Ctrl-\` to detach, reattach from a second
      machine: full replay, both machines can take turns.
- [ ] Exit the session shell with `exit 3`; `echo $?` locally prints 3.
- [ ] `posh otherhost:<Tab>` completes the remote session names (second
      Tab is instant — cached).
- [ ] Detached spawn (#67): `posh otherhost:work/w1 --detach -- sleep 600`
      returns promptly (no attach, no roaming client), printing the remote
      `session "w1" created`/`already exists` status. Re-running it is
      idempotent. A later `posh otherhost:work/w1` attaches to that same
      running session. (For the session forms the group is taken from the
      target — `host:group/session` — not the global `-g`.)
- [ ] Group-scoped remote listing (#66): `posh -g work list otherhost:`
      lists `w1` as `otherhost:work/w1` (pastes back into the same group);
      `posh list otherhost:` (default group) does NOT show it.

## 5. SSH agent forwarding (FDR 0004)

Forwarding is on by default whenever a local agent exists. Prereq: a local
agent holding a key — `ssh-add -l` on the machine you're sitting at shows a
fingerprint. That fingerprint is the proof target.

```sh
ssh-add -l                       # note the SHA256 fingerprint(s)
$P otherhost:dev                 # forwarding is on by default; -A to force + warn loudly
# ...then, inside the forwarded session:
echo $SSH_AUTH_SOCK              # => <base>/agent/sock (e.g. $XDG_RUNTIME_DIR/posh/agent/sock)
ssh-add -l                       # must list the SAME fingerprint as locally
ssh -T git@github.com            # or: git ls-remote <a key-only repo> — a real auth over the forwarded key
```

- [ ] Inside the session, `ssh-add -l` lists the **same** fingerprint your local
      agent holds — the remote `ssh-add` is talking to your local agent through
      posh. (`-A`/`--forward-agent` forces it on and warns if no agent is found;
      `-a`/`--no-forward-agent` or `POSH_FORWARD_AGENT=no` disables it.)
- [ ] A real operation authenticates: `ssh -T git@github.com` greets you by name,
      or a `git ls-remote`/`git push` against a key-only repo succeeds.
- [ ] Diagnostic: `Ctrl-^` → **Show agent-forwarding debug info**. Two lines
      composite over the session —
      `agent-fwd: on sock=<local agent> peer-advertised=yes channels=N …` and
      `server: endpoint=up channels=N next_chan=M symlink=ok`. Run `ssh-add -l`
      again while watching: `channels` ticks up for the in-flight request.
- [ ] Roam: switch networks (toggle wifi/VPN) mid-session, then `ssh-add -l`
      again — still lists the key (the agent socket is stable across the roam).
- [ ] Forwarded once: open a SECOND `$P otherhost:dev`; both sessions' `ssh-add
      -l` work, and `agent/sock` points at the newest connection (its diagnostic
      shows `symlink=ok`; the older connection keeps serving).

Reading the diagnostic when it is NOT working:

- `peer-advertised=no` — the server is not forwarding at all (most common
  misconfig: server-side `POSH_FORWARD_AGENT=no`, or no local agent was found so
  the client never advertised).
- `server: endpoint=down` — server-side forwarding is off (or an older server).
- `server: … symlink=broken` — another posh server stole the well-known symlink.

## Known gaps — do not file as new bugs

None currently. The Wave D/E gaps that used to live here — wheel scroll
(#28), remote suspend (#30), BEL/OSC 52 forwarding (#27), kitty graphics
over remote (#29), connect diagnostics (#31) — were all fixed as of
2026-06-10 ([#34](https://github.com/amarbel-llc/posh/issues/34)). A
failure in any of those areas is a regression: file it.

For a guided capability pass (instead of ad-hoc probes), run
[posht](posht.md): `just run-posht` locally, `just run-posht <host>`
through the whole posh pipeline.
