//! `poshterity record --script FILE`: an expect-style driver. A script waits
//! for the recorded program's LIVE screen to show (or stop showing) some text
//! before it sends the next keys, so a scripted session does not race the
//! program with `sleep`s. This module is the pure part — the parser and the
//! step state machine — with no clock and no PTY, so it is tested with a fake
//! clock and fake screens; `record.rs` supplies the real ones.
//!
//! Script syntax, one step per line (blank lines and `#` comments ignored):
//!
//! ```text
//! wait-for [--timeout SECS] TEXT    # until the screen contains TEXT (default 10 s)
//! wait-lacks [--timeout SECS] TEXT  # until the screen no longer contains TEXT
//! send TEXT                         # type TEXT; \r \n \t \e \\ and \xHH escapes
//! sleep SECS
//! mark NAME                         # record a marker at this point
//! ```
//!
//! TEXT for `wait-*` is the rest of the line, taken literally (trimmed).

/// How long a `wait-*` step waits when it names no `--timeout`.
pub const DEFAULT_WAIT_SECS: f64 = 10.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Send(Vec<u8>),
    Sleep(f64),
    WaitFor { text: String, timeout: f64 },
    WaitLacks { text: String, timeout: f64 },
    Mark(String),
}

impl Step {
    /// A short human description, for a timeout message.
    pub fn describe(&self) -> String {
        match self {
            Step::Send(b) => format!("send {:?}", String::from_utf8_lossy(b)),
            Step::Sleep(s) => format!("sleep {s}"),
            Step::WaitFor { text, timeout } => format!("wait-for {text:?} (timeout {timeout}s)"),
            Step::WaitLacks { text, timeout } => format!("wait-lacks {text:?} (timeout {timeout}s)"),
            Step::Mark(n) => format!("mark {n}"),
        }
    }
}

/// Parse a script. Errors carry the 1-based line number.
pub fn parse(src: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |msg: String| format!("script line {}: {msg}", i + 1);
        let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim_start();
        steps.push(match verb {
            "send" => Step::Send(unescape(rest).map_err(&at)?),
            "sleep" => Step::Sleep(secs(rest).map_err(&at)?),
            "mark" => {
                if rest.is_empty() {
                    return Err(at("mark needs a name".into()));
                }
                Step::Mark(rest.to_string())
            }
            "wait-for" | "wait-lacks" => {
                let (timeout, text) = wait_args(rest).map_err(&at)?;
                if text.is_empty() {
                    return Err(at(format!("{verb} needs some text")));
                }
                if verb == "wait-for" {
                    Step::WaitFor { text, timeout }
                } else {
                    Step::WaitLacks { text, timeout }
                }
            }
            other => return Err(at(format!("unknown step {other:?} (send|sleep|wait-for|wait-lacks|mark)"))),
        });
    }
    Ok(steps)
}

fn secs(s: &str) -> Result<f64, String> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or_else(|| format!("expected a non-negative number of seconds, got {s:?}"))
}

/// `[--timeout SECS] TEXT` -> (timeout, TEXT).
fn wait_args(rest: &str) -> Result<(f64, String), String> {
    if let Some(after) = rest.strip_prefix("--timeout") {
        let after = after.trim_start();
        let (n, text) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
        return Ok((secs(n)?, text.trim().to_string()));
    }
    Ok((DEFAULT_WAIT_SECS, rest.trim().to_string()))
}

/// `\r \n \t \e \\ \xHH` escapes; anything else after a backslash is an error.
pub fn unescape(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match it.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = it.by_ref().take(2).collect();
                let b = (hex.len() == 2)
                    .then(|| u8::from_str_radix(&hex, 16).ok())
                    .flatten()
                    .ok_or_else(|| format!("\\x needs two hex digits, got {hex:?}"))?;
                out.push(b);
            }
            Some(other) => return Err(format!("unknown escape \\{other}")),
            None => return Err("a trailing backslash".into()),
        }
    }
    Ok(out)
}

