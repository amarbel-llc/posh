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
	compute timing (full-dump re-parse and render compose, in
	microseconds) — plus a final line on exit. Rotated at 5 MB. Unset
	disables logging.

# SEE ALSO

*posh*(1), *posh-server*(1), *posh*(7), *mosh-client*(1)

# COPYING

posh is free software, licensed under the GNU General Public License
version 3 or later.
