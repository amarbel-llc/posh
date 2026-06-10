//! posh: persistent, roaming terminal sessions.
//!
//! Combines zmx-style local session persistence (daemon-per-session over
//! Unix sockets) with mosh-style roaming remote sessions (encrypted UDP).

mod pty;
mod remote;
mod session;
mod util;

use session::{Config, ListFormat};
use util::{Error, Result};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    if let Err(e) = run() {
        eprintln!("posh: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut group = std::env::var("POSH_GROUP").unwrap_or_else(|_| "default".to_string());

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
            println!("posh {VERSION}");
            Ok(())
        }
        "list" | "ls" | "l" => {
            let format = if args.iter().any(|a| a == "--short") {
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
        "server" => cmd_server(args),
        "client" => cmd_client(args),
        "ssh" => cmd_ssh(args),
        name if !name.starts_with('-') => {
            // Bare `posh <name>` attaches (creating the session if needed).
            cmd_attach(&group, rest)
        }
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

fn cmd_server(args: &[String]) -> Result<()> {
    let mut rest = args;
    // Accept the mosh-server-style `new` verb.
    if rest.first().map(|s| s.as_str()) == Some("new") {
        rest = &rest[1..];
    }
    let mut port_range: Option<(u16, u16)> = None;
    let mut command: Option<Vec<String>> = None;
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
            "--" => {
                let cmd: Vec<String> = rest[i + 1..].to_vec();
                command = (!cmd.is_empty()).then_some(cmd);
                break;
            }
            other => return Err(Error(format!("unknown server option {other}"))),
        }
    }
    remote::server::run(port_range, command)
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
    let (host, port) = match args {
        [host, port] => (
            host,
            port.parse::<u16>()
                .map_err(|_| Error(format!("invalid port ({port})")))?,
        ),
        _ => return Err(Error::from("usage: posh client <host> <port>")),
    };
    remote::client::run(host, port)
}

fn cmd_ssh(args: &[String]) -> Result<()> {
    let target = args
        .first()
        .ok_or_else(|| Error::from("usage: posh ssh [user@]host [-- command...]"))?;
    let mut remote_cmd = &args[1..];
    if remote_cmd.first().map(|s| s.as_str()) == Some("--") {
        remote_cmd = &remote_cmd[1..];
    }
    remote::sshwrap::run(target, remote_cmd)
}

const HELP: &str = "\
NAME
    posh - persistent, roaming terminal sessions

SYNOPSIS
    posh [-g GROUP] <command> [args]
    posh <name>                       (shorthand for: posh attach <name>)

GLOBAL OPTIONS
    -g, --group GROUP
        Session group (default: \"default\", or $POSH_GROUP). Each group is
        a subdirectory of the socket directory.

SESSION COMMANDS (local persistence)
    attach <name> [command...] [--detach]      (alias: a)
        Attach to a session, creating it (running command, default $SHELL)
        if needed. With --detach, ensure the session exists, print status,
        and exit without attaching. Detach key: Ctrl-\\.

    list [--short]                             (aliases: ls, l)
        List sessions in the group: name, pid, attached client count.
        --short prints names only.

    run <name> [--] <command...>               (alias: r)
        Send a command to a session (created if needed) without attaching.
        Reads the command from stdin when no arguments are given.

    detach [<name>]                            (alias: d)
        Detach all clients from the named session, or from the current
        session ($POSH_SESSION) when no name is given.

    kill <name>                                (alias: k)
        Kill a session, its shell, and all attached clients.

REMOTE COMMANDS (roaming over encrypted UDP)
    server [new] [-p PORT[:PORT2]] [-- command...]
        Start a remote server: bind a UDP port (default range 60001:60999),
        print \"POSH CONNECT <port> <key>\" on stdout, detach into the
        background, and run the command (default $SHELL) in a PTY.

    client <host> <port>
        Connect to a posh server. The session key is read from $POSH_KEY
        (never passed on the command line).

    ssh [user@]host [-- command...]
        Convenience wrapper: start `posh server new` on the host via ssh,
        then connect to it. Survives IP changes and sleep/resume.

ENVIRONMENT
    POSH_DIR        Socket directory (default: $XDG_RUNTIME_DIR/posh, then
                    $TMPDIR/posh-<uid>, then /tmp/posh-<uid>)
    POSH_GROUP      Default session group
    POSH_SESSION    Set inside sessions to the session name
    POSH_KEY        Remote session key (posh client)

OTHER
    help            Show this help message
    version         Show version
";
