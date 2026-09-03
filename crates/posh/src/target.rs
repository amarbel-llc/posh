//! RFC 0001 §1: the total grammar resolving a bare first argument to an
//! attach target. Every malformed namespace form falls back to a typed
//! outcome — parsing never errors, so every pre-existing argument keeps
//! its meaning (`fe80::1`, `::1`, and `box:` all stay pure hosts).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Bare word: local session, attach-or-create (legacy form).
    LocalSession { name: String },
    /// `:session` / `:group/session`: explicit local.
    Local {
        group: Option<String>,
        session: String,
    },
    /// An ssh destination with no session. Formerly the plain roaming shell
    /// (mosh form); since FDR 0011 (durable-by-default) the bare form errors
    /// with the host's session list, and the throwaway shell lives behind
    /// `posh start --ephemeral`. `posh start host` / `host:` read it as a
    /// remote auto-id create.
    Host { user: Option<String>, host: String },
    /// `[user@]host:[group/]session`: a session on a remote host.
    RemoteSession {
        user: Option<String>,
        host: String,
        group: Option<String>,
        session: String,
    },
}

impl Target {
    /// Resolution rules in RFC order; the first match wins.
    pub fn parse(arg: &str) -> Target {
        // Rule 1: explicit local. The remainder must be a valid session
        // part (non-empty, no ':') — IPv6 literals like `::1` fall through
        // to rule 3, whose empty head makes them hosts. A bare ":" stays a
        // session name.
        if let Some(rest) = arg.strip_prefix(':') {
            if !rest.is_empty() && !rest.contains(':') {
                let (group, session) = split_group(rest);
                return Target::Local { group, session };
            }
            if rest.is_empty() {
                return Target::LocalSession {
                    name: arg.to_string(),
                };
            }
        }

        // Rule 2: bracketed host (IPv6-safe). Unterminated falls through.
        if let Some(inner) = arg.strip_prefix('[') {
            if let Some((host, suffix)) = inner.split_once(']') {
                if let Some(session_part) = suffix.strip_prefix(':') {
                    if !session_part.is_empty() && !session_part.contains(':') {
                        let (group, session) = split_group(session_part);
                        return Target::RemoteSession {
                            user: None,
                            host: host.to_string(),
                            group,
                            session,
                        };
                    }
                }
                return Target::Host {
                    user: None,
                    host: host.to_string(),
                };
            }
        }

        // Rule 3: first-colon split; a malformed session part (empty or
        // containing ':') means the argument is a host.
        if let Some((head, session_part)) = arg.split_once(':') {
            if !head.is_empty() && !session_part.is_empty() && !session_part.contains(':') {
                let (user, host) = split_user(head);
                let (group, session) = split_group(session_part);
                return Target::RemoteSession {
                    user,
                    host,
                    group,
                    session,
                };
            }
            // An empty session part is just a trailing colon (`box:` is a
            // plain shell on box); a part containing ':' is an IPv6
            // literal whose colons belong to the host.
            let raw = if session_part.is_empty() && !head.is_empty() {
                head
            } else {
                arg
            };
            let (user, host) = split_user(raw);
            return Target::Host { user, host };
        }

        // Rule 6: no colon. @ or . marks an ssh destination.
        if arg.contains('@') || arg.contains('.') {
            let (user, host) = split_user(arg);
            return Target::Host { user, host };
        }
        Target::LocalSession {
            name: arg.to_string(),
        }
    }
}

/// Rule 4: text before the first `@` is the user (both halves non-empty).
fn split_user(s: &str) -> (Option<String>, String) {
    match s.split_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() => {
            (Some(user.to_string()), host.to_string())
        }
        _ => (None, s.to_string()),
    }
}

/// Rule 5: text before the first `/` is the group (both halves non-empty).
fn split_group(s: &str) -> (Option<String>, String) {
    match s.split_once('/') {
        Some((group, session)) if !group.is_empty() && !session.is_empty() => {
            (Some(group.to_string()), session.to_string())
        }
        _ => (None, s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::Target::{self, *};

    fn s(v: &str) -> String {
        v.into()
    }

    #[test]
    fn rfc_normative_examples() {
        let cases: Vec<(&str, Target)> = vec![
            ("dev", LocalSession { name: s("dev") }),
            (
                ":dev",
                Local {
                    group: None,
                    session: s("dev"),
                },
            ),
            (
                ":grp/dev",
                Local {
                    group: Some(s("grp")),
                    session: s("dev"),
                },
            ),
            (":", LocalSession { name: s(":") }),
            (
                "box.example",
                Host {
                    user: None,
                    host: s("box.example"),
                },
            ),
            (
                "user@box",
                Host {
                    user: Some(s("user")),
                    host: s("box"),
                },
            ),
            (
                "box:dev",
                RemoteSession {
                    user: None,
                    host: s("box"),
                    group: None,
                    session: s("dev"),
                },
            ),
            (
                "user@box:grp/dev",
                RemoteSession {
                    user: Some(s("user")),
                    host: s("box"),
                    group: Some(s("grp")),
                    session: s("dev"),
                },
            ),
            (
                "[fe80::1]:dev",
                RemoteSession {
                    user: None,
                    host: s("fe80::1"),
                    group: None,
                    session: s("dev"),
                },
            ),
            (
                "fe80::1",
                Host {
                    user: None,
                    host: s("fe80::1"),
                },
            ),
            (
                "::1",
                Host {
                    user: None,
                    host: s("::1"),
                },
            ),
            (
                "box:",
                Host {
                    user: None,
                    host: s("box"),
                },
            ),
            (
                "[fe80::1]",
                Host {
                    user: None,
                    host: s("fe80::1"),
                },
            ),
            // group/ split requires both halves non-empty:
            (
                "box:/dev",
                RemoteSession {
                    user: None,
                    host: s("box"),
                    group: None,
                    session: s("/dev"),
                },
            ),
            (
                "box:grp/",
                RemoteSession {
                    user: None,
                    host: s("box"),
                    group: None,
                    session: s("grp/"),
                },
            ),
            // unterminated bracket falls through to rule 3 (the ':' inside
            // makes the session part malformed -> Host):
            (
                "[fe80::1",
                Host {
                    user: None,
                    host: s("[fe80::1"),
                },
            ),
            // explicit-local nested group split:
            (
                ":a/b/c",
                Local {
                    group: Some(s("a")),
                    session: s("b/c"),
                },
            ),
            // user@ requires both halves: bare word with trailing @ is a
            // host by rule 6 (contains @) but keeps no user.
            (
                "user@",
                Host {
                    user: None,
                    host: s("user@"),
                },
            ),
        ];
        for (input, want) in cases {
            assert_eq!(Target::parse(input), want, "input {input:?}");
        }
    }

    #[test]
    fn legacy_dispatch_equivalence() {
        // Every form the old @/./: heuristic sent to ssh still reaches a
        // remote target kind, and bare words stay local.
        for host in ["user@host", "host.example.com", "fe80::1"] {
            assert!(
                matches!(Target::parse(host), Host { .. }),
                "{host} must stay an ssh destination"
            );
        }
        for name in ["dev", "my-session", "scratch2"] {
            assert!(
                matches!(Target::parse(name), LocalSession { .. }),
                "{name} must stay a local session"
            );
        }
    }
}
