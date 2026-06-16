poshterity(1)

# NAME

poshterity - deterministic, step-ratcheted terminal recorder/replayer

# SYNOPSIS

*poshterity record* [*--out* _FILE_] [*--via* _posh_|_ssh_ *--host* _HOST_] *--* _cmd_ [_args_]...

*poshterity replay* _file_ [*--to-marker* _NAME_] [*--dump* _text_|_vt_|_flat_]

*poshterity step* _file_ (*--by* _unit_ [*--n* _N_] | *--to-marker* _NAME_) [*--frame-gap* _SECS_] [*--dump* _..._]

*poshterity bless* _file_ *--golden* _path_ [*--at* _MARKER_] [*--kind* _grid_|_vt_|_flat_]

*poshterity assert* _file_ *--golden* _path_ [*--at* _MARKER_] [*--kind* _..._] [*--check-emu-rev*]

*poshterity version*

# DESCRIPTION

*poshterity* records a terminal's output byte stream and replays it
through the in-process *posh-term* emulator, so a recorded program or
session can be re-rendered and asserted on deterministically — with no
live terminal and no timing to race. It is the deterministic replacement
for the *tmux capture-pane* + *sleep* pattern: a *.castx* recording
replays to a screen that is a pure function of its bytes.

The recording format, *.castx*, is a strict superset of the asciinema
*.cast* v2 format; any conforming *.cast* v2 file replays through
poshterity, and any *.castx* plays in *asciinema*. The format contract is
specified in RFC 0003.

The replay surface (*replay*, *step*, *bless*, *assert*, *version*) is
also reachable on the *posh* binary as *posh rec* _..._ — the two share
one implementation. *record* is *not* available that way: it needs a PTY
and lives only in the standalone *poshterity* binary.

# SUBCOMMANDS

*record* [*--out* _FILE_] [*--via* _posh_|_ssh_ *--host* _HOST_] *--* _cmd_ [_args_]...
	Spawn _cmd_ under a PTY and tee its output both to the terminal and
	to a *.castx* recording (default _--out recording.castx_). With
	*--via*/*--host* the command is run over a remote transport (a posh
	roaming session, or *ssh -t*), for capturing a recording through the
	full remote pipeline; the two flags are all-or-nothing.

*replay* _file_ [*--to-marker* _NAME_] [*--dump* _text_|_vt_|_flat_]
	Feed the whole recording through the emulator and print the final
	screen. *--to-marker* stops at the first marker named _NAME_.
	*--dump* selects the rendering: _text_ (plain, default), _vt_ (a full
	escape-stream reconstruction), or _flat_ (single active grid).

*step* _file_ (*--by* _unit_ [*--n* _N_] | *--to-marker* _NAME_) [*--frame-gap* _SECS_] [*--dump* _..._]
	Advance by discrete emulator-defined steps and print the
	intermediate screen. *--by* takes _byte_, _escape_, _write_,
	_change_, _frame_, or _marker_; *--n* repeats the unit _N_ times.
	*--frame-gap* sets the inter-frame gap (seconds) that delimits a
	_frame_ step.

*bless* _file_ *--golden* _path_ [*--at* _MARKER_] [*--kind* _grid_|_vt_|_flat_]
	Render the screen (optionally *--at* a named marker) and write it as
	a golden-frame snapshot to _path_. *--kind* selects the golden
	rendering (_grid_, default; _vt_; _flat_).

*assert* _file_ *--golden* _path_ [*--at* _MARKER_] [*--kind* _..._] [*--check-emu-rev*]
	Render the screen and compare it against the golden at _path_,
	exiting non-zero on a mismatch — the CI gate. *--check-emu-rev*
	additionally warns when the golden was blessed under a different
	emulator revision (see *EMU_REV* below); it never fails on that
	difference alone.

*version*
	Print *poshterity* _VERSION_ (_GIT_SHA_) and exit. The version flows
	from the repo's _version.env_; the git SHA from the build. See
	*eng-versioning*(7).

*help*
	Print usage and exit (also *-h*, *--help*).

# EMU_REV

Each recording's *.castx* header carries an _emu_rev_ string — the
*posh-term* emulator revision the recording was produced against, as
_version+git-sha_. A golden frame records the same value; *assert* with
*--check-emu-rev* compares the two and warns on a difference, so a golden
that needs regeneration after an emulator change is surfaced
deliberately rather than silently. The check is advisory: a mismatch
warns but does not fail the assertion. See RFC 0003 and
*eng-versioning*(7).

# EXAMPLES

Record a command and replay its final screen:

```
poshterity record --out run.castx -- ls --color=always /etc
poshterity replay run.castx
```

Bless a golden at a marker, then gate on it in CI:

```
poshterity bless run.castx --golden run.grid --at prompt-ready
poshterity assert run.castx --golden run.grid --at prompt-ready --check-emu-rev
```

# SEE ALSO

*posh*(1), *posh*(7), *asciinema*(1)

# COPYING

poshterity is free software, licensed under the GNU General Public
License version 3 or later. It is built on the posh-term terminal
emulator.
