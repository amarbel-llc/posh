//! posh: persistent, roaming terminal sessions.
//!
//! Combines zmx-style local session persistence (daemon-per-session over
//! Unix sockets) with mosh-style roaming remote sessions (encrypted UDP).

mod completions;
mod pty;
mod remote;
mod session;
mod tailnet;
mod target;
mod terminfo;
mod util;

use remote::datagram::Family;
use session::{Config, ListFormat};
use util::{Error, Result};

// Flowed from version.env (POSH_VERSION) by build.rs; see eng-versioning(7).
const VERSION: &str = env!("POSH_VERSION");
// Git revision (short sha, "-dirty" when the tree was unclean at build), also
// flowed by build.rs — from the nix flake's rev, or `git` in a dev checkout.
const GIT_SHA: &str = env!("POSH_GIT_SHA");

fn main() {
    if let Err(e) = run() {
        eprintln!("posh: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // mosh-server parity: the package installs `bin/posh-server -> posh`;
    // invoked under that name every argument belongs to the server
    // subcommand (`posh-server new -p ...` == `posh server new -p ...`),
    // which is what the ssh bootstrap runs on the remote host.
    let invoked_as_server = std::env::args()
        .next()
        .as_deref()
        .map(std::path::Path::new)
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "posh-server");
    if invoked_as_server {
        return cmd_server(&argv);
    }

    let mut group = std::env::var("POSH_GROUP").unwrap_or_else(|_| "default".to_string());
    // SSH agent forwarding (FDR 0004): the client-side flag, highest precedence
    // in `resolve_forward_policy`. Only the roaming `host:session` path acts on
    // it; `posh ssh` passes a literal `-A` through to real ssh instead.
    let mut forward_flag = remote::agent::ForwardFlag::Unset;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-g" | "--group" => {
                group = argv
                    .get(i + 1)
                    .ok_or_else(|| Error::from("--group requires a value"))?
                    .clone();
                i += 2;
            }
            "-a" | "--no-forward-agent" => {
                forward_flag = remote::agent::ForwardFlag::Disable;
                i += 1;
            }
            "-A" | "--forward-agent" => {
                forward_flag = remote::agent::ForwardFlag::ExplicitOn;
                i += 1;
            }
            // Long-option-with-`=` only, so bare `-A host` never swallows the
            // target word (FDR 0004 Interface).
            arg if arg.starts_with("--forward-agent=") => {
                let path = arg.strip_prefix("--forward-agent=").unwrap();
                forward_flag = remote::agent::ForwardFlag::Path(path.into());
                i += 1;
            }
            "--no-init" => {
                // mosh --no-init parity: travels as an environment variable
                // (like MOSH_NO_TERM_INIT) so it reaches the attach/remote
                // client wherever the grammar dispatch lands.
                std::env::set_var("POSH_NO_TERM_INIT", "1");
                i += 1;
            }
            "--record" => {
                // Tee the session's PTY output into a .castx recording. Travels
                // as an env var (like --no-init) so it survives the daemon's
                // double-fork; daemon_main opens it. poshterity(1) replays it.
                let file = argv
                    .get(i + 1)
                    .ok_or_else(|| Error::from("--record requires a value"))?
                    .clone();
                std::env::set_var("POSH_RECORD_FILE", &file);
                i += 2;
            }
            _ => break,
        }
    }
    let rest = &argv[i..];

    let Some(command) = rest.first() else {
        return session::cmd_list(&Config::new(&group)?, ListFormat::Default);
    };
    let args = &rest[1..];

    match command.as_str() {
        "help" | "h" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        "version" | "v" | "-V" | "--version" => {
            println!("posh {VERSION} ({GIT_SHA})");
            Ok(())
        }
        "list" | "ls" | "l" => {
            // `posh list box:` — remote listing through the namespace
            // (RFC 0001 §1): a trailing-colon host runs the same query
            // completion uses, output prefixed so names paste back in. The
            // local `-g`/$POSH_GROUP scopes the remote probe (#66), so a
            // session in a non-default group on the remote is visible.
            if let Some(arg) = args.iter().find(|a| !a.starts_with('-')) {
                if arg.ends_with(':') {
                    if let target::Target::Host { user, host } = target::Target::parse(arg) {
                        return cmd_list_remote(user, host, &group);
                    }
                }
            }
            let format = if args.iter().any(|a| a == "--json" || a == "-j") {
                ListFormat::Json
            } else if args.iter().any(|a| a == "--short") {
                ListFormat::Short
            } else {
                ListFormat::Default
            };
            session::cmd_list(&Config::new(&group)?, format)
        }
        "attach" | "a" => cmd_attach(&group, args),
        "kill" | "k" => {
            let name = args
                .first()
                .ok_or_else(|| Error::from("kill requires a session name"))?;
            session::cmd_kill(&Config::new(&group)?, name)
        }
        "detach" | "d" => {
            session::cmd_detach(&Config::new(&group)?, args.first().map(|s| s.as_str()))
        }
        "detach-all" | "da" => session::cmd_detach_all(&Config::new(&group)?),
        "run" | "r" => {
            let name = args
                .first()
                .ok_or_else(|| Error::from("run requires a session name"))?;
            let mut cmd_args = &args[1..];
            if cmd_args.first().map(|s| s.as_str()) == Some("--") {
                cmd_args = &cmd_args[1..];
            }
            session::cmd_run(&Config::new(&group)?, name, cmd_args)
        }
        "fork" | "f" => session::cmd_fork(&Config::new(&group)?, args.first().map(|s| s.as_str())),
        "groups" | "gs" => session::cmd_groups(),
        // Tailnet peer names (MagicDNS), one per line — the completion source
        // for tab-completing tailscale hosts; empty/silent without tailscale.
        "tailnet" => {
            for name in tailnet::names() {
                println!("{name}");
            }
            Ok(())
        }
        "history" | "hi" => cmd_history(&group, args),
        "completions" | "c" => {
            let shell_arg = args
                .first()
                .ok_or_else(|| Error::from("usage: posh completions <bash|zsh|fish>"))?;
            let shell = completions::Shell::from_str(shell_arg)
                .ok_or_else(|| Error(format!("unknown shell {shell_arg} (bash, zsh, or fish)")))?;
            println!("{}", shell.script());
            Ok(())
        }
        "server" => cmd_server(args),
        "client" => cmd_client(args),
        "ssh" => cmd_ssh(args, None),
        // `posh rec ...` == the standalone `poshterity` binary: deterministic
        // recording replay (poshterity owns the logic; this is just an alias).
        "rec" => poshterity::cli::run(args).map_err(Error::from),
        name if !name.starts_with('-') => match target::Target::parse(name) {
            // Bare `posh <name>` attaches (creating the session if needed).
            target::Target::LocalSession { .. } => cmd_attach(&group, rest),
            // `posh :grp/dev` — explicit local, with optional group.
            target::Target::Local { group: g, session } => {
                let mut args = vec![session];
                args.extend_from_slice(&rest[1..]);
                cmd_attach(&g.unwrap_or(group), &args)
            }
            // mosh parity: `posh [user@]host [-- command...]` connects
            // remotely over ssh + encrypted UDP. This roaming path honors
            // agent forwarding (FDR 0004), like `host:session`.
            target::Target::Host { .. } => cmd_ssh(rest, Some(&forward_flag)),
            // `posh host:grp/dev` — persistent remote session over the
            // roaming transport (RFC 0001 §2).
            target::Target::RemoteSession {
                user,
                host,
                group: g,
                session,
            } => cmd_ssh_session(user, host, g, session, &rest[1..], &forward_flag),
        },
        flag => Err(Error(format!("unknown option {flag} (see posh help)"))),
    }
}