/// Something the runner wants done now.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Send(Vec<u8>),
    Mark(String),
}

/// Where the script stands after an [`Runner::advance`].
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Waiting on a sleep or a screen condition.
    Pending,
    /// Every step has run.
    Done,
    /// A `wait-*` step ran out of time; `what` describes it.
    TimedOut { what: String },
}

/// Runs the steps in order against a clock and a screen, both supplied by the
/// caller (seconds as `f64`, the screen as a closure so it is only rendered
/// while a wait is active).
pub struct Runner {
    steps: Vec<Step>,
    next: usize,
    /// When the current sleep/wait step began or must end (set on first visit).
    deadline: Option<f64>,
}

impl Runner {
    pub fn new(steps: Vec<Step>) -> Runner {
        Runner { steps, next: 0, deadline: None }
    }

    /// Run as many steps as can run at `now`. Returns the actions to perform
    /// (in order) and the outcome.
    pub fn advance(&mut self, now: f64, screen: &mut dyn FnMut() -> String) -> (Vec<Action>, Outcome) {
        let mut actions = Vec::new();
        while let Some(step) = self.steps.get(self.next).cloned() {
            match step {
                Step::Send(b) => actions.push(Action::Send(b)),
                Step::Mark(n) => actions.push(Action::Mark(n)),
                Step::Sleep(s) => {
                    let until = *self.deadline.get_or_insert(now + s);
                    if now < until {
                        return (actions, Outcome::Pending);
                    }
                }
                Step::WaitFor { ref text, timeout } | Step::WaitLacks { ref text, timeout } => {
                    let want_present = matches!(step, Step::WaitFor { .. });
                    let until = *self.deadline.get_or_insert(now + timeout);
                    if screen().contains(text.as_str()) == want_present {
                        // satisfied
                    } else if now >= until {
                        return (actions, Outcome::TimedOut { what: step.describe() });
                    } else {
                        return (actions, Outcome::Pending);
                    }
                }
            }
            self.next += 1;
            self.deadline = None;
        }
        (actions, Outcome::Done)
    }

    /// The step the runner is on (or stopped at); `None` once every step has run.
    pub fn current(&self) -> Option<&Step> {
        self.steps.get(self.next)
    }

    /// Seconds from `now` until this runner next needs attention without any
    /// new output (a sleep ending, a wait timing out); `None` when idle.
    pub fn next_wake(&self, now: f64) -> Option<f64> {
        self.steps
            .get(self.next)
            .and(self.deadline)
            .map(|d| (d - now).max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(s: &'static str) -> impl FnMut() -> String {
        move || s.to_string()
    }

    #[test]
    fn parses_every_step_and_skips_comments() {
        let steps = parse(
            "# a prompt\n\nwait-for ready>\nsend hi\\r\nwait-lacks --timeout 2.5 spinner\nsleep 0.5\nmark done\n",
        )
        .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::WaitFor { text: "ready>".into(), timeout: DEFAULT_WAIT_SECS },
                Step::Send(b"hi\r".to_vec()),
                Step::WaitLacks { text: "spinner".into(), timeout: 2.5 },
                Step::Sleep(0.5),
                Step::Mark("done".into()),
            ]
        );
    }

    #[test]
    fn wait_text_is_the_rest_of_the_line_and_may_hold_spaces() {
        let steps = parse("wait-for   Time left: 29s  \n").unwrap();
        assert_eq!(steps, vec![Step::WaitFor { text: "Time left: 29s".into(), timeout: DEFAULT_WAIT_SECS }]);
    }

    #[test]
    fn rejects_bad_lines_with_their_number() {
        for (src, needle) in [
            ("jump 3", "line 1"),
            ("\n\nsleep soon", "line 3"),
            ("wait-for", "needs some text"),
            ("wait-for --timeout x foo", "seconds"),
            ("send \\q", "unknown escape"),
            ("send \\xZZ", "hex"),
            ("mark", "needs a name"),
        ] {
            let e = parse(src).expect_err(src);
            assert!(e.contains(needle), "{src:?} -> {e}");
        }
    }

