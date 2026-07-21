posh-client(1)

# NAME

posh-client - local endpoint of a posh roaming session

# SYNOPSIS

*posh client* [*-4*|*-6*] _host_ _port_

# DESCRIPTION

*posh-client* is the local half of a posh roaming connection (the
mosh-client analog). It connects to a *posh-server*(1) over the
encrypted UDP transport, renders the server's screen state, echoes
keystrokes speculatively (local prediction), and reconnects
automatically across IP changes and sleep/resume.

It is reached as the *client* subcommand of *posh*(1). Users rarely
invoke it directly: bare *posh* _host_ and *posh* _host_*:*_session_
run the ssh bootstrap (*posh ssh*), which starts a *posh-server*(1) on
the host and then launches this client against the reported address.

When the remote *posh-server*(1)'s organic wedge watchdog detects a session
stall — its terminal advanced but no frame was produced for longer than the
stall threshold (see _POSH_WEDGE_CAPTURE_ in *posh-server*(1)) — it signals
that on the wire and the client raises a sticky one-line banner over the
session: _session stall detected - captured to server log_. The banner is
sticky so a fast self-recovery does not rob you of the notice; it is dismissed
by the next keystroke.

# OPTIONS

*-4*, *-6*
	Force IPv4 or IPv6 when resolving _host_. The default chooses
	automatically.

# KEYS

*Ctrl-^*
	Open the command palette — the escape menu. It lists every client-local
	escape command: the predictive-echo models (_adaptive_, _optimistic_,
	_always_, _never_, plus the evolved-predictor pilot's _controller_ and
		_from-scratch_, RFC 0007), the client *and* server debug-logging toggles
	(_POSH_DEBUG_LOG_; the server toggle rides the transport), shell-out on the
	server (FDR 0008, the _POSH_ESCAPE_CMD_ command), reset & resync, dump wedge
	forensics, show wedge debug info (an on-screen transport-state summary,
	readable even while the session is frozen, plus a full dump to the
	diagnostic sink), show connection health (the transport-liveness view behind
	the "Last contact" banner — the age since the last frame, whether the banner
	is showing and its threshold, frame-arrival counts by kind including
	heartbeats, the largest inter-arrival gap and how many gaps crossed the
	banner threshold, and SRTT/RTO/send-interval — for diagnosing a *disconnect*
	that a healthy link should not have reported), show echo prediction stats (the local-echo outcome
	gauges plus, for the evolved GP models, the live evolution state — RFC
	0007: generations, population and fitness-window sizes, the champion's
	rank/size and whether it or the adaptive shadow is displayed, and the
	champion hyphence-doc record under
	_$XDG_DATA_HOME/posh/predict/champions/_), show agent-forwarding debug info (the
		SSH agent-forwarding state on both the client and server ends, FDR 0004), suspend the client, and quit. Type to filter, arrows to
	choose, Enter to run, Esc to
	dismiss. Rendered by the *posh-palette* helper over a JSON-RPC control
	channel; the session greys behind it.

	If that helper cannot be launched, *Ctrl-^* instead arms a one-shot
	fallback: *Ctrl-^* then *.* quits, and *Ctrl-^* then *Ctrl-^* (or *^*)
	sends a literal *Ctrl-^* to the remote.

# ENVIRONMENT

_POSH_KEY_
	The base64 session key, as printed by *posh-server*(1) in its
	*POSH CONNECT* line. The client reads it from the environment and
	immediately clears the variable, so the key never appears in the
	process's argv (which is world-readable via *ps*(1)). Required.

_POSH_PREDICTION_MODEL_
	Local-echo prediction model: _always_, _never_, _adaptive_
	(default), _experimental_, or _optimistic_. The first four are the
	mosh _MOSH_PREDICTION_DISPLAY_ analog; _optimistic_ (FDR 0006) echoes
	keystrokes immediately and lets the next paint correct them, gated on
	the remote PTY's ECHO flag and alt-screen. _POSH_PREDICTION_ is a
	deprecated alias, honored when _POSH_PREDICTION_MODEL_ is unset.

_POSH_PREDICTION_RENDER_
	How predicted cells are painted, independent of the model: _replace_
	(default; the model's own look — replace the glyph, underline on a
	slow link) or _dim_ (replace the glyph and mark it faint). The render
	style and the model are separate experimentation axes.

_POSH_PREDICTION_OVERWRITE_
	When set, predictions overwrite the character under the cursor
	instead of inserting.