fn cmd_attach(group: &str, args: &[String]) -> Result<()> {
    let mut detach_flag = false;
    let mut name: Option<&str> = None;
    let mut command: Vec<String> = Vec::new();
    for arg in args {
        if arg == "--detach" {
            detach_flag = true;
        } else if name.is_none() {
            name = Some(arg);
        } else {
            command.push(arg.clone());
        }
    }
    let name = name.ok_or_else(|| Error::from("attach requires a session name"))?;
    let command = (!command.is_empty()).then_some(command);
    session::client::cmd_attach(&Config::new(group)?, name, command, detach_flag)
}

fn cmd_history(group: &str, args: &[String]) -> Result<()> {
    let mut name: Option<&str> = None;
    let mut vt = false;
    for arg in args {
        match arg.as_str() {
            "--vt" => vt = true,
            other if name.is_none() => name = Some(other),
            _ => {}
        }
    }
    let name = name.ok_or_else(|| Error::from("history requires a session name"))?;
    session::cmd_history(&Config::new(group)?, name, vt)
}

fn cmd_server(args: &[String]) -> Result<()> {
    let mut rest = args;
    // Accept the mosh-server-style `new` verb.
    if rest.first().map(|s| s.as_str()) == Some("new") {
        rest = &rest[1..];
    }
    let mut port_range: Option<(u16, u16)> = None;
    let mut family = Family::Auto;
    let mut command: Option<Vec<String>> = None;
    // Agent forwarding (FDR 0004): the ssh bootstrap appends a bare `-A` to
    // `posh-server new` exactly when the client resolved forwarding on (C4 —
    // the policy stays client-side; the server only learns the outcome). The
    // server then stands up the agent endpoint and exports SSH_AUTH_SOCK.
    let mut agent_forward = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-p" | "--port" => {
                let value = rest
                    .get(i + 1)
                    .ok_or_else(|| Error::from("-p requires PORT[:PORT2]"))?;
                port_range = Some(parse_port_range(value)?);
                i += 2;
            }
            flag @ ("-4" | "-6") => {
                family = Family::from_flag(flag).expect("matched flag");
                i += 1;
            }
            "-A" | "--forward-agent" => {
                agent_forward = true;
                i += 1;
            }
            "--" => {
                let cmd: Vec<String> = rest[i + 1..].to_vec();
                command = (!cmd.is_empty()).then_some(cmd);
                break;
            }
            other => return Err(Error(format!("unknown server option {other}"))),
        }
    }
    remote::server::run(port_range, family, command, agent_forward)
}