    #[test]
    fn unescape_handles_control_keys() {
        assert_eq!(unescape("a\\r\\n\\t\\e\\\\\\x1e").unwrap(), b"a\r\n\t\x1b\\\x1e".to_vec());
        assert_eq!(unescape("é").unwrap(), "é".as_bytes().to_vec());
    }

    #[test]
    fn runner_sends_and_marks_in_order_then_finishes() {
        let mut r = Runner::new(parse("send a\nmark m\nsend b").unwrap());
        let (acts, out) = r.advance(0.0, &mut screen(""));
        assert_eq!(acts, vec![Action::Send(b"a".to_vec()), Action::Mark("m".into()), Action::Send(b"b".to_vec())]);
        assert_eq!(out, Outcome::Done);
    }

    #[test]
    fn runner_waits_for_the_screen_then_continues() {
        let mut r = Runner::new(parse("wait-for ready\nsend go").unwrap());
        let (acts, out) = r.advance(0.0, &mut screen("booting"));
        assert!(acts.is_empty() && out == Outcome::Pending);
        let (acts, out) = r.advance(1.0, &mut screen("booting"));
        assert!(acts.is_empty() && out == Outcome::Pending);
        let (acts, out) = r.advance(1.5, &mut screen("ready now"));
        assert_eq!(acts, vec![Action::Send(b"go".to_vec())]);
        assert_eq!(out, Outcome::Done);
    }

    #[test]
    fn runner_wait_lacks_waits_for_the_text_to_go() {
        let mut r = Runner::new(parse("wait-lacks banner\nmark gone").unwrap());
        assert_eq!(r.advance(0.0, &mut screen("a banner")).1, Outcome::Pending);
        let (acts, out) = r.advance(0.2, &mut screen("clean"));
        assert_eq!((acts, out), (vec![Action::Mark("gone".into())], Outcome::Done));
    }

    #[test]
    fn runner_times_out_naming_the_step() {
        let mut r = Runner::new(parse("wait-for --timeout 2 never").unwrap());
        assert_eq!(r.advance(0.0, &mut screen("x")).1, Outcome::Pending);
        assert_eq!(r.advance(1.9, &mut screen("x")).1, Outcome::Pending);
        match r.advance(2.0, &mut screen("x")).1 {
            Outcome::TimedOut { what } => assert!(what.contains("never") && what.contains("2"), "{what}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_satisfied_wait_beats_its_own_deadline() {
        let mut r = Runner::new(parse("wait-for --timeout 1 ok").unwrap());
        assert_eq!(r.advance(0.0, &mut screen("")).1, Outcome::Pending);
        // the screen shows it at the very instant the deadline passes: still ok
        assert_eq!(r.advance(5.0, &mut screen("ok")).1, Outcome::Done);
    }

    #[test]
    fn current_names_the_step_it_is_on() {
        let mut r = Runner::new(parse("send a\nwait-for ready").unwrap());
        assert!(matches!(r.current(), Some(Step::Send(_))));
        let _ = r.advance(0.0, &mut screen(""));
        assert!(matches!(r.current(), Some(Step::WaitFor { .. })), "{:?}", r.current());
        let _ = r.advance(1.0, &mut screen("ready"));
        assert_eq!(r.current(), None);
    }

    #[test]
    fn runner_sleep_and_next_wake() {
        let mut r = Runner::new(parse("sleep 2\nsend x").unwrap());
        assert_eq!(r.next_wake(0.0), None, "no deadline until the step is first visited");
        assert_eq!(r.advance(10.0, &mut screen("")).1, Outcome::Pending);
        assert_eq!(r.next_wake(11.0), Some(1.0));
        let (acts, out) = r.advance(12.0, &mut screen(""));
        assert_eq!((acts, out), (vec![Action::Send(b"x".to_vec())], Outcome::Done));
        assert_eq!(r.next_wake(12.0), None);
    }
}
