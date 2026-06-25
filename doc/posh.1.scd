posh(1)

# NAME

posh - persistent, roaming terminal sessions

# SYNOPSIS

*posh* [*-g* _GROUP_] _command_ [_args_]...

*posh* _name_

*posh* *:*[_group_/]_session_

*posh* [_user_@]_host_ [*--* _command_...]

*posh* [_user_@]_host_*:*[_group_/]_session_ [_command_...]

# DESCRIPTION

*posh* combines zmx-style local session persistence (a daemon per
session, reached over a Unix socket) with mosh-style roaming remote
sessions (an encrypted UDP transport that survives IP changes and
sleep/resume). A single scp-style namespace addresses everything
attachable; see *posh*(7) for the model.

With no arguments, *posh* lists the sessions in the current group
(equivalent to *posh list*).

A bare first argument that is not a known subcommand is resolved as an
attach target by the grammar in _TARGET GRAMMAR_ below.

# TARGET GRAMMAR

The first non-option argument resolves to one of four targets (RFC
0001 §1; the first rule that matches wins, and resolution never
errors — a malformed namespace form falls back to a plain host or
local-session name):

_name_
	A bare word with no _@_, _._, or _:_ is a *local session*:
	attach to it, creating it if needed. Shorthand for
	*posh attach* _name_.

*:*_session_, *:*_group_/_session_
	A leading colon is an *explicit local* session, optionally
	group-qualified.

\[_user_@]_host_
	An ssh destination (contains _@_ or _._, or is a bracketed IPv6
	literal, or has a trailing colon) is a *plain roaming shell* over
	the encrypted UDP transport. Shorthand for *posh ssh* _host_.

\[_user_@]_host_*:*[_group_/]_session_
	A *persistent session on a remote host*, transported over the
	roaming UDP connection: attach-or-create just like a local
	session. Persistence lives in the remote session daemon, so the
	transport survives roaming and the session survives detach. Use
	brackets for IPv6 hosts: *[fe80::1]:dev*.

# GLOBAL OPTIONS

*-g*, *--group* _GROUP_
	Session group (default: _default_, or _$POSH_GROUP_). Each group
	is a subdirectory of the socket directory.

*--no-init*
	Do not take over the terminal's alternate screen on
	attach/connect (mosh *--no-init* parity; also _$POSH_NO_TERM_INIT_).
	The takeover sequences normally come from terminfo _smcup_/_rmcup_
	for _$TERM_, falling back to DECSET 1049 when no database entry is
	found; a terminal whose entry defines no alternate screen is never
	taken over. See *posh*(7) and FDR 0002.

*-A*, *--forward-agent*
	Explicitly enable SSH agent forwarding for a roaming connection
	(both *posh* _host_ and *posh* _host_*:*_session_). The source
	socket resolves _$POSH_FORWARD_AGENT_ (when it names a path), then
	_$SSH_AUTH_SOCK_; *-A* overrides a _$POSH_FORWARD_AGENT_=*no*
	opt-out. Unlike the best-effort default, when neither resolves to a
	usable agent *-A* warns loudly and forwarding stays off, rather than
	passing silently. See FDR 0004.

