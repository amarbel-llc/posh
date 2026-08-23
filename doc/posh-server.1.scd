posh-server(1)

# NAME

posh-server - remote endpoint of a posh roaming session

# SYNOPSIS

*posh-server* *new* [*-p* _PORT_[*:*_PORT2_]] [*-4*|*-6*] [*-A*] [*--channels*] [*--* _command_...]

*posh server* [*new*] [*-p* _PORT_[*:*_PORT2_]] [*-4*|*-6*] [*-A*] [*--channels*] [*--* _command_...]

*posh-server* [*-p* _PORT_[*:*_PORT2_]] [*-4*|*-6*] *agent* *--client-id* _ID_ [*--channels*]

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

*-A*, *--forward-agent*
	Enable SSH agent forwarding (FDR 0004). The *posh*(1) ssh bootstrap
	appends this exactly when the client resolved forwarding on; it is
	not normally typed by hand. The server then binds a per-pid agent
	socket, claims the stable _<session-dir>/agent/sock_ symlink, and
	exports _SSH_AUTH_SOCK_ into the session shell. The forwarding
	*policy* (which socket, opt-out) lives entirely in the client; the
	server only learns the outcome.

*--channels*
	Speak the RFC 0011 channel-envelope protocol on the datagram
	transport: the session stream and each forwarded agent connection
	ride their own multiplexed channels. Appended by the *posh*(1) ssh
	bootstrap exactly when the client selected the protocol
	(_$POSH_CHANNELS_); not normally typed by hand. Without it the
	server speaks the baseline (unenveloped) protocol, byte-identical
	to prior releases.

*--* _command_...
	Run _command_ in the PTY instead of the default _$SHELL_.
	Everything after *--* is the command and its arguments.

*agent* *--client-id* _ID_
	Run the per-destination mux remote peer: agent channels (FDR 0014)
	plus, since M2, a channel table relaying each wire session channel
	to its named session daemon under the relay contract — one process
	per destination, no PTY of its own. *mux* is the M2 verb name;
	*agent* remains as the M1 alias (the zero-session case). The
	server prints the same connect handshake, detaches,
	binds the per-client-host socket _<base>/agent/mux-<ID>.sock_
	(recording its pid beside it), participates in the _agent/sock_
	election as a full sibling of the per-connection endpoints, and
	serves forwarded-agent connections as RFC 0011 *agent* channels
	over the enveloped transport. Bootstrapped by the *posh*(1)
	per-destination mux daemon (one per destination, governed by the
	client's default-on _$POSH_MUX_ gate); not normally typed by hand. Flags precede
	the verb, like *new*. It exits after a minute of peer silence — an
	agent-only connection with a never-returning peer has nothing left
	to serve. The rendezvous PERSISTS through that exit: with no active
	sibling, _agent/sock_ stays pointed at the deterministic
	_mux-<ID>.sock_, whose file and pid record stay on disk — a consumer
	during the outage gets a fast connect failure instead of a missing
	path, and the successor endpoint rebinds the same name in place, so
	the path heals with no relinking. Its lifecycle is journaled to a persistent, appending
	per-client-host log beside its socket,
	_<base>/agent/mux-<ID>.log_: startup, peer-silence and resume
	edges, _agent/sock_ claim/release transitions, and the exit reason
	(peer timeout vs. signal) — the remote-side timeline for a
	forwarded-agent outage post-mortem. It also answers introspection
	(RFC 0013): a daemon heartbeat requesting the server's identity
	gets back its build (version, git sha, pid, start time) on the
	same channel — what the client's *mux ls* shows as _remote=_ —
	and a local status socket beside the agent one,
	_<base>/agent/mux-<ID>.status.sock_, answers each connection with
	one status line (identity, peer, heard age, channel counts,
	_agent/sock_ ownership) and closes; the host's *posh list* reads
	it as the remote-endpoints section.

	*--client-id* _ID_ is required: it is the client-host identity the
	election reports and the socket is named after, restricted to
	letters, digits, dot, underscore, and dash (the client sends its
	sanitized hostname, or _$POSH_CLIENT_ID_). *--channels* is implied
	— agent channels exist
	only on the enveloped transport — and is tolerated on either side
	of the verb so the client builds one uniform bootstrap tail.

# HANDSHAKE

On startup the server prints, on stdout, before backgrounding:

```
POSH CONNECT <port> <key>
```

where _<key>_ is the base64 session key. When the bootstrap ssh knows
the connecting address (from _$SSH_CONNECTION_) it also prints a
*POSH IP* _<addr>_ line, and when it honored a _$POSH_AGENT_EXPORT_
request, a bare *POSH AGENT_EXPORT* line — both before *POSH CONNECT*,
which ends the client's parse; a client that does not know a *POSH* line
ignores it. The client reads the key from the environment
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

_POSH_AGENT_EXPORT_
	When truthy (*1*, *true*, *on*, *yes*), *new* (and its *relay* form)
	hands the session shell _SSH_AUTH_SOCK=<base>/agent/sock_ — the
	host's stable forwarded-agent path — WITHOUT standing up a
	per-connection endpoint of its own. Set by the client on the bootstrap
	command when its per-destination mux endpoint (FDR 0014, _$POSH_MUX_)
	owns forwarding for the invocation, so no *-A* arrives yet the session
	is still born pointing at the agent that endpoint serves; the path is
	valid the moment any endpoint claims it and survives endpoint respawns
	and wire reconnects underneath. An env prefix rather than a flag so a
	server predating it stays bootstrappable (it simply ignores the
	variable). A server that honors it prints *POSH AGENT_EXPORT* on the
	handshake stream before *POSH CONNECT* (see *HANDSHAKE*), so the
	client can tell; with *-A* present the request is redundant — the
	connection's own endpoint names the same path. Not forwarded from the
	client's environment; the client emits it itself, and the server
	scrubs it before spawning the session. The *mux*/*agent* peer seeds the
	same path into its own environment unconditionally (it owns the
	endpoint), so the sessions it creates for M2 session channels are born
	on it too.

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

_POSH_WEDGE_CAPTURE_
	Controls the organic wedge watchdog (on by default). The server
	continuously watches for its own terminal model advancing without a
	visible frame going out — a stall otherwise indistinguishable from an
	idle session by transport state. When that persists past an internal
	threshold it lazily opens the same per-pid diagnostic sink the
	*SIGUSR2* dump uses (unless _POSH_DEBUG_LOG_ already armed one) and
	records the frame-emission gate variables, once per stall episode, so
	an intermittent freeze is captured without pre-arming logging. Set to
	_0_, _off_, _false_, or _no_ to disable it if it ever misbehaves.
	Distinct from the client-side _POSH_WEDGE_WATCHDOG_ (see
	*posh-client*(1)), which is also on by default and acts on apply-stalls.

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
	loss or a dead client. An *agent* mux peer answers with its own
	shape — peer address, heard age, live and cumulative agent channel
	counts, whether it still owns _agent/sock_, and its session-channel
	count — appended to its persistent per-client-host log (see *agent*
	above).

*SIGHUP*
	Ignored, so a hangup on the bootstrap ssh channel never reaps the
	detached session.

# SEE ALSO

*posh*(1), *posh-client*(1), *posh*(7), *mosh-server*(1), *ssh*(1)

# COPYING

posh is free software, licensed under the GNU General Public License
version 3 or later.