fn parse_port_range(s: &str) -> Result<(u16, u16)> {
    let parse = |v: &str| -> Result<u16> {
        v.parse::<u16>()
            .map_err(|_| Error(format!("invalid port number ({v})")))
    };
    let (low, high) = match s.split_once(':') {
        Some((l, h)) => (parse(l)?, parse(h)?),
        None => {
            let p = parse(s)?;
            (p, p)
        }
    };
    if low == 0 || low > high {
        return Err(Error(format!("invalid port range ({s})")));
    }
    Ok((low, high))
}

fn cmd_client(args: &[String]) -> Result<()> {
    let mut family = Family::Auto;
    let mut positional: Vec<&String> = Vec::new();
    for arg in args {
        if let Some(f) = Family::from_flag(arg) {
            family = f;
        } else {
            positional.push(arg);
        }
    }
    let (host, port) = match positional.as_slice() {
        [host, port] => (
            host.as_str(),
            port.parse::<u16>()
                .map_err(|_| Error(format!("invalid port ({port})")))?,
        ),
        _ => return Err(Error::from("usage: posh client [-4|-6] <host> <port>")),
    };
    // The raw `posh client` subcommand carries no forwarding policy (it's the
    // low-level transport entrypoint); agent forwarding is resolved on the
    // `posh host:session` path. Off here.
    remote::client::run(host, port, family, None)
}