_POSH_FRAMESYNC_
	Server→client screen-sync codec (experimental, #15): _morph_ opts into
	the incremental path — the server ships a minimal forward escape-delta
	and the client applies it to its existing screen model, instead of
	re-parsing a full screen dump every frame (far cheaper on large/wide
	terminals). Anything else (unset/empty/_dumpdiff_, the default) keeps the
	full-dump path. Negotiated with the server, which falls back to the
	full-dump path for any client that does not request _morph_, so the wire
	is unchanged by default.

_POSH_GRAB_MOUSE_
	_on_/_off_ (default _off_): at a bare prompt the wheel scrolls the
	session's primary-screen scrollback locally (FDR 0005) — instant,
	frozen while scrolled, with a top status-bar indicator; any keystroke
	or scrolling back to the bottom resumes the live view. When _on_, the
	wheel is instead translated into arrow keys (the legacy cross-terminal
	scroll normalization, posh#50). Both grab the wheel from the outer
	terminal, costing its click-to-select while active; a session app that
	tracks the mouse itself (vim, tmux) keeps the wheel either way.

_POSH_NO_TERM_INIT_
	Suppress the alternate-screen takeover (set by *posh*(1)'s
	*--no-init*).

_POSH_DEBUG_LOG_
	When set to a writable path, the client appends periodic one-line
	performance summaries — smoothed RTT, RTO, send interval, received
	frame counts (full/diff/empty), download bandwidth, prediction
	state, render activity (writes, bytes, idle skips), and per-frame
	compute timing as _avg/max_ microseconds (full-dump re-parse
	_apply_us_ and render _compose_us_; the max is the worst single frame
	in the interval), keystroke→consumed input latency (_input_ms_, the
	lag you feel), and event-loop timing (_loop iters=… busy=…us idle=…us
	busy_pct=…% max_iter_us=…_, the stall / busy-CPU view) — plus _#wedge_
	breadcrumbs and a final line on exit. All timings are windowed per
	interval. Rotated at 5 MB. The on-demand *SIGUSR2* state dump
	(*SIGNALS*) is written to this same sink. Unset disables logging.

_POSH_WEDGE_WATCHDOG_
	Controls the apply-stall watchdog (*on by default*). If the visible
	model stays frozen past a threshold while diff frames keep arriving —
	the apply-stall signature — the client auto-captures a wedge forensic
	bundle, arms the diagnostic sink, and forces a resync to break the
	stall, once per episode. Forensic bundles contain *raw screen bytes*
	(whatever was on the terminal at capture time) and, like the
	diagnostic log, are created private (mode 0600). The RFC 0006 base-integrity check and the
	auto-escalate on an undecodable diff recover the common cases; this is
	the uniform net over the silent-drop apply paths those miss (a stale or
	base-mismatched frame dropped without recovery, a transport-side
	stall). Set to _0_, _off_, _false_, or _no_ to disable. Setting it
	*explicitly* on also counts as a debug posture (see *SIGNALS*).
	Independent of _POSH_DEBUG_LOG_.

# SIGNALS

*SIGUSR2*
	Append a one-line snapshot of live transport state to the diagnostic
	sink: peer (server) address, last-send age, applied frame number,
	input-outbox base and pending bytes, accumulated scrollback depth,
	smoothed RTT/RTO and send interval, byte counters, prediction gauges,
	terminal size, and the remote echo state. A trailing _link=_ group carries
	the transport-liveness view behind the "Last contact" banner — whether the
	banner is showing (_late_), the largest frame inter-arrival gap
	(_gap_max_), how many gaps crossed the banner threshold (_late_gaps_), the
	received-frame total, the heartbeat count, and the retransmit count — so a
	*disconnect* a healthy link should not have reported can be told apart from
	a genuinely dead peer. In a debug posture
	(_POSH_DEBUG_LOG_ set, or _POSH_WEDGE_WATCHDOG_ *explicitly* on — its
	default-on state does not count) the client also requests
	the server's own transport state, which then rides each frame and appears
	as a trailing _srv=_ field — the server's pid (so a wedged client maps to
	the exact remote posh-server log), frame number, ack, frames outstanding,
	terminal generation, pty-open flag, and an _agent=_ block: _off_ when the
	server is not forwarding, else its live channel count, the CUMULATIVE
	channels-opened count, symlink health, and the agent stream's
	sent/queued/resent byte totals. Two things read out of that block:
	channels-opened climbing on an idle connection means something is opening
	agent channels nobody asked for, and _resent_ is what cumulative-only
	acknowledgement costs — the unacked tail rides every message until acked.
	*just debug-posh-agent* parses exactly this. This is the far side
	of a wedge that a remote *posh-server*(1) cannot be *SIGUSR2*'d for
	directly: server _num_ climbing while the client's _applied_num_ is frozen
	is an apply-stall, both frozen points at the server or the path. Written
	to _$POSH_DEBUG_LOG_
	when set, otherwise to a per-pid default file
	_<rundir>/posh-client-<pid>.log_ (the same _<rundir>_ resolution as
	*posh-server*(1)). The dump always goes to a file, never the terminal,
	so it never disturbs the live display; like the server's it needs no
	startup flag, so it introspects an already-running client when a
	session has wedged.

	When a visible-frame body is currently being rejected by the apply path
	(an apply-stall: the diff's base matches the applied frame but cannot be
	reconstructed, so the screen freezes), the dump also writes a byte-level
	_forensic bundle_ beside the log — _posh-forensic-client-<pid>-<num>.txt_
	carrying a verdict line, plus _.applied_ and _.diff_ raw bytes for an
	offline diff re-run. The bundle is also written automatically on the first
	such rejection per episode, and on demand via the *Dump wedge forensics*
	palette command.

*SIGWINCH*
	Re-read the terminal size and repaint at the new geometry.

*SIGTERM*, *SIGINT*, *SIGHUP*
	Wind the session down through the normal shutdown handshake,
	restoring the tty.

*SIGCONT*
	Repaint from scratch after the client is resumed; the outer terminal
	state is unknown after *SIGSTOP* or fg.

# SEE ALSO

*posh*(1), *posh-server*(1), *posh*(7), *mosh-client*(1)

# COPYING

posh is free software, licensed under the GNU General Public License
version 3 or later.
