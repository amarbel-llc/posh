---
status: experimental
date: 2026-06-11
promotion-criteria: every actionable item in the parity checklist
  (github #44) is either closed or explicitly demoted to a non-goal in
  this record, and a manual smoke pass over a real remote (roam across a
  network change, suspend/resume, quit) is indistinguishable from mosh
  for a daily mosh user.
---

# Mosh-parity remote roaming surface

## Problem Statement

posh's remote transport is a port of mosh, and its users arrive with
mosh muscle memory: the same escape keys, environment variables, wrapper
flags, and terminal etiquette are expected to work. Without a recorded
parity contract, every divergence gets re-discovered the hard way —
real gaps are mistaken for configuration errors, and deliberate
non-goals get re-litigated or filed as bugs. This record fixes the
contract: what posh mirrors from mosh, what it intentionally does
differently, and where the remaining gaps are tracked.

## Interface

The mosh surface posh mirrors today (audited against the vendored
`zz-mosh` tree, 2026-06-11 — github #44 has the full sweep):

| mosh | posh | notes |
|---|---|---|
| `mosh user@host` | `posh user@host` | ssh bootstrap, `POSH CONNECT <port> <key>` |
| `mosh-server new -p P[:P2]` | `posh-server new -p P[:P2]` | default UDP range 60001:60999 |
| `MOSH_KEY` | `POSH_KEY` | key in env, never argv |
| `MOSH_PREDICTION_DISPLAY` | `POSH_PREDICTION` | always/never/adaptive/experimental |
| `MOSH_PREDICTION_OVERWRITE` | `POSH_PREDICTION_OVERWRITE` | |
| `MOSH_SERVER_NETWORK_TMOUT` / `_SIGNAL_TMOUT` | `POSH_SERVER_*_TMOUT` | + SIGUSR1 |
| `--no-init` / `MOSH_NO_TERM_INIT` | `--no-init` / `POSH_NO_TERM_INIT` | FDR 0002 |
| smcup/rmcup via terminfo | same, built-in term(5) reader | FDR 0002 |
| Ctrl-^ quit sequence, Ctrl-^ Ctrl-Z suspend | same | escape key not yet configurable |
| `-4`/`-6` | `-4`/`-6` (auto ≈ prefer-inet) | `all`/`prefer-inet6` missing |
| locale forwarding (`-l NAME=VALUE`) | env-prefix on the remote command | equivalent mechanism |
| `MOSH IP` from `$SSH_CONNECTION` | `POSH IP` | the `remote` discovery mode only |

Where posh deliberately exceeds mosh: session exit-status propagation
(#18, RFC 0001 §3), alternate-screen fidelity in the emulator (mosh has
no alt screen at all — quitting vim under mosh leaves its last frame),
kitty keyboard and graphics protocols, persistent local sessions and the
unified `host:session` namespace, and `posh history`.

## Limitations

Known parity gaps, tracked as a living checklist in **github #44** (the
headline items, in rough priority order):

1. no client-side color count → server `-c` → `TERM` export for the
   spawned shell (root cause of #42);
2. remote-IP discovery is `$SSH_CONNECTION`-only — mosh's default
   `proxy` mode (ssh ProxyCommand) and `local` mode are missing, so
   round-robin DNS / jump hosts / ssh-config Hostname rewrites can dial
   the wrong address;
3. no server bind-address selection (`--bind-server`, `-s`/`-i`);
4. no `--ssh` override (pairs with #40 `--server`), no `--local`;
5. prediction is env-only (no `--predict`/`-a`/`-n`/`-o` flags);
6. escape/detach keys are hardcoded (no `MOSH_ESCAPE_KEY` equivalent);
7. no `[posh] ` window-title prefix / `*_TITLE_NOPREFIX` opt-out;
8. ssh runs without a pty (mosh uses `-n -tt`) — decide together with
   the #42 TERM fix;
9. renderer does not gate ECH/BCE/title on terminfo capabilities;
10. raw mode does not set IUTF8; assorted wrapper diagnostics.

Deliberate non-goals (do not file as bugs): utmp/utempter records,
syslog connection logging, motd/`.hushlogin` printing, the
"detached sessions" login warning (`posh list` / `posh list host:` is
the posh-native answer), `MOSH_CLIENT_PID`, the `mosh-server new -@`
getopt quirk, and mosh's SSP protobuf/zlib wire format (posh ships its
own dump_vt-based sync; the wire is already different and documented in
the README).

## More Information

- github #44 — the full audit checklist this record summarizes.
- FDR 0002 (`0002-terminal-takeover-and-restore.md`) — the takeover
  bracket and `--no-init`.
- RFC 0001 (`docs/rfcs/0001-target-grammar-and-capability-table.md`) —
  the namespace and capability table posh layers on top of the mosh
  model.
- `zz-mosh/` — the vendored mosh 1.4.0 reference tree the audit ran
  against; removed by ADR 0009 (upstream: mobile-shell/mosh, tag
  `mosh-1.4.0`).