/// `posh [user@]host:[group/]session` (RFC 0001 §2): attach to (creating
/// if needed) a persistent session on the host, transported over the
/// roaming UDP connection. Composes the two halves of posh: the remote
/// command is `posh-server new -- posh [-g GROUP] attach SESSION
/// [command...]`, so persistence lives in the remote session daemon and
/// this transport pair stays disposable.
///
/// A leading `--detach` requests a DETACHED spawn (#67): create-or-ensure the
/// session on the host and return promptly, WITHOUT standing up the roaming
/// transport. The session keeps running as a daemon on the host (the remote
/// analog of local `posh attach --detach`); a later foreground
/// `posh host:group/session` attaches to that same session. This is the
/// fire-and-return primitive a remote session-manager worker (spinclass
/// FDR 0006, clown) maps onto.
fn cmd_ssh_session(
    user: Option<String>,
    host: String,
    group: Option<String>,
    session: String,
    extra: &[String],
    forward_flag: &remote::agent::ForwardFlag,
) -> Result<()> {
    let (detached, command) = parse_remote_session_extra(extra);
    let inner = remote_session_argv(group.as_deref(), &session, detached, command);
    let dest = match &user {
        Some(u) => format!("{u}@{host}"),
        None => host,
    };
    if detached {
        // Detached spawn (#67): no transport, so no agent endpoint — execute
        // the inner `posh attach --detach` directly over ssh and return.
        let opts = remote::sshwrap::SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
        };
        return remote::sshwrap::run_detached(&dest, &inner, &opts);
    }
    // Foreground roaming attach. Resolve agent forwarding (flag > env >
    // default-on).
    let opts = remote::sshwrap::SshOptions {
        family: Family::Auto,
        port_range: None,
        agent_source: resolve_agent_source(forward_flag),
    };
    remote::sshwrap::run(&dest, &inner, &opts)
}

/// Splits the post-target args of `posh host:[group/]session ...` into
/// `(detached, command)`. A leading `--detach` requests a detached spawn
/// (#67); a single leading `--` separator (after the optional `--detach`) is
/// consumed so the rest is the opaque create-command, mirroring `posh run`.
fn parse_remote_session_extra(extra: &[String]) -> (bool, &[String]) {
    let detached = extra.first().map(|s| s.as_str()) == Some("--detach");
    let mut command = if detached { &extra[1..] } else { extra };
    if command.first().map(|s| s.as_str()) == Some("--") {
        command = &command[1..];
    }
    (detached, command)
}

/// The inner `posh [-g GROUP] attach SESSION [--detach] [command...]` argv
/// that rides the remote host — under `posh-server new` for the foreground
/// roaming attach, or directly over ssh for a detached spawn (#67). `--detach`
/// lands after SESSION (where the remote `posh attach` recognizes it) and
/// before the create-command.
fn remote_session_argv(
    group: Option<&str>,
    session: &str,
    detached: bool,
    command: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["posh".into()];
    if let Some(g) = group {
        argv.push("-g".into());
        argv.push(g.into());
    }
    argv.push("attach".into());
    argv.push(session.into());
    if detached {
        argv.push("--detach".into());
    }
    argv.extend_from_slice(command);
    argv
}

