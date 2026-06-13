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

_POSH_PREDICTION_
	Local-echo prediction display: _always_, _never_, _adaptive_
	(default), or _experimental_. The mosh _MOSH_PREDICTION_DISPLAY_
	analog.

_POSH_PREDICTION_OVERWRITE_
	When set, predictions overwrite the character under the cursor
	instead of inserting.

_POSH_GRAB_MOUSE_
	_on_/_off_ (default _off_): when on, grab the wheel on the outer
	terminal at a bare prompt and translate wheel up/down into arrow
	keys, normalizing scroll behavior across terminals. Costs the
	outer terminal's click-to-select while active.

_POSH_NO_TERM_INIT_
	Suppress the alternate-screen takeover (set by *posh*(1)'s
	*--no-init*).

_POSH_DEBUG_LOG_
	When set to a writable path, the client appends periodic one-line
	performance summaries — smoothed RTT, RTO, send interval, received
	frame counts (full/diff/empty), download bandwidth, prediction
	state, and render activity (writes, bytes, idle skips) — plus a
	final line on exit. Rotated at 5 MB. Unset disables logging.

# SEE ALSO

*posh*(1), *posh-server*(1), *posh*(7), *mosh-client*(1)

# COPYING

posh is free software, licensed under the GNU General Public License
version 3 or later.
