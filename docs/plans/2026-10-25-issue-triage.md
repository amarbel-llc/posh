# Open-issue triage, 2026-10-25

135 open issues, read by four read-only reviewers (forge reads and code reads
only; nothing was built or run, because a fleet build lock was in force). Each
row says what was checked. "Not code-verified" means the class rests on the
issue text and comments alone. Evidence cited as `file:line` was seen in the
worktree at the time of triage and may have moved.

Classes: **a** fixable now, test-first, no operator decision; **b** needs a
decision (options and a recommendation); **c** blocked (on what); **d** stale,
only where the premise was checked and found gone.

Caveat: several issue bodies were truncated by the tool, so for those the
reviewer read the head, the tail and the comments. Those rows say so.

## a. Fixable now (smallest risk first)

| # | size/risk | what, and what was checked |
|---|---|---|
| 175 | S / low | `docs/features/0011` lines 92, 95, 136, 137, 190 name `--workers` and `posh list --format json`; `main.rs:1878` has only `--json`/`-j`. Cheap fix: edit the doc to match the CLI. |
| 207 | S / low | `doc/posh.1.scd` has no `version` entry. `posh version` output format not verified. |
| 149 | S / none | `remote/diag.rs:427,458,491,558` held a real-looking tailnet address in test fixtures; swapped for `192.0.2.7` (documentation range). Done. |
| 92 | S / none | Palette "Reset & resync" is absent from the command-palette FDR (0009); RFC 0006 documents `CLIENT_FLAG_RESYNC`. Docs only. |
| 68 | S / low | No FDR for `POSH_DEBUG_LOG`; docs-only. The issue allows "too small for an FDR". |
| 88 | S / low | `zz-pocs/charm-tui-poc` still exists and nothing outside it references it. Check the justfile and flake for its recipes before deleting. |
| 244 | S / low | `backed_off` (`agent.rs:1120`) and `history_resend_after` (`daemon.rs:835`) duplicate the doubling. |
| 242 | S / low | `switch_route_target` (`daemon.rs:1868`) never checks `initialized()`, which exists and is used at 1354 and 1525. Test first: an un-Inited probe at a higher index. |
| 234 | S / low | `queue_frame` (`daemon.rs:941`) takes the snapshot by value and drops it when there is no producer. |
| 232 | S / low | Test-only refactor of about 12 tests in `remote/server.rs` (count not verified). |
| 230 | S / low | `test_client_conn()` exists (`daemon.rs:3446`); about six struct literals remain (3581, 3626, 4246, 4533, 5173, 5384) plus `baseline_conn` (3489). |
| 237 | S / low | Stop cloning the acked dump and snapshot in `producer.rs`. Not benchmarked. |
| 169 | S / low | `s.queued.push(bytes)` (`mux.rs:1992`) has no cap. Add a byte cap with drop-oldest; the cap size is a small choice. |
| 154 | S / low-med | `main.rs:1491` accepts only "0"; `agent.rs:1856` only "no"/"0"; `util.rs:99` has `parse_default_on_gate`. The man-page ENVIRONMENT text moves in the same commit. |
| 148 | S / low | `AgentOp` (`agent.rs:1767-1773`) has only `Other(u8)` besides two named ops; FDR 0004 notice contract changes. Wording of the issue not fully read. |
| 124 | S / low | No `NO_COLOR`/`dumb` handling anywhere in `crates`; change goes in `cmd_list`'s `pretty` gate. Recommend plain fallback for `TERM=dumb`, uncoloured for `NO_COLOR`. |
| 69 | S / low | Three `#[allow(clippy::too_many_arguments)]` at `stats.rs:905,925,945`; introduce gauge structs. |
| 219 | S / low | `posh-build/src/lib.rs` re-runs only for `version.env` (line 53) and `POSH_GIT_SHA` (line 78); nothing watches `.git/HEAD`. Awkward to test-first; verify by build, commit, rebuild. |
| 153 | S-M / low | `MuxFrameBuffer` and `FrameBuffer` duplication not re-read. |
| 112 | S-M / low | `channel_open_data_close_lifecycle` (`agent.rs:2547`) flaky; replace a wall-clock wait with synchronisation. Test body not read. |
| 192 | S-M / low | `Render:` palette entries and an RFC 0005 `render.set`. The `apply_echo_model` seam was not verified. |
| 168 | S-M / low | Record the mux fallback reason; add an About/transport line. |
| 204 | M / low-med | `main.rs` parses `--kind`; the remote branch drops it. Split relay-argv half from the M2 OPEN wire half. |
| 165 | M / low-med | `-a` plus `POSH_AGENT_EXPORT` on detached spawn. Not code-verified. |
| 80 | M / low | posht (Go) alt-screen walk mode with a fixed hold. |
| 70 | M / medium | Loopback smoke for `POSH_DEBUG_LOG`; tty and global `LOGGER` obstacles. |
| 113 | M / low | `signal_integration.rs:716` failed consistently per the issue; the test may have changed, so reproduce first. Same family as #249 (c). |
| 245 | M / medium | `screen.rs:249-251` pops scrollback rows on a grow without lowering `scrollback_total`. Issue recommends an add-only posh-term API. |
| 228, 229 | M / medium, M-L / medium | Two `#[ignore]`d red tests each (`dump.rs:1608,1633` and `1707,1756`). Same replay branch; fix together. Unblocks #236 and #233. |
| 190 | M / medium | Surface ssh stderr, treat a mux first-frame timeout as not-established, wait while `reconnecting`. Not code-verified. |
| 191 | M / medium | Perf; `just debug-perf-echo` exists. Needs before/after numbers. |
| 212 | M / medium | `MuxConnState::Draining` is never assigned (`mux.rs:457-479, 3125, 5404`). Short design decision first (FDR 0017 variant lifecycle), then test-first drain. |
| 197, 163 | S-M / low | Investigations with repros described; #197 is sequenced after the FDR 0016 auto-pop fix (landing not checked), #163 after the #162 probe verdict (not checked). |
| 210 | L / med-high | `CAP_SESSION_SIZE` is not in `crates/`. ADR 0006 and RFC 0012 decide it. Wire change: RFC 0001 row. |
| 188 | L / high | Daemon-side Scrollback2; follow-up to #181. |
| 86 | S first step | Add a posh-term test with `\x1b[38;2;153;153;153m` split across `process()` calls (existing test not checked). Otherwise b. |
| 101 | M / low or b | Changes the RFC 0007 contract; treat as a decision unless asked. |