/// The ssh argv behind `posh list host:` (separated for testability). A
/// non-default `group` is threaded as `posh -g GROUP list --short` so the
/// remote probe is scoped to that group (#66); the default group injects no
/// `-g`, leaving the pre-#66 wire shape unchanged.
fn remote_list_argv(user: Option<&str>, host: &str, group: &str) -> Vec<String> {
    let dest = match user {
        Some(u) => format!("{u}@{host}"),
        None => host.to_string(),
    };
    let mut argv: Vec<String> = ["ssh", "-o", "BatchMode=yes", &dest, "posh"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if group != "default" {
        argv.push("-g".into());
        argv.push(group.into());
    }
    argv.push("list".into());
    argv.push("--short".into());
    argv
}

/// The pasteable RemoteSession target for one remote-listed name. A
/// non-default group carries its `group/` segment so the name resolves back to
/// the same group (Target::parse: `host:group/session`); default-group names
/// stay bare (`host:name`).
fn remote_list_line(prefix: &str, group: &str, name: &str) -> String {
    if group == "default" {
        format!("{prefix}:{name}")
    } else {
        format!("{prefix}:{group}/{name}")
    }
}

fn cmd_list_remote(user: Option<String>, host: String, group: &str) -> Result<()> {
    let argv = remote_list_argv(user.as_deref(), &host, group);
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| Error(format!("ssh: {e}")))?;
    if !out.status.success() {
        use std::io::Write;
        let _ = std::io::stderr().write_all(&out.stderr);
        return Err(Error(format!("remote list failed on {host}")));
    }
    // Every printed name is itself a valid RemoteSession target.
    let prefix = match &user {
        Some(u) => format!("{u}@{host}"),
        None => host.clone(),
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if !line.is_empty() {
            println!("{}", remote_list_line(&prefix, group, line));
        }
    }
    Ok(())
}

// `forward` is Some for the mosh-parity bare `posh host` roaming path (which
// honors agent forwarding like `host:session`), and None for the explicit
// `posh ssh` subcommand, which stays a thin ssh wrapper — a `-A` there is the
// real ssh flag, not posh forwarding (FDR 0004 §Limitations).
fn cmd_ssh(args: &[String], forward: Option<&remote::agent::ForwardFlag>) -> Result<()> {
    let usage = "usage: posh ssh [-4|-6] [-p PORT[:PORT2]] [user@]host [-- command...]";
    let mut family = Family::Auto;
    let mut port_range: Option<String> = None;
    let mut target: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            flag @ ("-4" | "-6") => {
                family = Family::from_flag(flag).expect("matched flag");
                i += 1;
            }
            "-p" | "--port" => {
                let value = args.get(i + 1).ok_or_else(|| Error::from(usage))?;
                parse_port_range(value)?; // validate locally before passing on
                port_range = Some(value.clone());
                i += 2;
            }
            "--" => {
                i += 1;
                break;
            }
            _ if target.is_none() => {
                target = Some(&args[i]);
                i += 1;
            }
            _ => break,
        }
    }
    let target = target.ok_or_else(|| Error::from(usage))?;
    let mut remote_cmd = &args[i..];
    if remote_cmd.first().map(|s| s.as_str()) == Some("--") {
        remote_cmd = &remote_cmd[1..];
    }
    // Resolve agent forwarding for the roaming bare-host path; the explicit
    // `posh ssh` subcommand passes None and stays a thin wrapper.
    let opts = remote::sshwrap::SshOptions {
        family,
        port_range,
        agent_source: forward.and_then(resolve_agent_source),
    };
    remote::sshwrap::run(target, remote_cmd, &opts)
}

/// Resolves the local agent socket to forward (FDR 0004) from the CLI flag plus
/// $POSH_FORWARD_AGENT / $SSH_AUTH_SOCK, printing the explicit-`-A`-no-agent
/// warning to stderr. Shared by the `host:session` and bare-`host` roaming
/// paths. Returns the source path when forwarding is on, else None.
fn resolve_agent_source(flag: &remote::agent::ForwardFlag) -> Option<std::path::PathBuf> {
    let (policy, warning) = remote::agent::resolve_forward_policy(
        flag,
        std::env::var("POSH_FORWARD_AGENT").ok().as_deref(),
        std::env::var("SSH_AUTH_SOCK").ok().as_deref(),
    );
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    match policy {
        remote::agent::ForwardPolicy::On { source } => Some(source),
        remote::agent::ForwardPolicy::Off => None,
    }
}

