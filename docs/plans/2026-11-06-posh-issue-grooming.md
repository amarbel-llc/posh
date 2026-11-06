# posh issue grooming and cleanup sweep (2026-11-06)

Read-only pass over every open posh issue (123 at the time) plus a cleanup
sweep of the tree, at master `bdd5b29`. Each issue's body and comments were
read and checked against code and git history. Builds on the 2026-10-25
triage (`docs/plans/2026-10-25-issue-triage.md`), which several verdicts here
correct.

Nothing in the tracker was changed. Every close, retitle, relabel and merge
below is a **proposal for the operator**. Verdicts that rest on inference
rather than a commit say so ("likely", "uncertain"). No new issues were
filed: the candidate bugs at the end are unverified, so they are listed for a
ruling instead.

Verdicts:

- **FIXED**: done on master; the commit is cited.
- **STALE**: the premise is gone.
- **DUPLICATE**: the same as the issue cited.
- **READY**: workable now; scoped with an acceptance test, files, size (S < 1 h, M half a day, L several days) and whether it needs a ruling.
- **TRACKING / EXPLORE / NEEDS-INFO / NEEDS-DECISION**: not workable as-is.

## 1. Proposed closes

| # | verdict | evidence |
|---|---|---|
| 256 | FIXED | `500215e`: recording notices name the client host the `.castx` is written on |
| 187 | FIXED | `91a55db`: SessionResume fixes input dropped across a mux reconnect; RFC 0015 |
| 188 | FIXED (M2) / relay half STALE | `35f080a` carries SCROLLBACK2 through the M2 bridge; ADR 0007 makes the relay do-not-extend |
| 170 | FIXED (main scope) | `2f5308c` strict attach. The zsh `ph` completion leftover belongs in a new issue |
| 161 | FIXED (foreground) | `fa1794f`, `92bc0a5`, `6a0ae1f`. #165 is the detached residual |
| 160 | FIXED | `5b719ac`, `dfe4f10`: mephisto replaced by galapagos |
| 152 | FIXED | `582fd16`: repoint on release (shape c); test at `agent.rs:2284` |
| 117 | FIXED | `daf7976`, `d328c89`, `7980bb4`. The RFC 0009/0002 status bookkeeping moves to #188's successor |
| 113 | FIXED, DUPLICATE of #111 | `5e9d7fc`: hermetic `start_server`. Not the same failure as #249 |
| 90 | FIXED | `90763f9` client RESYNC on apply-stall; the daemon variant was fixed under #157 |
| 83 | FIXED | Diagnosed as the #117 class; same commits |
| 84 | STALE | No committed `conformist.toml` (`conformist.nix:8-11`) |
| 75 | FIXED | `f7e50c7`: posh-proto and `poshterity/src/framereplay.rs`; follow-up #102 was won't-do |
| 61 | STALE | Migration target `zz-mosh` removed by ADR 0009; RFC 0003 exists |
| 56 | FIXED | Phases 1-4 closed; `poshterity/src/{castx,record,player,assert,golden}.rs` |
| 55 | FIXED | FDR 0004 records phase 1 landed; FDR 0014 is `stable` |
| 40 | FIXED (premise) | `POSH_SERVER_CMD` (`sshwrap.rs:277-313`). Keep only if a `--server` alias is wanted (S) |
| 34 | TRACKING, done | Campaign complete; residuals are #36, #2 and #3 |
| 28 | FIXED | `ca510e4` (DECSET 1007) plus the FDR 0005 wheel intercept (`scrollview.rs:18-27`). A quick kitty check is prudent |
| 3 | FIXED / not posh | Plain-ssh behaviour is kitty's; inside posh it is the FDR 0005 intercept |
| 2 | STALE | The `make check` lane went with `zz-mosh` (ADR 0009) |

Close after one live check:

- **#114**: bare reattach shows the inner program's title (`dump.rs:370` emits OSC 0 in every dump).
- **#133**: one macOS run of the Darwin branches at `justfile:1628,1701,1857-1885,1906`.
- **#128**: Shift+Enter over `ph host:session`. Fixed locally by `38f2465` and `8647ff8`, but no `CAP_KITTY_KEYBOARD` appears under `remote/`; see candidate 6.
- **#129**: re-test; a lone ESC is forwarded unheld (`session/client.rs:2200-2216`).
- **#189**: the fleet shows `base_history=` absorbing RESYNCs (`2923b5d`).
- **#159**: close once the M2 promotion date is recorded in the design-doc header (candidate 4).