*--forward-agent*=_PATH_
	Forward _PATH_ instead of _$SSH_AUTH_SOCK_ (a second agent,
	*gpg-agent*'s ssh socket, etc.). The path lives only in the local
	client; nothing about it reaches the wire or the remote host.

*-a*, *--no-forward-agent*
	Disable agent forwarding for this connection (the per-connection
	opt-out; _$POSH_FORWARD_AGENT_=*no* is the profile-level switch).

Agent forwarding is *on by default* on a roaming connection whenever a
usable _$SSH_AUTH_SOCK_ exists; with none, posh proceeds without it. On
the remote host the forwarded agent is reachable at the stable
_$SSH_AUTH_SOCK_=_<session-dir>/agent/sock_, valid across
detach/reattach. This diverges from OpenSSH's opt-in default — posh
targets are overwhelmingly your own hosts; the trust model is otherwise
identical to *ssh -A*, so prefer confirm-constrained keys (*ssh-add -c*)
for hosts you do not fully trust. *posh ssh* stays a thin *ssh* wrapper:
a *-A* there is the real *ssh* flag, not posh forwarding.

# SESSION COMMANDS

Local persistence over Unix sockets.

*attach* _name_ [_command_...] [*--detach*] (alias: *a*)
	Attach to a session, creating it (running _command_, default
	_$SHELL_) if needed. With *--detach*, ensure the session exists,
	print status, and exit without attaching. Detach key: *Ctrl-\\*.

*list* [*--short*] [*-j*|*--json*] (aliases: *ls*, *l*)
	List sessions in the group: name, pid, attached client count.
	*--short* prints names only; *--json* prints a machine-readable
	array. *posh list* _host_*:* lists sessions on a remote host
	(over a *BatchMode* ssh), printing each name host-prefixed so it
	pastes back as a target.

*run* _name_ [*--*] _command_... (alias: *r*)
	Send a command to a session (created if needed) without
	attaching. Reads the command from stdin when no arguments are
	given.

*fork* [_name_] (alias: *f*)
	Fork the current session (_$POSH_SESSION_) into a new detached
	session with the same command and working directory. Without a
	name, the first free _<current>-N_ is used.

*detach* [_name_] (alias: *d*)
	Detach all clients from the named session, or from the current
	session (_$POSH_SESSION_) when no name is given.

*detach-all* (alias: *da*)
	Detach all clients from all sessions in the group.

*kill* _name_ (alias: *k*)
	Kill a session, its shell, and all attached clients.

*groups* (alias: *gs*)
	List session groups that contain sessions.

*history* _name_ [*--vt*] (alias: *hi*)
	Print the session's scrollback. Plain text by default; *--vt*
	emits the escape stream that reconstructs the screen with
	attributes.

*completions* _shell_ (alias: *c*)
	Print the completion script for _bash_, _zsh_, or _fish_.

# REMOTE COMMANDS

Roaming over encrypted UDP. See *posh-server*(1) and *posh-client*(1)
for the two endpoints; most users never invoke these directly — bare
*posh* _host_ and *posh* _host_*:*_session_ drive them.

*ssh* [*-4*|*-6*] [*-p* _PORT_[*:*_PORT2_]] [_user_@]_host_ [*--* _command_...]
	Convenience wrapper (mosh-style; also reached as a bare
	*posh* [_user_@]_host_ when the host contains _@_, _._, or _:_):
	start *posh-server new* on the host via ssh (forwarding the
	locale environment — LANG and the LC_ variables — and the
	*-p*/*-4*/*-6* flags), then connect to the address the
	server reports. The remote host needs *posh-server*(1) on its
	non-interactive PATH. Survives IP changes and sleep/resume.

*server* [*new*] [*-p* _PORT_[*:*_PORT2_]] [*-4*|*-6*] [*--* _command_...]
	Start a remote server; see *posh-server*(1).

*client* [*-4*|*-6*] _host_ _port_
	Connect to a posh server; see *posh-client*(1).

# ENVIRONMENT

_POSH_DIR_
	Socket directory. Default: _$XDG_RUNTIME_DIR/posh_, then
	_$TMPDIR/posh-<uid>_, then _/tmp/posh-<uid>_.

_POSH_GROUP_
	Default session group (overridden by *-g*).

_POSH_SESSION_
	Set inside a session to its name; read by *fork* and *detach*.

_POSH_KEY_
	Remote session key, read by *posh client* (never passed on the
	command line). See *posh-client*(1).

_POSH_PREDICTION_MODEL_
	Local-echo prediction model: _always_, _never_, _adaptive_
	(default), _experimental_, or _optimistic_ (FDR 0006: echo
	immediately and correct on the next paint; ECHO/alt-screen gated).
	_POSH_PREDICTION_ is a deprecated alias. See *posh-client*(1).

_POSH_PREDICTION_RENDER_
	How predicted cells are painted, independent of the model: _replace_
	(default) or _dim_. See *posh-client*(1).

_POSH_PREDICTION_OVERWRITE_
	When set, predictions overwrite instead of inserting.

_POSH_FRAMESYNC_
	Server→client screen-sync codec (experimental): _morph_ for the
	incremental escape-delta path; the default re-parses a full screen
	dump each frame. See *posh-client*(1).

_POSH_GRAB_MOUSE_
	_on_/_off_ (default _off_): when on, the client grabs the wheel on
	the outer terminal at a bare prompt and turns wheel up/down into
	arrow keys, so scrolling behaves the same across terminals. Costs
	the outer terminal's click-to-select while active; apps that grab
	the mouse themselves are unaffected.

_POSH_NO_TERM_INIT_
	Set by *--no-init*; suppresses the alternate-screen takeover.

_POSH_FORWARD_AGENT_
	Agent-forwarding default for roaming connections (FDR 0004),
	overridden by *-A*/*-a*. *no* or *0* disables forwarding; any other
	value is a socket path to forward instead of _$SSH_AUTH_SOCK_. Unset
	means the best-effort default (forward _$SSH_AUTH_SOCK_ when set).

_POSH_AGENT_NOTICE_
	When agent forwarding is active, a rate-limited banner (at most one
	per minute) reports when the remote host opens a connection to your
	forwarded agent — the ambient signal that it is being used. Enabled
	by default; set to *no* or *0* to silence it.

_POSH_SERVER_NETWORK_TMOUT_
	Server exits after N seconds without client contact (0 = never,
	the default). See *posh-server*(1).

_POSH_SERVER_SIGNAL_TMOUT_
	On SIGUSR1, the server exits if the client has been silent for N
	seconds (0 = never, the default). See *posh-server*(1).

_POSH_DEBUG_LOG_
	Path to a performance-diagnostics log for the roaming remote
	transport. When set, *posh client* and *posh server* each append
	periodic one-line summaries (RTT, bandwidth, frame counts, full-vs-
	diff economics, prediction state, render activity, per-frame compute
	timing), rotated at 5 MB.
	For the bare _host:session_ form it is forwarded to the remote, so the
	server logs to that path on the remote host (failing closed if it is
	not writable there). The on-demand *SIGUSR2* dump (*SIGNALS*) shares
	this sink. Unset disables logging. See *posh-client*(1) and
	*posh-server*(1).

# SIGNALS

The roaming transport responds to two diagnostic signals, sent to the
*posh server* or *posh client* process (find server pids with *posh
list* on the host, or _ps_):

*SIGUSR1*
	Asks an idle server to wind down, gated by
	_$POSH_SERVER_SIGNAL_TMOUT_. See *posh-server*(1).

*SIGUSR2*
	Dumps a one-line snapshot of live transport state — peer address,
	last-heard and last-send ages, acked-vs-current frame, RTT — to the
	_$POSH_DEBUG_LOG_ sink or a per-pid default file. The on-demand way
	to introspect a wedged, already-running server or client without
	restarting it. See *posh-server*(1) and *posh-client*(1).

# EXAMPLES

Attach to (or create) a local session:

```
posh dev
```

Start a session on a remote host and pick it up later from another
machine — it keeps running across the detach and the network change:

```
laptop$ posh box:dev
laptop$ <Ctrl-\>            # detach
desktop$ posh box:dev      # same session, full replay
```

A plain roaming shell on a host (no persistence):

```
posh box
posh user@box -- htop
```

List local and remote sessions:

```
posh list
posh list box:
```

# SEE ALSO

*posh-server*(1), *posh-client*(1), *posh*(7), *ssh*(1), *mosh*(1)

# COPYING

posh is free software, licensed under the GNU General Public License
version 3 or later. It is a combined Rust rewrite of zmx (local session
persistence) and mosh (roaming remote terminal).