## b. Needs a decision

| # | options and recommendation |
|---|---|
| 255 | The issue's own comment says all 9 gaps are done on this branch. **Close at merge.** Code not independently verified. |
| 248 | Still present: `scrollview.rs:161`. Fix (a) or (b). **(b): give the bar its own row** (changes one pinned literal on purpose). |
| 241 | `conformist.nix` has only nixfmt and shfmt. **Add rustfmt** and land one reformat commit when no branch is in flight, or document the gap. |
| 235 | Merge the two dump calls into one returning bytes and shape. **Do it before the posh-term API is frozen.** |
| 226 | Add a reason record before dropping the backlog (like `Tag::ExitCause`), or report "daemon link closed" without a cause. **Recommend the first.** |
| 224 | Read-only observer attach; authorisation needs a principal (spinclass FDR 0032, spinclass#347). |
| 221, 220 | Modal vs pop/announce for "lost contact"; app-menu contributions. The app-modal prototype is partial prior art. Pairs with `docs/plans/2026-10-25-countdown-decisions.md`. |
| 218 | Last-screen under the exit notice; the issue itself wants an FDR 0016 amendment first. |
| 213 | Grace period then hard cutover; decide after #164 gives a migration path. |
| 205 | Needs `ui.update` plus an RFC 0005 row; decide polling vs push carrier. |
| 202 | Move only the newer `POSH START` line, or defer. |
| 199 | Options (a) non-blocking ensure, (b) pump the modal inside blocking waits. **Recommend (b).** (c) painter thread is ruled out by the double fork. |
| 194 | Three asks; the empty-stack behaviour (open picker or exit) needs a call. Related to spinclass#270. |
| 174, 167, 121, 87 | Large refactors (Predictor/Renderer, EchoAck fold, `server_loop` extraction, shared client input core). Each needs a go/no-go. |
| 253, 222, 250 | Lossy-link agent and handshake behaviour; need design and a repro. #253 and #222 probably share a root. |
| 252 | Nix-darwin firewall module probably lives in another repo; decide the owner first. |
| 251 | Recommend option 3 (document and quieten) now, option 1 (forward terminfo) later. |
| 247 | Which flood-state fields to show; RFC 0014 key-order amendment. |
| 231 | Exploration: construct the orphan spacer-cell state, then prove or fix. |
| 171, 164, 159, 209, 214 | Umbrella, tracking or design issues; no direct action. |
| 166, 64, 104, 115, 146, 145, 142 | Explore or protocol-design issues. #146 should stay an explore issue. |
| 152 | The issue's own comment says the machinery is permanent; **close as superseded** or retire the `POSH_MUX=0` opt-out. |
| 151 | Implement as proposed with `{target}` default; operator confirms the default. Cross-repo (clown#192); pairs with #114. |
| 150 | RFC 0001:259 vs shipped `CAP_METRICS`. **Soften the RFC band language** rather than burn a wire id. |
| 139 | Issue proposes closing once ADR 0006 is accepted. Check the ADR status, then close. |
| 138, 105 | Socket-dir resolution split and deep-`$TMPDIR` overflow (`session/mod.rs:41-59,114-120`). Decide together. |
| 129 | Ctrl-^ disambiguation delay: timeout or different key. |
| 128 | `CAP_KITTY_KEYBOARD` appears implemented (RFC 0010, `caps.rs`, `daemon.rs:1533`); operator verifies Shift+Enter, then closes. |
| 122 | Split or close. |
| 116 | Reverses the documented inert-`0.0.0` convention; operator decision. |
| 114 | Daemon does not persist OSC title; decide with #151. |
| 109 | Watchdog and `FLAG_WEDGE` still exist and #117 wired them to recovery; keep-or-remove. |
| 107 | Issue says "accept for now"; close as accepted unless a fix is wanted. |
| 103 | Host-global agent rendezvous; depends on #53, #54. |
| 89 | Keystroke-to-echo gauge design open; lower priority. |
| 83 | Last comment says the freeze self-heals via the default-on watchdog; **close in favour of #117** after daily-use confirmation. |
| 78, 77 | Reference renders doc; kitty graphics over posh (needs a capability and an RFC; support at all?). |
| 61, 56, 55, 53, 46, 45, 44, 36 | Old epics and promotions: reconcile FDR 0004 status vs FDR 0014 `stable` (#55), whether `setenv` is still wanted (#53; no `Setenv` in `crates`), phase 5 given `zz-mosh` is gone (#61), FDR 0002/0003 promotions are operator-run (#45, #46), the unsafe-Rust decision (#36). |
| 40 | `POSH_SERVER_CMD` covers the premise (`sshwrap.rs:269-302`); add a `--server` alias or close. |
| 28, 3 | `POSH_GRAB_MOUSE` shipped (default-off). Close both, or make the grab default-on or conditional. |

## c. Blocked

| # | on what |
|---|---|
| 254 | Next extraction waits for a second modal kind (the issue's own comment). |
| 249 | Flaky once in a nix build; needs a second occurrence. |
| 246 | A longer-flood harness or a field measurement. |
| 240, 238 | Stage 7 (retire v1) and Stage 6 (local attach on pacing). Both could be closed. |
| 236, 233 | #229/#228 fix. |
| 225 | Umbrella; remaining work is in `docs/plans/2026-10-05-flood-delivery.md`. |
| 223 | No repro or version recorded. |
| 211, 196, 189, 187 | Field recurrence or verification. |
| 170 | clown, then spinclass, migrating to `posh attach --create`. |
| 161 | Looks superseded by #196 and #165; confirm closure before closing. |
| 133 | macOS hardware. |
| 117, 90, 91 | Live re-test of a reportedly fixed wedge; #91 needs a captured session. |
| 81, 79, 76, 75 | CellDelta, #80 repro, RFC 0009 implementation, #102. |
| 39 | A manual kitty-vs-posh run. |
| 34 | Campaign complete; close after a manual smoke on a second host. |

## d. Stale

| # | evidence |
|---|---|
| 84 | `CLAUDE.md` says there is no committed `conformist.toml`; the repo root has `conformist.nix` and no `conformist.toml` (directory listing). |
| 2 | The test belonged to the vendored `zz-mosh`, removed by ADR 0009; no `zz-mosh` at the repo root. |
| 160 | No `mephisto` in `flake.nix` or `flake.lock` (rg, docs excluded); the four hits are in `predict/evolved.rs`. Git history not checked. |

**Probably stale, not verified:** #42 (TERM/COLORTERM forwarding exists at `sshwrap.rs:365-387` with a test at `:1612-1650`, but Bubble Tea backgrounds were not seen to render) and #41 (`remote/agent.rs` exists and FDR 0014 is `stable`, but not every point of the issue was checked). Confirm with a manual repro before closing.

## Progress (later on 2026-10-25)

Done and closed: #42, #41 (closed as addressed, with the caveats in their
closing comments), #255 (landed at `81010ef`), #207, #149, #242, #92, #230,
#175 (docs now match the CLI). Partly done: #154 (`POSH_RELAY` and
`POSH_AGENT_NOTICE` only; the `POSH_WEDGE_*` gates still do not trim or
case-fold). Decided against for now: #254 (the four panel-width copies have
different policies; the issue says to wait for a second modal kind) and #234
(legacy clients only; about 40 call sites). #225 gained a red test for the case
Stage 1 documents as unfixed (`posh225_a_viewport_larger_than_the_session_...`,
`#[ignore]`d; one 4 KiB chunk queues 906,363 bytes for a 40x132 viewport on a
24x80 session).

## Order of work for (a)

Doc and fixture fixes first (#175, #207, #149, #92, #68), then small
test-first code (#242, #244, #234, #230, #124, #154, #169), then #228 with #229.
Nothing is started: builds are barred until the fleet lock lifts.
