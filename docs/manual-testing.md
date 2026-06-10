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
- [ ] Reattach replays the screen: prior output, cursor position, modes.
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
$P user@otherhost                # mosh-style: bare args containing @ . or :
$P ssh otherhost                 # explicit form (for bare ssh aliases)
```

Both run `posh-server new` over ssh. Server-host requirements:
`posh-server` on the **non-interactive ssh PATH** (the nix package installs
it next to posh; for a cargo build, symlink `target/release/posh` to
`~/.local/bin/posh-server`) — otherwise the wrapper reports "did not find
posh server startup message" — and UDP 60001–60999 reachable.

- [ ] Session comes up; typing survives suspending the laptop / switching
      networks (roaming).
- [ ] "Last contact N seconds ago" banner appears ~6.5s after cutting the
      network, clears on reconnect.

## Known gaps — do not file as new bugs

Tracked Wave D/E work
([#34](https://github.com/amarbel-llc/posh/issues/34)):

- Wheel scrolling under kitty sprays arrow keys at a prompt
  ([#28](https://github.com/amarbel-llc/posh/issues/28)).
- No Ctrl-Z suspend of the *remote* client
  ([#30](https://github.com/amarbel-llc/posh/issues/30)).
- Remote BEL / OSC 52 clipboard not forwarded
  ([#27](https://github.com/amarbel-llc/posh/issues/27)).
- Kitty graphics lost over remote sync and attach replay
  ([#29](https://github.com/amarbel-llc/posh/issues/29)).
- No connect/timeout diagnostics — a firewalled port waits silently
  ([#31](https://github.com/amarbel-llc/posh/issues/31)).