const HELP: &str = "\
NAME
    posh - persistent, roaming terminal sessions

SYNOPSIS
    posh [-g GROUP] <command> [args]
    posh <name>                       (shorthand for: posh attach <name>)
    posh :[group/]session             (explicit local attach)
    posh [user@]host [-- command...]  (shorthand for: posh ssh ...)
    posh [user@]host:[group/]session [--detach] [command...]
                                      (persistent session on the host over
                                       the roaming transport; scp-style —
                                       brackets for IPv6: [fe80::1]:dev.
                                       With --detach, create the session on
                                       the host and return without attaching
                                       — the remote analog of attach --detach.)

GLOBAL OPTIONS
    -g, --group GROUP
        Session group (default: \"default\", or $POSH_GROUP). Each group is
        a subdirectory of the socket directory.

    --no-init
        Do not take over the terminal's alternate screen on attach/connect
        (mosh --no-init parity; also $POSH_NO_TERM_INIT). The takeover
        sequences normally come from terminfo smcup/rmcup for $TERM,
        falling back to DECSET 1049 when no database entry is found; a
        terminal whose entry defines no alternate screen is never taken
        over. FDR 0002.

    --record FILE
        Tee this session's PTY output into a .castx recording (also
        $POSH_RECORD_FILE). Replay it deterministically with
        `poshterity replay FILE` / `posh rec replay FILE`.

SESSION COMMANDS (local persistence)
    attach <name> [command...] [--detach]      (alias: a)
        Attach to a session, creating it (running command, default $SHELL)
        if needed. With --detach, ensure the session exists, print status,
        and exit without attaching. Detach key: Ctrl-\\.

    list [--short] [-j|--json]                 (aliases: ls, l)
        List sessions in the group: name, pid, attached client count.
        --short prints names only; --json prints a machine-readable array.

    run <name> [--] <command...>               (alias: r)
        Send a command to a session (created if needed) without attaching.
        Reads the command from stdin when no arguments are given.

    fork [<name>]                              (alias: f)
        Fork the current session ($POSH_SESSION) into a new detached
        session with the same command and working directory. Without a
        name, the first free \"<current>-N\" is used.

    detach [<name>]                            (alias: d)
        Detach all clients from the named session, or from the current
        session ($POSH_SESSION) when no name is given.

    detach-all                                 (alias: da)
        Detach all clients from all sessions in the group.

    kill <name>                                (alias: k)
        Kill a session, its shell, and all attached clients.

    groups                                     (alias: gs)
        List session groups that contain sessions.

    tailnet
        List reachable Tailscale peer names (MagicDNS), one per line — what
        tab-completion offers for tailnet hosts. Empty (and exit 0) when
        tailscale isn't installed or you're not logged in. A tailnet name
        then attaches/connects like any other host (e.g. posh peer:dev).

    history <name> [--vt]                      (alias: hi)
        Print the session's scrollback. Plain text by default; --vt emits
        the escape stream that reconstructs the screen with attributes.

    completions <shell>                        (alias: c)
        Print the completion script for bash, zsh, or fish.

REMOTE COMMANDS (roaming over encrypted UDP)
    server [new] [-p PORT[:PORT2]] [-4|-6] [-- command...]
        Start a remote server: bind a UDP port (default range 60001:60999,
        dual-stack when possible; -4/-6 force a family), print
        \"POSH CONNECT <port> <key>\" on stdout, detach into the
        background, and run the command (default $SHELL) in a PTY.

    client [-4|-6] <host> <port>
        Connect to a posh server. The session key is read from $POSH_KEY
        (never passed on the command line). Keystrokes are echoed
        speculatively (mosh-style prediction; see $POSH_PREDICTION) and
        the screen is updated with minimal diffs.
        Quit sequence: Ctrl-^ then \".\" (Ctrl-^ twice for a literal one).

    ssh [-4|-6] [-p PORT[:PORT2]] [user@]host [-- command...]
        Convenience wrapper (mosh-style; also reachable as a bare
        `posh [user@]host` when the host contains @ . or :): start
        `posh-server new` on the host via ssh (forwarding LANG/LC_* and
        the -p/-4/-6 flags), then connect to the address the server
        reports. The remote host needs `posh-server` on its
        non-interactive PATH (the nix package installs it next to posh).
        Survives IP changes and sleep/resume.

