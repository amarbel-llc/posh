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

# OPTIONS

*-4*, *-6*
	Force IPv4 or IPv6 when resolving _host_. The default chooses
	automatically.

# KEYS

*Ctrl-^* then *.*
	Quit the client and close the connection. Press *Ctrl-^* twice to
	send a literal *Ctrl-^* to the remote.

*Ctrl-^* then *Ctrl-Z*
	Suspend the client (background it); resume with the shell's job
	control. The remote session keeps running.

*Ctrl-^* then *s*
	Open a transient shell on the *server* in the session's working
	directory (FDR 0008), overlaying the live session until that shell
	exits — handy for a quick command alongside a full-screen session
	app. The sub-key is _POSH_ESCAPE_KEY_ (default _s_); the command is
	the server's _POSH_ESCAPE_CMD_ (default the login shell). See
	*posh-server*(1).

*Ctrl-^* then *e*
	Cycle the local predictive-echo model live (_adaptive_, _optimistic_,
	_always_, _never_) without reconnecting; a banner shows the new model.
	Overrides _POSH_PREDICTION_MODEL_ for the rest of the session.

*Ctrl-^* then *d*
	Toggle client-side debug logging at runtime (the _POSH_DEBUG_LOG_
	stream). When turned on without _POSH_DEBUG_LOG_ set, periodic transport
	summaries are written to a per-pid file under the posh runtime dir; a
	banner shows the state and path.

*Ctrl-^* then *p*
	Open the command palette: a filterable overlay listing the client-local
	controls (the echo models and logging toggle above, plus quit). Type to
	filter, arrows to choose, Enter to run, Esc to dismiss. Rendered by the
	*posh-palette* helper over a JSON-RPC control channel; if that binary is
	not found the palette is unavailable and a banner says so. The direct
	chords keep working regardless.

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

_POSH_ESCAPE_KEY_
	The single sub-key, pressed after *Ctrl-^*, that opens an
	escape-to-shell overlay (FDR 0008); default _s_. The command it runs
	is chosen on the server by _POSH_ESCAPE_CMD_ — see *posh-server*(1).
	Avoid _._ and _^_, which the *Ctrl-^* menu already uses.

_POSH_DEBUG_LOG_
	When set to a writable path, the client appends periodic one-line
	performance summaries — smoothed RTT, RTO, send interval, received
	frame counts (full/diff/empty), download bandwidth, prediction
	state, render activity (writes, bytes, idle skips), and per-frame
	compute timing (full-dump re-parse and render compose, in
	microseconds) — plus a final line on exit. Rotated at 5 MB. The
	on-demand *SIGUSR2* state dump (*SIGNALS*) is written to this same
	sink. Unset disables logging.

# SIGNALS

*SIGUSR2*
	Append a one-line snapshot of live transport state to the diagnostic
	sink: peer (server) address, last-send age, applied frame number,
	input-outbox base and pending bytes, accumulated scrollback depth,
	smoothed RTT/RTO and send interval, byte counters, prediction gauges,
	terminal size, and the remote echo state. Written to _$POSH_DEBUG_LOG_
	when set, otherwise to a per-pid default file
	_<rundir>/posh-client-<pid>.log_ (the same _<rundir>_ resolution as
	*posh-server*(1)). The dump always goes to a file, never the terminal,
	so it never disturbs the live display; like the server's it needs no
	startup flag, so it introspects an already-running client when a
	session has wedged.

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