### Close list (prepared, not run)

The comments to post when the operator confirms. Every cited sha was
re-checked on 2026-11-06 with `git log -1 <sha>`.

Fixed by a named commit:

| # | close comment |
|---|---|
| 256 | Fixed by 500215e: recording notices now name the client host the `.castx` is written on. |
| 187 | Fixed by 91a55db: the SessionResume cursor carries input across a mux reconnect (RFC 0015). |
| 188 | Fixed on M2 by 35f080a, which carries SCROLLBACK2 through the bridge. The relay half is superseded: ADR 0007 makes the relay do-not-extend. |
| 170 | Fixed by 2f5308c: bare `posh attach` is strict (Phase B). The zsh `ph` completion is left over for a separate issue. |
| 161 | Fixed for foreground sessions by fa1794f (stable-path export + bootstrap `-a`), with 92bc0a5 and 6a0ae1f. #165 tracks the detached case. |
| 160 | Fixed by 5b719ac (mephisto replaced by public galapagos) and dfe4f10. |
| 152 | Fixed by 582fd16: repoint-on-release (shape c), now permanent under FDR 0014. |
| 117 | Fixed by daf7976, d328c89 and 7980bb4. The RFC 0009/0002 status bookkeeping moves to #188's successor. |
| 113 | Fixed by 5e9d7fc (hermetic `start_server`); duplicate of #111. #249 is a different failure and stays open. |
| 90 | Fixed by 90763f9 (client RESYNC on apply-stall); the daemon variant was fixed under #157. |
| 83 | Fixed by daf7976, d328c89 and 7980bb4: diagnosed as the #117 wedge class. |
| 75 | Fixed by f7e50c7 (posh-proto plus `poshterity/src/framereplay.rs`); the follow-up #102 was won't-do. |
| 28 | Fixed by ca510e4 (DECSET 1007), with the FDR 0005 wheel intercept. A quick kitty check is prudent before closing. |

Done or stale, closed on evidence rather than one commit:

| # | close comment |
|---|---|
| 56 | Done: phases 1-4 are closed and poshterity ships `castx`, `record`, `player`, `assert` and `golden`; phase 5 (#61) is stale. |
| 55 | Done: FDR 0004 records phase 1 as landed, and FDR 0014 (stable endpoint) is `stable`. |
| 61 | Stale: the migration target `zz-mosh` was removed by ADR 0009; the `.castx` RFC exists as RFC 0003. |
| 84 | Stale: `conformist.toml` is no longer committed; `conformist.nix` is the single source. |
| 2 | Stale: the `make check` lane went away with `zz-mosh` (ADR 0009). |
| 3 | Not posh: over plain ssh this is kitty's behaviour. Inside posh sessions it is handled by the FDR 0005 wheel intercept. |
| 40 | Solved by `POSH_SERVER_CMD`; reopen if a `--server` flag alias is wanted. |
| 34 | The campaign is complete; its residuals are tracked in #36, and #2 and #3 are closed above. |

## 2. Retitle, narrow, merge, relabel

- **#174**: narrow to items 4-6 plus the perf evaluation. `683cc6a` did items 1-3 and 7.
- **#171**: narrow to item (1), the daemon stamping `CAP_EXIT_STATUS`.
- **#194**: narrow to items 2-3. Item 1 landed in `68055e8`.
- **#190**: narrow to defect 3. Defects 1-2 landed in `e4594eb`.
- **#76**: narrow to findings 5-6. Link #240 and #245: finding 3 has the same root as #245, and 2 and 4 go away when v1 retires.
- **#44**: tick item 1 (TERM/COLORTERM, `sshwrap.rs:397`) and item 4 (`POSH_SERVER_CMD`). Split or demote the rest into FDR 0003 Limitations so #46 can move.
- **#249**: cross-link to #113 (same test, different symptom); keep #249 as needs-info.
- **#250**: keep as the umbrella with both 2026-11-04 plans linked, or split follow-ups 2-4 into three issues. `7c032c5` (`POSH_SSH_SHARE`) fixed item 1.
- **Relabel tracking**: #225, #240, #238, #254, #214, #209, #196, #164, #139, #103, #81, #46, #45.
- **Relabel explore**: #231, #233, #246, #222, #220, #191, #166, #146, #145, #115, #104, #89, #64.
- **Close-or-park as won't-fix**: #234 (triage decided against it).

## 3. Ready to work

No ruling needed, smallest first:

| # | scope | size |
|---|---|---|
| 88 | delete `zz-pocs/charm-tui-poc` (unreferenced); acceptance: `just` green | S |
| 232 | shared send/receive helper for `remote/server.rs` tests (about a dozen copies) | S |
| 236 | option 1: `2*mirror_rows - rows` at `posh-term/src/dump.rs:267-275` | S |
| 112 | replace the 20 ms sleeps (`remote/agent.rs:2576,2597`) with a bounded poll | S |
| 69 | gauge structs for `stats.rs:902,922,942` (`too_many_arguments`) | S |
| 78 | posht reference-render text for 16/256/truecolor (`posht/tests_visual.go`) | S |
| 148 | named mutating `AgentOp` variants (`remote/agent.rs:1767`) | S |
| 153 | one generic frame reassembler (`remote/mux.rs:382` vs `session/ipc.rs:238`; header endianness as a parameter) | S |
| 163 | **now NEEDS-DECISION.** A loopback test (kept in the session scratchpad, not committed) showed a *connected* UDP socket gets ECONNREFUSED from a closed port. But `Connection::client` (`datagram.rs`) never `connect()`s; it uses `send_to`, and Linux reports ICMP errors only to connected sockets, so the error surfaces on neither send nor recv. Ruling: connect the client socket, or use `IP_RECVERR`. The `mux.rs` comment calling it a "connected UDP socket" is wrong and should be fixed with that ruling | — |
| 165 | `-a` + `POSH_AGENT_EXPORT` on detached spawn (`sshwrap.rs:987-1021`; `remote_command` at `:1602` is the model) | S |
| 192 | palette `render.set` (seam at `remote/client.rs:188-189`) plus an RFC 0005 row | S |
| 219 | `rerun-if-changed` on HEAD, ref and index in `posh-build/src/lib.rs` (resolve a worktree's `.git` file) | S |
| 86 | probe: feed `\x1b[38;2;153;153;153m` split at every byte into posh-term; if green, needs a field repro or closes | S |
| 235 | one call returning a frame dump's shape and bytes (`dump.rs:308,325`, `daemon.rs:631`), before the API freezes | S-M |
| 237 | stop cloning in `encode_visible` (`posh-proto/src/framesync/producer.rs:204-208` etc.) | S-M |
| 168 | (About half landed, `Refs #168`; the heading marker still needs a ruling) About shows "requested mux, riding UDP (reason)" (`main.rs:2017-2031`, `remote/client.rs:724`); ruling only on the heading marker | S-M |
| 245 | **now NEEDS-DECISION.** Option 2 changes wire semantics. For v2, the sender must skip popped rows when they re-scroll, which changes the RFC 0009 re-anchor rule and `evicted_upto`; it also risks losing rows the program overwrote in the meantime. For v1, treating a height grow as a reflow contradicts `reset_scrollback_floors_on_reflow` (a paced client still holds unshipped rows) and needs an RFC 0002 §4 statement | — |
| 228 + 229 | un-ignore the four `dump_vt` tests (`dump.rs:1608,1633,1707,1756`) on one branch; unblocks #236 option 2 and #233 | M-L |
| 70 | `POSH_DEBUG_LOG` loopback smoke, modelled on `just debug-agent-e2e` | M |
| 80 | posht page-per-test alt-screen walk; unblocks #79 | M |
| 212 | the drain half: nothing ever assigns `MuxConnState::Draining` (`mux.rs:469,479`) | M |
| 210 | `CAP_SESSION_SIZE` per ADR 0006 / RFC 0012; first step `ClientConn::mirror_geometry()` | L |

Ready, but need a small ruling first:

- **#169**: cap the M2 gap-time `queued` (`remote/mux.rs:2015`). Drop-oldest or coalesce? The issue's precondition ("before M2 goes default-on") has already passed, so priority should go up. S.
- **#248**: (a) or (b); triage recommends (b). The fix changes a pinned literal on purpose. S-M.
- **#226**: a reason record before closing a backlog-dropped client (`session/daemon.rs:2269-2279`; `Tag::ExitCause` is prior art). M.
- **#109**: remove the server wedge watchdog and `FLAG_WEDGE`. It is a default-on diagnostic plus a wire flag (an RFC 0001 row); recovery lives in the client watchdog and is unaffected. M.
- **#151**: outer-title fallback `{group}/{session}`; ruling on the knob name and default. S-M.
- **#204**: carry `--kind` (`main.rs:411-426`, `remote_session_argv`). Confirm the scope is M2 plus inner argv, since ADR 0007 freezes the relay. M.
- **#205**: extend `ui.update` (`posh-palette/main.go:795-800`) with picker rows. Decide the carrier. A notification has no `-32601`, so the issue's fallback design needs revising. M.
- **#105**: the clearer error is S and needs no ruling; the `/tmp` fallback goes with #138.
- **#247**: needs an RFC 0014 §4.2 key-order amendment; sequenced after flood Stage 7 Task 7.1. M.
- **#199**: close as mitigated by #198, or pump the spinner inside the local IPC waits (`mux.rs:2290,2367-2376`). S-M.

## 4. Decisions only the operator can make

- **Refactor go/no-go**: #121 (`server_loop`, about 2000 lines), #87 (shared client core), #167 (EchoAck drive).
- **Policy and design**:
  - #36: rustix yes or no.
  - #53: Setenv still wanted, or superseded by #103?
  - #77: kitty graphics forwarding (L, needs a capability and an RFC).
  - #116: keep the inert `0.0.0` crate version convention?
  - #150: give `CAP_METRICS` a released id, or soften the RFC 0001 band wording?
  - #138: socket-dir derivation.
  - #142: SACK for agent channels.
  - #202: handshake grammar.
  - #213: hard deprecation (after FDR 0017).
  - #218, #221: modal designs.
  - #224: observer attach.
  - #241: add rustfmt to `conformist.nix` (one reformat commit while no branch is in flight), or document the gap. AGENTS.md currently reads as if `lint-fmt` checks Rust.
  - #251: docs now, forward terminfo later.
  - #252: which repo owns a nix-darwin module.
  - #253: forwarded-agent outage UX.
  - #101: park while `always` is the default.
  - #107: fold into #188's successor or close as accepted.
  - ADR 0006: unblocks #139 and moves #210.
- **Needs a live repro or capture**: #223, #197, #222, #196, #91, #79, #39 (`just run-posht '' --only resize` both ways), #249.

### Correction to the 2026-11-04 follow-ups plan (#190, #211)

The Finding 2 outline in `docs/plans/2026-11-04-connect-followups-2-4.md`
drops the client's 15 s first-frame deadline once the endpoint reports
`Connected`, and keeps it while the endpoint is `Reconnecting`. Grooming
shows two conflicts:

- **#211**: the endpoint reported `connected` while opens went unanswered. After Finding 2, such a recurrence would wait out the daemon's OPEN budget (up to 60 s) before falling back, instead of 15 s.
- **#190**, defect 3: it asks for the opposite on `Reconnecting`, so that a riding client waits rather than falling back.

Finding 2 should not land until the operator rules on both. A bounded wait
under `Connected` (for example, the remaining OPEN budget capped well under
60 s) would keep most of the win.

## 5. Cleanup sweep

Highest value first. Every item is local and mechanical unless noted.

1. **Stale `#[allow(dead_code)]` with "until callers land" comments, though callers now exist**:
   - `posh-proto/src/caps.rs:43-59` and `:687,704,804,825,845,855,865` (agent caps and codecs; callers in `relay.rs`, `client.rs`, `server.rs`)
   - `remote/sync.rs:827-982` (agent record types)
   - `remote/agent.rs:1859` (`AgentNotice::from_env`)
   - `remote/client.rs:1595` (`last_metrics`)
2. **Module-wide `#![allow(dead_code)]` that masks whole modules**, to narrow item by item: `remote/relay.rs:82`, `remote/hostmetrics.rs:13`, `remote/predict/metric.rs:24`, `remote/predict/species.rs:23`, `remote/predict/evolved.rs:18`. The `evolved.rs:16-17` "not wired" comment conflicts with `predict/mod.rs:548`, where `ControllerPredictor` is live. Also `predict/mod.rs:40,49` (`unused_imports`, "once the GP is wired"). Uncertain how much of it is still really dead.
3. **`util::Error::kind()`** (`util.rs:32`) has no non-test caller found. Uncertain; a candidate for deletion.
4. **RFC 0007 status drift**: `exploring` ("no implementation expected"), yet the controller ships (`predict/mod.rs:548`, the galapagos dependency). Move it to `experimental`. Also `docs/decisions/0001-*.md` uses `exploring` on an ADR, outside the ADR vocabulary (uncertain).
5. **Gate-parser duplication**:
   - Five one-line wrappers over `util::parse_default_on_gate` (`util.rs:99`): `session/mod.rs:29`, `remote/mux.rs:91`, `remote/agent.rs:1145`, `main.rs:1499`, `remote/sshwrap.rs:706`. `mux.rs:112` already calls util directly.
   - Three copies of the opt-in predicate: `session/modal.rs:55`, `session/client.rs:1320` (identical bodies), and `remote/sshwrap.rs:357` (no trim).
   - Fold them into one opt-in helper in `util`.
6. **Retired `POSH_SESSION_FRAMES` leftovers**:
   - Comments at `remote/mux.rs:88,5975`, `remote/agent.rs:1143`, `session/client.rs:639` and `session/daemon.rs:1262,4112`.
   - `session/client.rs:1318` cites `parse_frames_gate`, which no longer exists.
7. **Undocumented env knobs**: `POSH_COALESCE` (`session/client.rs:1312`), `POSH_SERVER_CMD` (`remote/sshwrap.rs:312`), `POSH_PREDICTION_CONTROLLER_ECHO` (`remote/predict/evolved.rs:411`) and `POSH_PALETTE` (`remote/palette.rs:91`). Document them in posh(1)/posh-client(1) or mark them internal. `POSH_RECORD_FILE` (`main.rs:218`) looks like an internal hand-off.
8. **Doc references**:
   - `doc/posh-client.1.scd:205` cites `#15` for `POSH_FRAMESYNC`; it should be #81.
   - `remote/stats.rs:434` refers to the server's `POSH_WEDGE_CAPTURE`; update it with #109.
   - `remote/relay.rs:220-229` has a doc comment fused onto the wrong item (`content_caps` at `:246` has none).
9. **Untracked TODOs** (convert to `TODO(#NNN):` or delete): `remote/predict/species.rs:117`, `remote/predict/metric.rs:269`, `remote/predict/evolved.rs:473`. No `TODO(#NNN)` exists anywhere yet.
10. **Similar private-dir hardening**: `remote/mux.rs:131` (`mux_dir_at`) and `session/mod.rs:80-109`. A shared `ensure_private_subdir` would cover both.
11. **`macOS gap (posh#214):` inventory**: 11 sites (`session/confine.rs:164`, `session/daemon.rs:7272`, `remote/hostmetrics.rs:114,142,168`, `pty.rs:104,111,652`, `remote/server.rs:3667`, `tests/session_integration.rs:47`, `justfile:1758`). Not checked against #214's own list.

## 6. Candidate bugs with no issue (not filed: unverified)

1. **Kitty images in every frame dump** (uncertain; the posh#225 class). `dump_vt_impl` calls `dump_graphics` on both paths (`posh-term/src/dump.rs:401,474`), and that re-sends every stored image (`:554-596`; store quota 320 MB, `graphics.rs:11`). The outer renderer never draws them (#77). It is unknown whether the DumpDiff/Morph codecs hide the cost in steady state; a Full keyframe would carry it.
2. **Missing scrollback rows ship as blanks** (`unwrap_or_default`) at `remote/server.rs:2259`, `session/daemon.rs:1228` and `remote/history.rs:333`. Tracked only inside #76's body; it should fail loudly.
3. **Recordings can vanish on logout** (inference). `.castx` files go to the socket base (`remote/diag.rs:282-288`), which is `$XDG_RUNTIME_DIR/posh`, a tmpfs on systemd hosts. This is one reading of #256 that `500215e` did not address.
4. **Design-doc drift**: `docs/plans/2026-07-28-connection-mux-endpoint-design.md:16-24` says M2 is opt-in, but code is default-on (`remote/mux.rs:111-112`).
5. **RFC drift**: `ui.update` is a live palette control method (`posh-palette/main.go:795-800`, `remote/palette.rs:260-263`) and is absent from RFC 0005. #205 would build on an unspecified method.
6. **Possible remote kitty-keyboard gap** (uncertain). No `CAP_KITTY_KEYBOARD` reference under `remote/`, while the daemon suppresses kitty replies unless every attached client advertises it (`session/daemon.rs:1533-1534`). It holds only if the relay or M2 bridge does not pass the far client's capability table through.
7. **`clear_v2_row_space`** (`remote/client.rs:2690`) sets `scroll_offset = 0` directly rather than via `set_scroll`, so the live-render memo is not reset. This is flood-plan queue row 24; unverified.
