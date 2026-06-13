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

_POSH_DEBUG_LOG_
	When set to a writable path, the server appends periodic one-line
	performance summaries — smoothed RTT, RTO, transmitted frame counts
	(full/diff/empty), diff-saved percentage, average _dump_vt_ time in
	microseconds, upload bandwidth, outstanding-frame count, and
	retransmissions — plus a final line on exit. Rotated at 5 MB. Since
	the server has detached and redirected its standard streams to
	_/dev/null_, this file is its only diagnostic sink. For the bare
	*posh* _host:session_ form the path is forwarded from the client, so
	it is interpreted on the remote host. Unset disables logging.

# SEE ALSO

*posh*(1), *posh-client*(1), *posh*(7), *mosh-server*(1), *ssh*(1)

# COPYING

posh is free software, licensed under the GNU General Public License
version 3 or later.
