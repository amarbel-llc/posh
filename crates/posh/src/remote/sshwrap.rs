//! ssh bootstrap wrapper (mosh.pl port, simplified): run `posh server new`
//! on the remote host over ssh, parse the POSH CONNECT line, then run the
//! UDP client locally with the key in the environment.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use crate::util::{Error, Result};

pub fn run(target: &str, remote_cmd: &[String]) -> Result<()> {
    let mut server_cmd = String::from("posh server new");
    if !remote_cmd.is_empty() {
        server_cmd.push_str(" --");
        for arg in remote_cmd {
            server_cmd.push(' ');
            server_cmd.push_str(&shell_quote(arg));
        }
    }

    let mut child = Command::new("ssh")
        .arg(target)
        .arg("--")
        .arg(&server_cmd)
        .stdin(Stdio::inherit()) // keep the tty for auth prompts
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| Error(format!("cannot exec ssh: {e}")))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let mut connect: Option<(u16, String)> = None;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if let Some(rest) = line.strip_prefix("POSH CONNECT ") {
            connect = parse_connect(rest);
            if connect.is_none() {
                return Err(Error(format!("bad POSH CONNECT string: {line}")));
            }
            break;
        }
        // Pass through motd and friends.
        println!("{line}");
    }
    let _ = child.wait();

    let (port, key) = connect.ok_or_else(|| {
        Error::from("did not find posh server startup message (is posh installed on the server?)")
    })?;

    // mosh.pl uses an ssh ProxyCommand to learn the server IP; we simply
    // reuse the hostname and resolve it locally.
    let host = target.rsplit('@').next().unwrap_or(target);
    std::env::set_var("POSH_KEY", key);
    crate::remote::client::run(host, port)
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
}