TOOLS
    rec replay <file> [--to-marker NAME] [--dump text|vt|flat]
    rec step <file> --by <granularity> [--n N] [--dump ...]
    rec bless/assert <file> --golden <path> [--at MARKER] [--kind grid|vt|flat]
        Replay a .castx / asciinema .cast v2 recording through the
        in-process posh-term emulator (deterministic; timing is never
        replayed as sleeps). `replay` prints the final screen; `step`
        advances by byte/escape/write/change/frame/marker; `bless`/`assert`
        write and check golden-frame snapshots (a deterministic
        capture-pane). Also the standalone `poshterity` binary, which
        additionally records (`poshterity record -- <cmd>`).

ENVIRONMENT
    POSH_DIR        Socket directory (default: $XDG_RUNTIME_DIR/posh, then
                    $TMPDIR/posh-<uid>, then /tmp/posh-<uid>)
    POSH_GROUP      Default session group
    POSH_SESSION    Set inside sessions to the session name
    POSH_KEY        Remote session key (posh client)
    POSH_PREDICTION
                    Local-echo prediction display: always, never, adaptive
                    (default), or experimental
    POSH_PREDICTION_OVERWRITE
                    When set, predictions overwrite instead of inserting
    POSH_GRAB_MOUSE
                    on/off (default off): when on, the client grabs the
                    wheel on the outer terminal at a bare prompt and turns
                    wheel up/down into arrow keys, so scrolling behaves the
                    same across terminals (kitty otherwise sprays arrows on
                    its own). Costs the outer terminal's click-to-select
                    while active; apps that grab the mouse are unaffected.
    POSH_SERVER_NETWORK_TMOUT
                    Server exits after N seconds without client contact
                    (0 = never, the default)
    POSH_SERVER_SIGNAL_TMOUT
                    On SIGUSR1, the server exits if the client has been
                    silent for N seconds (0 = never, the default)
    POSH_DEBUG_LOG  Path to a performance-diagnostics log for the roaming
                    remote transport. When set, the client and server each
                    append periodic one-line summaries (RTT, bandwidth, frame
                    sizes, full-vs-diff economics, prediction state, render
                    activity; 5 MB rotation). For the bare host:session form
                    it is forwarded to the remote, so the server logs to that
                    path on the remote host. Unset = no logging.

