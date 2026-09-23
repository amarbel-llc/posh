//! A session's working directory, decided once (ADR 0008).
//!
//! No single fact is available, current and portable at once, so the answer
//! is an ordered cascade: the first fact that is present AND names an existing
//! directory wins. The facts are gathered by the caller (the daemon holds the
//! child pid, the terminal model and its start directory); this module only
//! orders them, so the rule lives in one place and is testable without a
//! session.

use std::path::Path;

/// The facts a caller can supply, in cascade order.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    /// The cwd of a caller INSIDE the session (a CLI such as `posh start --
    /// <cmd>`); `None` when the request did not come from one.
    pub caller: Option<&'a str>,
    /// The kernel's cwd of the session's child process (Linux only today).
    pub kernel: Option<&'a str>,
    /// The shell's last OSC 7 report.
    pub osc7: Option<&'a str>,
    /// The daemon's start directory.
    pub start: &'a str,
    /// `$HOME`.
    pub home: Option<&'a str>,
}

/// Which cascade step produced the answer. The byte is its wire form (the
/// `Tag::Info` tail); an unknown byte reads as [`Source::Start`], the only
/// meaning a session's cwd had before the cascade existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Caller,
    Kernel,
    Osc7,
    Start,
    Home,
}

impl Source {
    pub fn to_byte(self) -> u8 {
        match self {
            Source::Caller => 1,
            Source::Kernel => 2,
            Source::Osc7 => 3,
            Source::Start => 4,
            Source::Home => 5,
        }
    }

    pub fn from_byte(b: u8) -> Source {
        match b {
            1 => Source::Caller,
            2 => Source::Kernel,
            3 => Source::Osc7,
            5 => Source::Home,
            _ => Source::Start,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Caller => "caller",
            Source::Kernel => "kernel",
            Source::Osc7 => "osc7",
            Source::Start => "start",
            Source::Home => "home",
        }
    }
}

/// The cascade's answer and the step that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub dir: String,
    pub source: Source,
}

/// ADR 0008's cascade: the first fact that is non-empty and names an existing
/// directory. `/` when even `$HOME` is unusable, so there is always an answer.
pub fn session_cwd(f: Facts<'_>) -> Resolved {
    let steps = [
        (f.caller, Source::Caller),
        (f.kernel, Source::Kernel),
        (f.osc7, Source::Osc7),
        (Some(f.start), Source::Start),
        (f.home, Source::Home),
    ];
    steps
        .into_iter()
        .find_map(|(fact, source)| {
            fact.filter(|d| !d.is_empty() && Path::new(d).is_dir())
                .map(|d| Resolved { dir: d.to_string(), source })
        })
        .unwrap_or_else(|| Resolved { dir: "/".to_string(), source: Source::Home })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> String {
        std::env::temp_dir().display().to_string()
    }

    const GONE: &str = "/no/such/posh/dir";

    fn facts<'a>(
        caller: Option<&'a str>,
        kernel: Option<&'a str>,
        osc7: Option<&'a str>,
        start: &'a str,
        home: Option<&'a str>,
    ) -> Facts<'a> {
        Facts { caller, kernel, osc7, start, home }
    }

    /// The first available fact wins, in ADR 0008's order.
    #[test]
    fn the_first_available_fact_wins_in_order() {
        let t = tmp();
        let t = t.as_str();
        let at = |f| session_cwd(f).source;
        assert_eq!(at(facts(Some(t), Some(t), Some(t), t, Some(t))), Source::Caller);
        assert_eq!(at(facts(None, Some(t), Some(t), t, Some(t))), Source::Kernel);
        assert_eq!(at(facts(None, None, Some(t), t, Some(t))), Source::Osc7);
        assert_eq!(at(facts(None, None, None, t, Some(t))), Source::Start);
        assert_eq!(session_cwd(facts(None, None, None, t, None)).dir, t);
    }

    /// A fact naming a directory that no longer exists is skipped, not used —
    /// and so is an empty one (a shell that never sent OSC 7).
    #[test]
    fn a_missing_or_empty_fact_is_skipped() {
        let t = tmp();
        let t = t.as_str();
        assert_eq!(session_cwd(facts(Some(GONE), Some(""), Some(t), t, None)).source, Source::Osc7);
        assert_eq!(session_cwd(facts(None, Some(GONE), Some(GONE), t, None)).source, Source::Start);
    }

    /// `$HOME` is the last resort, and `/` stands in when even that is gone.
    #[test]
    fn home_is_the_last_resort_then_root() {
        let t = tmp();
        let r = session_cwd(facts(None, None, None, GONE, Some(t.as_str())));
        assert_eq!((r.dir.as_str(), r.source), (t.as_str(), Source::Home));
        let r = session_cwd(facts(None, None, None, GONE, Some(GONE)));
        assert_eq!((r.dir.as_str(), r.source), ("/", Source::Home));
    }

    /// The wire byte round-trips, and an unknown byte reads as the start
    /// directory — what `cwd` always meant before the cascade.
    #[test]
    fn source_bytes_roundtrip_and_unknown_reads_start() {
        for s in [Source::Caller, Source::Kernel, Source::Osc7, Source::Start, Source::Home] {
            assert_eq!(Source::from_byte(s.to_byte()), s);
        }
        assert_eq!(Source::from_byte(0), Source::Start);
        assert_eq!(Source::from_byte(200), Source::Start);
    }
}
