posh-server(1)

# NAME

posh-server - remote endpoint of a posh roaming session

# SYNOPSIS

*posh-server* *new* [*-p* _PORT_[*:*_PORT2_]] [*-4*|*-6*] [*--* _command_...]

*posh server* [*new*] [*-p* _PORT_[*:*_PORT2_]] [*-4*|*-6*] [*--* _command_...]

# DESCRIPTION

*posh-server* is the host-side half of a posh roaming connection (the
mosh-server analog). It binds a UDP port, prints a connect line on
stdout, daemonizes into the background, and runs a command in a PTY
whose screen state it serves to a *posh-client*(1) over the encrypted,
roaming transport.

It is the same binary as *posh*(1): the package installs
_bin/posh-server_ as a symlink to _posh_, and when invoked under that
name every argument belongs to the server subcommand
(*posh-server new -p ...* is exactly *posh server new -p ...*). The
*new* verb is accepted for mosh-server parity and may be omitted.

Users rarely run *posh-server* by hand. *posh*(1)'s ssh bootstrap runs
it on the remote host: bare *posh* _host_ and *posh* _host_*:*_session_
ssh in, exec *posh-server new*, and parse its connect line to start the
local client. The remote host only needs *posh-server* on its
non-interactive PATH.

# OPTIONS

*-p*, *--port* _PORT_[*:*_PORT2_]
	Bind a single UDP port, or the first free port in the inclusive
	range _PORT_..._PORT2_. Default: _60001:60999_.

*-4*, *-6*
	Force IPv4 or IPv6. The default binds dual-stack when possible.

*--* _command_...
	Run _command_ in the PTY instead of the default _$SHELL_.
	Everything after *--* is the command and its arguments.

# HANDSHAKE

On startup the server prints, on stdout, before backgrounding:

```
POSH CONNECT <port> <key>
```

where _<key>_ is the base64 session key. When the bootstrap ssh knows
the connecting address (from _$SSH_CONNECTION_) it also prints a
*POSH IP* _<addr>_ line. The client reads the key from the environment
(_$POSH_KEY_), never the command line — see *posh-client*(1).

# ENVIRONMENT

_POSH_SERVER_NETWORK_TMOUT_
	Exit after this many seconds with no datagram from the client
	(0 = never, the default). The mosh _MOSH_SERVER_NETWORK_TMOUT_
	analog. A live client is otherwise waited for indefinitely so a
	roaming session survives long disconnections.

_POSH_SERVER_SIGNAL_TMOUT_
	On *SIGUSR1*, exit if the client has been silent for this many
	seconds (0 = never, the default). The mosh
	_MOSH_SERVER_SIGNAL_TMOUT_ analog: a supervisor sends SIGUSR1 to
	ask idle servers to wind down.

_POSH_ESCAPE_CMD_
	The command the escape-to-shell overlay runs (FDR 0008), spawned in
	the session's working directory when the client picks *Shell out* from
	its *Ctrl-^* command palette. Whitespace-split into argv (so _sc exec_
	works); unset or blank runs _$SHELL_ as a login shell, the same default
	as the session. In an eng/spinclass environment this is set to _sc exec_,
	which resolves the worktree session from the cwd and loads its devshell.
	For the bare *posh* _host:session_ form it is forwarded from the client
	(like _POSH_DEBUG_LOG_), so to turn the feature on you set it once
	locally — _export POSH_ESCAPE_CMD='sc exec'_ — and it takes effect on the
	remote server. See *posh-client*(1).

_POSH_DEBUG_LOG_
	When set to a writable path, the server appends periodic one-line
	performance summaries — smoothed RTT, RTO, transmitted frame counts
	(full/diff/empty), diff-saved percentage, _dump_vt_ time as
	_avg/max_ microseconds (the max is the worst single serialization in
	that interval), upload bandwidth, outstanding-frame count,
	retransmissions, and event-loop timing (_loop iters=… busy=…us
	idle=…us busy_pct=…% max_iter_us=…_, the busy/idle/stall view) — plus a
	final line on exit. All timings are windowed per interval, so a spike
	shows up in the second it happened. Rotated at 5 MB. Since
	the server has detached and redirected its standard streams to
	_/dev/null_, this file is its only diagnostic sink. For the bare
	*posh* _host:session_ form the path is forwarded from the client, so
	it is interpreted on the remote host. The on-demand *SIGUSR2* state
	dump (*SIGNALS*) is written to this same sink. Unset disables logging
	at startup, but a connected client can turn the summaries on or off at
	runtime from its *Ctrl-^* command palette (*Server debug logging*); when
	enabled that way without _POSH_DEBUG_LOG_ set, the sink is the same
	per-pid default file the *SIGUSR2* dump uses. See *posh-client*(1).

# SIGNALS

*SIGUSR1*
	Idle-exit trigger. With _$POSH_SERVER_SIGNAL_TMOUT_ set the server
	exits if the client has been silent that long (mosh-server parity);
	otherwise it is taken and ignored.

*SIGUSR2*
	Append a one-line snapshot of live transport state to the diagnostic
	sink: peer address, has-remote and peer-active, last-heard and last-
	send ages, current vs. acked frame, outstanding frames, smoothed
	RTT/RTO and send interval, byte counters, terminal generation, and
	whether the PTY is still open. Written to _$POSH_DEBUG_LOG_ when set,
	otherwise to a per-pid default file _<rundir>/posh-server-<pid>.log_,
	where _<rundir>_ is _$POSH_DIR_, else _$XDG_RUNTIME_DIR/posh_, else a
	_/tmp_ fallback. Unlike the periodic _$POSH_DEBUG_LOG_ summary it
	needs no startup flag, so it introspects an already-running server
	when a session has wedged: the peer address and last-heard age
	distinguish a roam the server has not re-pinned from one-way packet
	loss or a dead client.

*SIGHUP*
	Ignored, so a hangup on the bootstrap ssh channel never reaps the
	detached session.

# SEE ALSO

*posh*(1), *posh-client*(1), *posh*(7), *mosh-server*(1), *ssh*(1)

# COPYING

posh is free software, licensed under the GNU General Public License
version 3 or later.