OTHER
    help            Show this help message
    version         Show version
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_range_parsing() {
        assert_eq!(parse_port_range("60001").unwrap(), (60001, 60001));
        assert_eq!(parse_port_range("60001:60999").unwrap(), (60001, 60999));
        assert!(parse_port_range("0").is_err());
        assert!(parse_port_range("70000").is_err());
        assert!(parse_port_range("100:50").is_err());
        assert!(parse_port_range("abc").is_err());
    }

    #[test]
    fn remote_list_command_shape() {
        // `posh list box:` runs a BatchMode ssh so completion-time and
        // script callers can never hang on an auth prompt. The default group
        // injects no `-g`, so the wire shape is unchanged from pre-#66.
        assert_eq!(
            remote_list_argv(Some("user"), "box", "default"),
            ["ssh", "-o", "BatchMode=yes", "user@box", "posh", "list", "--short"]
                .map(String::from)
        );
        assert_eq!(remote_list_argv(None, "box", "default")[3], "box");
    }

    #[test]
    fn remote_list_threads_nondefault_group() {
        // #66: `posh -g GROUP list host:` must scope the remote probe to
        // GROUP via `posh -g GROUP list --short`, or a session created in a
        // non-default group on the remote is invisible to the probe.
        assert_eq!(
            remote_list_argv(Some("user"), "box", "spinclass"),
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "user@box",
                "posh",
                "-g",
                "spinclass",
                "list",
                "--short",
            ]
            .map(String::from)
        );
    }

    #[test]
    fn remote_list_line_carries_nondefault_group() {
        // Every printed name must paste back as a valid RemoteSession in the
        // SAME group: default-group names stay bare, non-default names carry
        // the `group/` segment (Target::parse: host:group/session).
        assert_eq!(remote_list_line("box", "default", "dev"), "box:dev");
        assert_eq!(
            remote_list_line("user@box", "spinclass", "id7"),
            "user@box:spinclass/id7"
        );
    }

    #[test]
    fn remote_session_argv_foreground_and_detached() {
        // Foreground attach is unchanged: `posh -g grp attach dev htop`.
        assert_eq!(
            remote_session_argv(Some("grp"), "dev", false, &["htop".into()]),
            ["posh", "-g", "grp", "attach", "dev", "htop"].map(String::from)
        );
        // #67 detached spawn: --detach sits after SESSION, before the command.
        assert_eq!(
            remote_session_argv(Some("spinclass"), "w1", true, &["worker".into(), "--flag".into()]),
            ["posh", "-g", "spinclass", "attach", "w1", "--detach", "worker", "--flag"]
                .map(String::from)
        );
        // No group, no command, detached.
        assert_eq!(
            remote_session_argv(None, "w", true, &[]),
            ["posh", "attach", "w", "--detach"].map(String::from)
        );
    }

    #[test]
    fn parse_remote_session_extra_detects_detach_and_strips_separator() {
        let v = |xs: &[&str]| -> Vec<String> { xs.iter().map(|s| s.to_string()).collect() };

        // Plain create-command, no detach: passed through untouched.
        let e = v(&["htop"]);
        assert_eq!(parse_remote_session_extra(&e), (false, &e[..]));

        // #67 spawn form: `--detach -- <command>` (the spinclass/clown shape).
        // The `--detach` is consumed, the `--` separator stripped, command
        // opaque.
        let e = v(&["--detach", "--", "worker", "arg"]);
        let (detached, command) = parse_remote_session_extra(&e);
        assert!(detached);
        assert_eq!(command, &v(&["worker", "arg"])[..]);

        // `--detach` with no create-command.
        let e = v(&["--detach"]);
        assert_eq!(parse_remote_session_extra(&e), (true, &[][..]));

        // Leading `--` without `--detach`: a foreground create-command after
        // the separator (no detach).
        let e = v(&["--", "vim"]);
        let (detached, command) = parse_remote_session_extra(&e);
        assert!(!detached);
        assert_eq!(command, &v(&["vim"])[..]);
    }

    #[test]
    fn help_covers_all_commands_and_env() {
        for needle in [
            "attach",
            "list",
            "run",
            "fork",
            "detach",
            "detach-all",
            "kill",
            "groups",
            "history",
            "completions",
            "server",
            "client",
            "ssh",
        ] {
            assert!(HELP.contains(needle), "help missing {needle}");
        }
        for env in [
            "POSH_DIR",
            "POSH_GROUP",
            "POSH_SESSION",
            "POSH_KEY",
            "POSH_PREDICTION",
            "POSH_SERVER_NETWORK_TMOUT",
            "POSH_SERVER_SIGNAL_TMOUT",
        ] {
            assert!(HELP.contains(env), "help missing {env}");
        }
        assert!(HELP.contains("--json"));
        assert!(HELP.contains("-4|-6"));
        assert!(HELP.contains("Ctrl-^"));
        // RFC 0001 namespace forms.
        assert!(HELP.contains("host:[group/]session"));
        assert!(HELP.contains(":[group/]session"));
    }
}
