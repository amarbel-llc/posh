//! FDR 0016 session picker: the row source shared by the top-level `ph`
//! chooser and the in-session palette's *Switch session…* command, the
//! switch hand-off between a client that ended with a selection and the front
//! door that re-attaches to it, and the "leaving a session" choice (keep it
//! running, or kill it once the new attach is up).
//!
//! The renderer draws the rows without knowing what a session is (RFC 0005
//! §3.5); this module is where a row's cells and its `session.switch` target
//! are decided. A target is an RFC 0001 spelling the front door already
//! routes (`crate::ph_parse`), so a selection attaches exactly as typing it
//! would.

use std::sync::Mutex;

use posh_proto::caps::SessionKind;
use serde_json::{json, Value};

use crate::session::{self, Config};
use crate::util::Result;

/// The picker's heading and its no-rows text (RFC 0005 §3.2 `title` /
/// §3.5 `empty`).
pub const TITLE: &str = "sessions";
pub const EMPTY: &str = "(no sessions)";

/// One row: the target a selection attaches to and its display cells —
/// label (activity, else launch command, else blank), session id, host,
/// status. The id rides beside the label so same-label sessions (a fleet of
/// detached workers running one command) stay tellable.
#[derive(Debug, PartialEq, Eq)]
pub struct PickerRow {
    pub target: String,
    pub cells: Vec<String>,
}

/// The RFC 0001 target for `session` on `dest` (`None` = this machine) in
/// `group` (`None` = the default group): `:name`, `:group/name`,
/// `dest:name`, `dest:group/name` — the spelling `crate::ph_parse` routes.
pub fn target_for(dest: Option<&str>, group: Option<&str>, session: &str) -> String {
    let scoped = match group {
        Some(g) if g != "default" => format!("{g}/{session}"),
        _ => session.to_string(),
    };
    format!("{}:{scoped}", dest.unwrap_or(""))
}

/// The rows for one host's entries: `dest` is the `[user@]host` the host
/// cell shows and the target carries; `None` is this machine. A trailing
/// `+ create new session…` row routes to `posh start` there (`:+`).
pub fn rows_for(dest: Option<&str>, group: &str, entries: &[session::PickerEntry]) -> Vec<PickerRow> {
    let host_cell = dest.unwrap_or("local").to_string();
    let mut rows: Vec<PickerRow> = entries
        .iter()
        .map(|e| PickerRow {
            target: target_for(dest, Some(group), &e.name),
            cells: vec![e.label.clone(), e.name.clone(), host_cell.clone(), e.status.clone()],
        })
        .collect();
    rows.push(PickerRow {
        target: target_for(dest, Some(group), "+"),
        cells: vec![
            "+ create new session…".to_string(),
            String::new(),
            host_cell,
            String::new(),
        ],
    });
    rows
}

/// The rows the picker shows: one host (`ph host:`, `scope` = its
/// `(user, host)`), or this machine plus every host with a live mux endpoint
/// (bare `ph`, the in-session switcher) — the connected set. A host whose
/// listing fails is reported on stderr and skipped, never fatal.
pub fn rows(scope: Option<(Option<String>, String)>, group: &str) -> Result<Vec<PickerRow>> {
    let mut rows = Vec::new();
    let mut dests: Vec<String> = Vec::new();
    match scope {
        Some((user, host)) => dests.push(crate::ph_dest(user.as_deref(), &host)),
        None => {
            rows.extend(rows_for(None, group, &session::picker_entries_local(&Config::new(group)?)?));
            match crate::remote::mux::live_endpoint_dests() {
                Ok(d) => dests = d,
                Err(e) => eprintln!("posh: mux endpoints unavailable: {e}"),
            }
        }
    }
    for dest in dests {
        let (user, host) = split_dest(&dest);
        match crate::remote_list_output(user, host, group, "--json")
            .and_then(|json| session::picker_entries_from_json(&json))
        {
            Ok(entries) => rows.extend(rows_for(Some(&dest), group, &entries)),
            Err(e) => eprintln!("posh: {dest}: {e}"),
        }
    }
    Ok(rows)
}

/// `[user@]host` split at the last `@` (ssh's rule); an empty user is absent.
fn split_dest(dest: &str) -> (Option<&str>, &str) {
    match dest.rsplit_once('@') {
        Some((u, h)) if !u.is_empty() => (Some(u), h),
        _ => (None, dest),
    }
}

/// The RFC 0005 §3.5 `rows` array: each row's cells plus a
/// `session.switch {target}` action (§7). No `previous` yet — the client
/// asks about the session it is leaving in a second step
/// (`crate::remote::palette_view::leave_commands`) when there is one.
pub fn rows_json(rows: &[PickerRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "cells": r.cells,
                    "action": { "method": "session.switch", "params": { "target": r.target } },
                })
            })
            .collect(),
    )
}

/// The candidate targets (every row but the create rows), for the non-TUI
/// error message.
pub fn candidates(rows: &[PickerRow]) -> String {
    rows.iter()
        .filter(|r| !r.target.ends_with(":+"))
        .map(|r| r.target.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The session group an in-session switch lists and re-attaches in:
/// `$POSH_GROUP`, else `default` (the front door's own resolution).
pub fn default_group() -> String {
    std::env::var("POSH_GROUP").unwrap_or_else(|_| "default".to_string())
}

/// A session the viewport switched away from: its target and the kind it
/// was known to be when left (`current_kind` at push time — the daemon's
/// report, or the created-dispatch fallback; `Unknown` otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackEntry {
    pub target: String,
    pub kind: SessionKind,
}

/// The stack as a VIEW MODEL (design 2026-09-21 §3): the attach in progress
/// and every session beneath it. The only producer;
/// `crate::remote::palette_view` is the only consumer that turns it into
/// RFC 0005 JSON or a heading — so the palette redesign changes that module
/// and nothing here.
///
/// The whole stack rather than a top and a count: the §3.6 notice names
/// every entry, and a separate `top`/`depth` could disagree with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackView {
    /// Sessions *Back* returns through, MOST RECENT FIRST — the order the
    /// notice lists them and the reverse of the internal push order.
    pub below: Vec<StackEntry>,
    pub current: Option<StackEntry>,
}

impl StackView {
    /// The session *Back* returns to.
    pub fn top(&self) -> Option<&StackEntry> {
        self.below.first()
    }

    /// How many sessions *Back* can return through.
    pub fn depth(&self) -> usize {
        self.below.len()
    }
}

// ---------------------------------------------------------------------------
// The viewport state machine.
//
// ONE static, ONE transition function. Everything this front-door process
// knows about where its viewport is and where it has been lives in
// [`VIEWPORT`]; every change goes through [`apply`], which is PURE — it
// mutates the state it is handed and RETURNS the effects for the CALLER to
// perform. Nothing here dials, prints, or touches a socket, so the whole
// machine is table-testable without a daemon, a socket, or a static.
//
// The shape exists to enforce one invariant: **a transition always pushes**.
// There is no public `set_current`, so no caller can record arriving
// somewhere without the session it left going on the stack. That WAS
// expressible — `stack_push_current()` then `set_current()`, two calls in a
// required order — and the FDR 0012 in-place switch did only the second, so
// anonymous sessions vanished from the bookkeeping entirely. Collapsing them
// into [`Event::Entered`] removes the ordering hazard rather than fixing its
// one known victim.

/// The attach in progress: its target, the kind its daemon reported on a
/// frame (`CAP_SESSION_KIND`; `Unknown` until it does), and whether this
/// front door CREATED it as an anonymous session (a `:+` dispatch) — the
/// design §2 fallback when the daemon reports Unknown, since such a session
/// is anonymous by construction. A named `posh start <name>` never sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Current {
    target: String,
    kind: SessionKind,
    anonymous_create: bool,
}

/// The transition a client asked for, waiting for the front door to carry it
/// out. The distinction only matters at [`Event::Entered`]: a switch PUSHES
/// what it leaves, a pop POPS what it returns to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    /// The client named where to go.
    Switch { target: String },
    /// The destination is the stack top, read when the dial is issued —
    /// never captured early, so a cascade that pops again dials the entry
    /// that is actually on top by then.
    Pop,
}

/// An automatic pop in flight. A session that ended returns the viewport to
/// the one beneath it; if THAT one turns out to be gone too the pop repeats,
/// so the chain is accumulated here and reported ONCE when the viewport
/// finally lands (or when the stack runs out).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cascade {
    /// The end that started it — the status the process exits with if every
    /// entry turns out to be dead.
    original: AttachEnd,
    /// The session that ended, then each stack entry whose re-dial failed,
    /// in the order they were found — which is most recently entered first,
    /// the order [`PopNotice::gone`] hands on.
    gone: Vec<StackEntry>,
}

/// Everything one viewport process knows about itself. Private fields: the
/// only ways in are [`apply`] and the read-only queries below.
#[derive(Debug, Default)]
pub struct ViewportState {
    current: Option<Current>,
    /// Sessions switched AWAY from, most recent last. Lives as long as the
    /// process — the `run()` re-attach loop — and no longer.
    stack: Vec<StackEntry>,
    pending: Option<Pending>,
    /// One-shot, consumed by the next [`Event::Entered`].
    anonymous_create: bool,
    /// The client's verdict on the attach that just ended, consumed by the
    /// front door's [`Event::AttachReturned`].
    end: Option<AttachEnd>,
    cascade: Option<Cascade>,
    /// Live overlays, oldest first. VISIBLE state: a renderer that is
    /// spawned but hidden has no entry.
    overlays: Vec<Overlay>,
    /// Why the viewport is here, for the NEXT attach to show once it is
    /// established — set when an automatic pop landed, taken by
    /// [`take_pending_notice`]. State rather than an effect: the attach that
    /// shows it has not begun when the pop lands, so something has to hold
    /// it across the re-dial, and this is the one place both clients read.
    notice: Option<PopNotice>,
}

impl ViewportState {
    const fn new() -> ViewportState {
        ViewportState {
            current: None,
            stack: Vec::new(),
            pending: None,
            anonymous_create: false,
            end: None,
            cascade: None,
            overlays: Vec::new(),
            notice: None,
        }
    }
}

/// Everything that can happen to a viewport. Each is an OBSERVATION by one
/// actor — a client, or the front door — never an instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A client asks to move to `target` (a picker selection). Recorded; the
    /// front door dials once the attach returns.
    SwitchRequested { target: String },
    /// A client asks for *Back*. Recorded as a pop of the stack top; a no-op
    /// with an empty stack.
    PopRequested,
    /// An attach BEGAN on `target` — the one event that moves `current`, and
    /// therefore the one that pushes. Every attach entry point fires it,
    /// including the FDR 0012 in-place re-home.
    Entered { target: String },
    /// The daemon reported the current session's kind (a frame's id 20).
    KindReported(SessionKind),
    /// The client's verdict on the attach that just ended.
    AttachEnded(AttachEnd),
    /// The front door: the attach call returned — decide what happens next.
    AttachReturned,
    /// The front door: dialling `target` failed, so that session is gone.
    DialFailed { target: String },
    /// The next [`Event::Entered`] is an anonymous session this front door
    /// is creating (`posh start :+` / `ph host:+`).
    AnonymousCreateArmed,
    OverlayOpened(&'static str),
    OverlayClosed(&'static str),
}

/// What the caller must do about it. The reducer decides; the caller acts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Attach to this target (the front door's re-dial loop).
    Dial { target: String },
    /// Leave posh with this verdict. `skipped` is non-empty only when a
    /// cascade exhausted the stack: those sessions died unobserved and this
    /// is the only record the user gets of them.
    Exit {
        end: Option<AttachEnd>,
        skipped: Vec<StackEntry>,
    },
    /// The status socket's snapshot changed (RFC 0014 §6).
    RefreshStatus,
}

/// What an automatic pop has to say for itself: why it started, the whole
/// chain that died, and where the viewport landed. Several `gone` entries is
/// the CASCADE case — reported in ONE notice rather than one per step,
/// because that list is the only record of sessions that ended unobserved.
/// `palette_view` turns it into the RFC 0005 §3.6 `notice` view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopNotice {
    /// How the session the viewport was in ended — `gone[0]`'s story. Every
    /// later `gone` entry was found already dead while popping.
    pub ended: AttachEnd,
    /// The session that ended, then each stack entry whose re-dial failed:
    /// most recently entered first, the order the notice lists them.
    pub gone: Vec<StackEntry>,
    pub view: StackView,
}

impl PopNotice {
    /// The one-line form, for a client with no renderer (or one that
    /// predates the §3.6 view): `session flac:s-1 ended (exit 1) — back to
    /// flac:dev`.
    pub fn banner(&self) -> String {
        let why = self.ended.label().unwrap_or_else(|| "ended".to_string());
        let left = self
            .gone
            .iter()
            .map(|e| display_target(&e.target))
            .collect::<Vec<_>>()
            .join(", ");
        let to = self
            .view
            .current
            .as_ref()
            .map_or_else(|| "posh".to_string(), |e| display_target(&e.target));
        format!("session {left} {why} \u{2014} back to {to}")
    }
}

static VIEWPORT: Mutex<ViewportState> = Mutex::new(ViewportState::new());

/// Apply `event` to this process's viewport and hand back the effects. The
/// lock is held only for the transition — an effect is performed outside it,
/// so a `Dial` that re-enters the reducer cannot deadlock.
pub fn dispatch(event: Event) -> Vec<Effect> {
    let mut state = VIEWPORT.lock().unwrap_or_else(|e| e.into_inner());
    apply(&mut state, event)
}

/// The pure transition. `pub(crate)` so the table tests drive it directly,
/// with no static, no socket and no daemon in sight.
pub(crate) fn apply(state: &mut ViewportState, event: Event) -> Vec<Effect> {
    match event {
        Event::SwitchRequested { target } => {
            state.pending = Some(Pending::Switch { target });
            Vec::new()
        }
        Event::PopRequested => {
            if state.stack.is_empty() {
                return Vec::new();
            }
            state.pending = Some(Pending::Pop);
            Vec::new()
        }
        Event::Entered { target } => {
            // The invariant: arriving somewhere and leaving somewhere are
            // ONE operation. A pop takes its destination off the stack; a
            // switch — and a plain first attach, and an in-place re-home —
            // puts what it is leaving on.
            match state.pending.take() {
                Some(Pending::Pop) => {
                    state.stack.pop();
                }
                Some(Pending::Switch { .. }) | None => {
                    if let Some(entry) = current_entry_of(state) {
                        state.stack.push(entry);
                    }
                }
            }
            state.current = Some(Current {
                target,
                kind: SessionKind::Unknown,
                anonymous_create: std::mem::take(&mut state.anonymous_create),
            });
            if let Some(cascade) = state.cascade.take() {
                state.notice = Some(PopNotice {
                    ended: cascade.original,
                    gone: cascade.gone,
                    view: view_of(state),
                });
            }
            vec![Effect::RefreshStatus]
        }
        Event::KindReported(kind) => {
            // A known kind is never downgraded to `Unknown`: an origin that
            // stopped saying does not unsay.
            if kind == SessionKind::Unknown {
                return Vec::new();
            }
            match state.current.as_mut() {
                Some(cur) if cur.kind != kind => {
                    cur.kind = kind;
                    vec![Effect::RefreshStatus]
                }
                _ => Vec::new(),
            }
        }
        Event::AttachEnded(end) => {
            state.end = Some(end);
            Vec::new()
        }
        Event::AttachReturned => {
            let end = state.end.take();
            // An explicit transition wins: the user asked for it, whatever
            // the attach's verdict was.
            if state.pending.is_some() {
                // The target is the stack top for a pop, else the recorded
                // switch — both already known to the caller, which passed
                // it in. Re-read it here so the reducer stays the authority.
                if let Some(target) = pending_target(state) {
                    return vec![Effect::Dial { target }];
                }
            }
            // No transition asked for: a session that ENDED or was LOST
            // returns the viewport to the one beneath it. A quit takes the
            // viewport out, and so does an empty stack.
            let popping = matches!(
                end,
                Some(AttachEnd::Ended { .. }) | Some(AttachEnd::Lost(_))
            );
            match (popping, state.stack.last().cloned()) {
                (true, Some(top)) => {
                    let gone = current_entry_of(state)
                        .into_iter()
                        .collect::<Vec<_>>();
                    state.cascade = Some(Cascade {
                        original: end.expect("popping implies an end"),
                        gone,
                    });
                    state.pending = Some(Pending::Pop);
                    vec![Effect::Dial { target: top.target }]
                }
                _ => vec![Effect::Exit {
                    end,
                    skipped: Vec::new(),
                }],
            }
        }
        Event::DialFailed { target } => {
            let Some(mut cascade) = state.cascade.take() else {
                // An ordinary re-dial that failed: the caller reports the
                // error. Nothing to pop back to and nothing to accumulate.
                state.pending = None;
                return Vec::new();
            };
            // The entry we were dialling is gone too. Take it off the stack,
            // record it, and keep popping.
            let dead = match state.stack.last() {
                Some(top) if top.target == target => state.stack.pop(),
                _ => Some(StackEntry {
                    target,
                    kind: SessionKind::Unknown,
                }),
            };
            cascade.gone.extend(dead);
            match state.stack.last().cloned() {
                Some(top) => {
                    state.cascade = Some(cascade);
                    state.pending = Some(Pending::Pop);
                    vec![Effect::Dial { target: top.target }]
                }
                None => {
                    // The whole stack was dead: exit with the status of the
                    // session that ORIGINALLY ended, naming what was skipped.
                    state.pending = None;
                    vec![Effect::Exit {
                        end: Some(cascade.original),
                        skipped: cascade.gone,
                    }]
                }
            }
        }
        Event::AnonymousCreateArmed => {
            state.anonymous_create = true;
            Vec::new()
        }
        Event::OverlayOpened(kind) => {
            let over = state.current.as_ref().map(|c| c.target.clone());
            state.overlays.push(Overlay { kind, over });
            vec![Effect::RefreshStatus]
        }
        Event::OverlayClosed(kind) => {
            if let Some(i) = state.overlays.iter().rposition(|o| o.kind == kind) {
                state.overlays.remove(i);
            }
            vec![Effect::RefreshStatus]
        }
    }
}

/// The target a recorded transition dials: the stack top for a pop, the
/// session the client named for a switch. The switch target is not stored —
/// the client hands it to `Dial` through [`Event::SwitchRequested`] — so
/// this is only meaningful mid-transition.
fn pending_target(state: &ViewportState) -> Option<String> {
    match &state.pending {
        Some(Pending::Pop) => state.stack.last().map(|e| e.target.clone()),
        Some(Pending::Switch { target }) => Some(target.clone()),
        None => None,
    }
}

/// The attach in progress as a stack entry — what a transition away from it
/// would push.
fn current_entry_of(state: &ViewportState) -> Option<StackEntry> {
    let cur = state.current.as_ref()?;
    Some(StackEntry {
        target: cur.target.clone(),
        kind: effective_kind(cur),
    })
}

/// The kind a session is REPORTED as: what its daemon said if it said, else
/// Anonymous for one this front door created by `:+` (a daemon that predates
/// the kind cannot say, but such a create is anonymous by construction),
/// else Unknown.
fn effective_kind(cur: &Current) -> SessionKind {
    match cur.kind {
        SessionKind::Unknown if cur.anonymous_create => SessionKind::Anonymous,
        kind => kind,
    }
}

fn view_of(state: &ViewportState) -> StackView {
    StackView {
        below: state.stack.iter().rev().cloned().collect(),
        current: current_entry_of(state),
    }
}

/// Read the viewport state. Every query below is one of these.
fn with_state<R>(f: impl FnOnce(&ViewportState) -> R) -> R {
    f(&VIEWPORT.lock().unwrap_or_else(|e| e.into_inner()))
}

// --- the client-facing verbs, each one event ---

/// A client dispatched `session.switch`: record the target for the front
/// door and end the attach. The front door dials it once the attach returns.
pub fn request_switch(target: &str) {
    dispatch(Event::SwitchRequested { target: target.to_string() });
}

/// A client dispatched `session.pop` (*Back*): record a pop of the stack
/// top. Returns the target it will dial, or `None` with an empty stack —
/// the client's cue to say "nothing to go back to" and stay put.
pub fn request_pop() -> Option<String> {
    let target = stack_top()?.target;
    dispatch(Event::PopRequested);
    Some(target)
}

/// A client loop's verdict on the attach it is leaving. The front door
/// consumes it on [`Event::AttachReturned`].
pub fn note_attach_end(end: AttachEnd) {
    dispatch(Event::AttachEnded(end));
}

/// The session *Back* would return to, if any.
pub fn stack_top() -> Option<StackEntry> {
    with_state(|s| s.stack.last().cloned())
}

pub fn stack_view() -> StackView {
    with_state(view_of)
}

/// The notice an automatic pop left for this attach, if any — taken once,
/// by whichever client establishes first ([`ViewportState::notice`]).
pub fn take_pending_notice() -> Option<PopNotice> {
    VIEWPORT.lock().unwrap_or_else(|e| e.into_inner()).notice.take()
}

/// Why an attach ended — noted by the client loop as it returns, read by
/// the front door to decide whether the stack pops on its own (FDR 0016
/// stacked switching: a top session that goes away returns the viewport to
/// the session under it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEnd {
    /// The session itself ended — its shell exited with this status, or its
    /// daemon was told to go (`cause`, when the daemon said; posh#194).
    Ended {
        code: i32,
        cause: Option<posh_proto::caps::SessionEnd>,
    },
    /// The attach lost the session without being asked to leave (the reason).
    Lost(String),
    /// The user quit or detached (a signal counts), or a switch ended it.
    Quit,
}

impl AttachEnd {
    /// The notice phrase: `ended (exit 1)`, `killed (posh kill)`, `ended
    /// (daemon got SIGTERM)`, `lost (mux channel closed)`; `None` for a quit.
    pub fn label(&self) -> Option<String> {
        Some(match self {
            AttachEnd::Ended { code, cause } => cause
                .unwrap_or(posh_proto::caps::SessionEnd::Exited)
                .label(*code),
            AttachEnd::Lost(reason) => format!("lost ({reason})"),
            AttachEnd::Quit => return None,
        })
    }
}

/// A target for a notice: a local `:session` names this machine, like the
/// picker's rows and the default title do.
pub(crate) fn display_target(target: &str) -> String {
    match target.strip_prefix(':') {
        Some(rest) => format!("{}:{rest}", crate::remote::mux::hostname()),
        None => target.to_string(),
    }
}

/// An attach BEGAN on `target`: the ONE way `current` moves, and therefore
/// the one that pushes what it leaves ([`Event::Entered`]). Every attach
/// entry point calls it — including the FDR 0012 in-place re-home, which is
/// why that path can no longer forget the push.
///
/// The FIRST attach of the process also binds the RFC 0014 §6 viewport
/// status socket (design §2); no listing passes here, so no listing binds.
pub fn entered(target: &str) {
    let effects = dispatch(Event::Entered { target: target.to_string() });
    crate::viewport_status::ensure_bound();
    perform_status_effects(&effects);
}

/// Perform the effects a client (rather than the front door) can act on:
/// the status refresh. `Dial` / `Exit` are the front door's, and a client
/// never produces them.
fn perform_status_effects(effects: &[Effect]) {
    if effects.iter().any(|e| matches!(e, Effect::RefreshStatus)) {
        crate::viewport_status::refresh_now();
    }
}

/// Flag the next attach as an anonymous session this front door is creating
/// (the `:+` / create-new dispatch): `cmd_start_local` (only for an
/// anonymous kind) and `start_remote_auto` (always anonymous), right before
/// the entry point that calls [`entered`].
pub fn next_attach_is_anonymous_create() {
    dispatch(Event::AnonymousCreateArmed);
}

/// Whether an anonymous create is armed for the next attach (test seam).
#[cfg(test)]
pub(crate) fn anonymous_create_armed() -> bool {
    with_state(|s| s.anonymous_create)
}

/// Clear this process's viewport state (test seam). The reducer's own tests
/// never need it — they own a [`ViewportState`] outright — but the tests in
/// other modules that drive the real verbs share one process, so each starts
/// from a known state under [`switch_test_guard`].
#[cfg(test)]
pub(crate) fn reset_for_test() {
    *VIEWPORT.lock().unwrap_or_else(|e| e.into_inner()) = ViewportState::new();
}

/// The daemon reported the current session's kind (a frame's id 20 entry).
/// A no-op with no attach in progress; a known kind is never downgraded to
/// `Unknown` (an origin that stopped saying does not unsay).
/// The kind arrives on a frame mid-attach, so the socket's `kind=` line is
/// refreshed here rather than by the front door's loop.
pub fn set_current_kind(kind: SessionKind) {
    let effects = dispatch(Event::KindReported(kind));
    perform_status_effects(&effects);
}

pub fn current() -> Option<String> {
    with_state(|s| s.current.as_ref().map(|c| c.target.clone()))
}

/// The current session's kind: what its daemon reported if it did; else
/// Anonymous for a session this front door created by `:+` (a daemon that
/// predates the kind cannot say, but such a create is anonymous by
/// construction); else `Unknown` (a plain attach or a named start against a
/// silent daemon, or no attach at all).
///
/// Production reads the kind through [`snapshot`] and [`stack_view`], which
/// apply the same [`effective_kind`]; this is the seam the client tests
/// assert the mirroring through.
#[cfg(test)]
pub(crate) fn current_kind() -> SessionKind {
    with_state(|s| s.current.as_ref().map_or(SessionKind::Unknown, effective_kind))
}

/// A renderer view the viewport is showing OVER its session (RFC 0014 §6
/// `overlay` line). `kind` is `palette` (the Commands palette, or a dialog
/// it hosts), `picker` (the session picker — in-session `session.list`,
/// or the standalone `ph` chooser), or `notice` (the must-dismiss pop
/// modal, RFC 0005 §3.6). `over` is the attach the view was opened over
/// (`None` for the standalone chooser).
///
/// The vocabulary is a `&'static str`, not an enum, so nothing here
/// constrains it — RFC 0014 §6's table is where the values are agreed, and
/// a new one belongs there first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub kind: &'static str,
    pub over: Option<String>,
}

/// A view of `kind` became visible over the current attach.
pub fn overlay_open(kind: &'static str) {
    let effects = dispatch(Event::OverlayOpened(kind));
    perform_status_effects(&effects);
}

/// The most recent view of `kind` was dismissed. A no-op when none is live.
pub fn overlay_close(kind: &'static str) {
    let effects = dispatch(Event::OverlayClosed(kind));
    perform_status_effects(&effects);
}

/// Everything the viewport status socket reports (RFC 0014 §6), read in one
/// go so its renderer stays pure: the attach in progress (target, its
/// EFFECTIVE kind — what [`current_kind`] answers and a push would carry —
/// and the anonymous-create flag that explains an `anonymous` reading
/// against a silent daemon), the stack bottom first, and the live overlays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub current: Option<(String, SessionKind, bool)>,
    pub stack: Vec<StackEntry>,
    pub overlays: Vec<Overlay>,
}

pub fn snapshot() -> Snapshot {
    with_state(snapshot_of)
}

fn snapshot_of(state: &ViewportState) -> Snapshot {
    let current = state
        .current
        .as_ref()
        .map(|c| (c.target.clone(), effective_kind(c), c.anonymous_create));
    Snapshot {
        current,
        stack: state.stack.clone(),
        overlays: state.overlays.clone(),
    }
}


/// The title a viewport shows for a session that has set none of its own:
/// `host:session` for the attach in progress — the host as typed minus any
/// `user@` and trailing domain labels (`box` for `me@box.example`), this
/// machine's hostname for a local attach — with the group kept for a
/// non-default one (`box:grp/dev`). `None` before any attach entry point
/// recorded a target. Asserted by both clients on every compose whose model
/// title is empty (a set title always wins), so a switch into a session
/// that never titled itself replaces the previous session's title instead
/// of leaving it stale (the posh#108 first-frame rule leaves an EMPTY title
/// untouched by design).
/// With the session's RFC 0013 §5 foreground process, when the daemon has
/// reported one on the frame (`CAP_SESSION_ACTIVITY`, #193), the title is
/// `host:session · process` — so an auto-named session reads as what it is
/// running, `flac:ff9fe216 · clown`, not just an id.
pub fn default_title_with(process: Option<&str>) -> Option<String> {
    let (host, session) = short_target(&current()?)?;
    let mut title = format!("{host}:{session}");
    if let Some(p) = process.map(str::trim).filter(|p| !p.is_empty()) {
        title.push_str(" \u{b7} ");
        title.push_str(p);
    }
    Some(title)
}

/// A target's `(host, session)` halves abbreviated for a title: the host as
/// typed minus any `user@` and trailing domain labels ([`short_host`]), this
/// machine's hostname for a local `:session`; the session with a UUID name
/// cut short ([`short_session`]). `None` for a string with no `:` (not a
/// target). Shared by the default title and the palette headings
/// (`crate::remote::palette_view`).
pub(crate) fn short_target(target: &str) -> Option<(String, String)> {
    let (dest, session) = target.rsplit_once(':')?;
    let host = if dest.is_empty() {
        crate::remote::mux::hostname()
    } else {
        short_host(dest.rsplit_once('@').map_or(dest, |(_, h)| h))
    };
    Some((host, short_session(session)))
}

/// A `[group/]name` for the title: an auto-generated UUID name (what clown
/// and other spawners hand `posh start`, and the FDR 0011 `:+` form) is cut
/// to its first 8 hex digits, git-abbreviation style — `flac:ff9fe216`
/// rather than a 36-character id — while any other name is kept whole.
/// Until the RFC 0013 §5 activity label rides frames (it reaches only the
/// unattached listing today), the name is all an attached viewport knows.
pub(crate) fn short_session(scoped: &str) -> String {
    let (group, name) = match scoped.rsplit_once('/') {
        Some((g, n)) => (Some(g), n),
        None => (None, scoped),
    };
    let is_uuid = name.len() == 36
        && name
            .char_indices()
            .all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() });
    let name = if is_uuid { &name[..8] } else { name };
    match group {
        Some(g) => format!("{g}/{name}"),
        None => name.to_string(),
    }
}

/// `box.example.com` → `box`; a bracketed / numeric address is kept whole.
pub(crate) fn short_host(host: &str) -> String {
    let literal = host.starts_with('[') || host.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':');
    if literal {
        host.to_string()
    } else {
        host.split('.').next().unwrap_or(host).to_string()
    }
}

// The viewport's own kill stack — `kill_target` / `kill_notice` /
// `remote_kill_argv` — went with the leave and switch-kill flows it existed
// for. Killing a session from a viewport is deferred to v2 session
// management; `posh kill [--unless-attached]` (`session::cmd_kill`) is the
// user-facing command meanwhile, and it never went through here.

#[cfg(test)]
pub(crate) fn switch_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// An empty session stack view (no *Back*), for the palette tests in both
/// clients and `palette_view` — pure, no statics touched.
#[cfg(test)]
pub(crate) fn no_stack() -> StackView {
    StackView { below: Vec::new(), current: None }
}

/// A stack view whose top is `target` (a named session), `depth` deep; the
/// entries under it are placeholders named `:under-N`.
#[cfg(test)]
pub(crate) fn stacked(target: &str, depth: usize) -> StackView {
    let entry = |t: String| StackEntry { target: t, kind: SessionKind::Named };
    StackView {
        below: std::iter::once(entry(target.into()))
            .chain((1..depth).map(|n| entry(format!(":under-{n}"))))
            .take(depth)
            .collect(),
        current: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PhRoute::*;

    #[test]
    fn default_title_is_short_host_colon_session() {
        let _g = switch_test_guard();
        entered(&target_for(Some("me@box.example.com"), None, "dev"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:dev"));
        entered(&target_for(Some("box"), Some("grp"), "dev"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:grp/dev"));
        entered(&target_for(Some("[fe80::1]"), None, "dev"));
        assert_eq!(default_title_with(None).as_deref(), Some("[fe80::1]:dev"));
        entered(&target_for(Some("10.0.0.7"), None, "dev"));
        assert_eq!(default_title_with(None).as_deref(), Some("10.0.0.7:dev"));
        // A local attach names this machine.
        entered(&target_for(None, None, "dev"));
        let local = default_title_with(None).unwrap();
        assert!(local.ends_with(":dev") && local.len() > 4, "{local}");
        // An auto-generated UUID name is abbreviated, group kept; a look-alike
        // that is not a UUID (wrong length / a non-hex digit) stays whole.
        entered(&target_for(Some("box"), None, "ff9fe216-9652-4e23-805c-6f4dd5ce7eca"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:ff9fe216"));
        entered(&target_for(Some("box"), Some("grp"), "ff9fe216-9652-4e23-805c-6f4dd5ce7eca"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:grp/ff9fe216"));
        entered(&target_for(Some("box"), None, "ff9fe216-9652-4e23-805c-6f4dd5ce7ecz"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:ff9fe216-9652-4e23-805c-6f4dd5ce7ecz"));
        // With the daemon's foreground process known, it is appended (#193).
        entered(&target_for(Some("box"), None, "ff9fe216-9652-4e23-805c-6f4dd5ce7eca"));
        assert_eq!(default_title_with(Some("clown")).as_deref(), Some("box:ff9fe216 \u{b7} clown"));
        assert_eq!(default_title_with(Some("  ")).as_deref(), Some("box:ff9fe216"));
    }

    /// Picker rows carry targets that `ph_parse` routes exactly as the typed
    /// form would — local `:name` / `:group/name`, remote `dest:name`, the
    /// create row `:+` / `dest:+` — and cells that label a session by
    /// activity beside its id.
    #[test]
    fn rows_round_trip_through_ph_parse() {
        let entries = vec![
            session::PickerEntry { name: "dev".into(), label: "~/x · nvim".into(), status: "detached".into() },
            session::PickerEntry { name: "s-2".into(), label: String::new(), status: "attached (1)".into() },
        ];
        let local = rows_for(None, "default", &entries);
        assert_eq!(local[0].cells, vec!["~/x · nvim", "dev", "local", "detached"]);
        assert_eq!(local[1].cells[..2], ["", "s-2"], "no label ⇒ blank, the id still names it");
        assert_eq!(
            crate::ph_parse(Some(&local[0].target)),
            LocalResolve { group: None, session: "dev".into() }
        );
        assert_eq!(crate::ph_parse(Some(&local[2].target)), LocalNew { group: None });
        assert_eq!(local[2].cells[0], "+ create new session…");

        let grouped = rows_for(None, "work", &entries[..1]);
        assert_eq!(
            crate::ph_parse(Some(&grouped[0].target)),
            LocalResolve { group: Some("work".into()), session: "dev".into() }
        );

        let remote = rows_for(Some("me@box"), "default", &entries[..1]);
        assert_eq!(remote[0].cells[2], "me@box");
        assert_eq!(
            crate::ph_parse(Some(&remote[0].target)),
            RemoteResolve { user: Some("me".into()), host: "box".into(), group: None, session: "dev".into() }
        );
        assert_eq!(
            crate::ph_parse(Some(&remote[1].target)),
            RemoteNew { user: Some("me".into()), host: "box".into(), group: None }
        );
        assert_eq!(candidates(&remote), "me@box:dev", "create rows are not candidates");

        // The RFC 0005 §3.5 rows: cells verbatim, a session.switch action per row.
        let json = rows_json(&remote);
        assert_eq!(json[0]["cells"][0], "~/x · nvim");
        assert_eq!(json[0]["action"]["method"], "session.switch");
        assert_eq!(json[0]["action"]["params"]["target"], "me@box:dev");
        assert_eq!(json[1]["action"]["params"]["target"], "me@box:+");
        // The attach entry points spell the current target the same way.
        assert_eq!(target_for(None, Some("default"), "dev"), ":dev");
        assert_eq!(target_for(None, None, "dev"), ":dev");
        assert_eq!(target_for(Some("me@box"), Some("work"), "s-1"), "me@box:work/s-1");
    }

    /// The hand-off is a one-shot: the front door dials the recorded
    /// transition once, and nothing is pending afterwards.
    #[test]
    fn the_transition_handoff_is_a_one_shot() {
        let _g = switch_test_guard();
        reset_for_test();
        request_switch("box:dev");
        assert_eq!(
            dispatch(Event::AttachReturned),
            [Effect::Dial { target: "box:dev".into() }]
        );
        entered("box:dev");
        assert_eq!(
            dispatch(Event::AttachReturned),
            [Effect::Exit { end: None, skipped: Vec::new() }],
            "the transition was consumed by entering"
        );
        reset_for_test();
    }

    // --- the reducer ---
    //
    // Table-driven over `(state, event) -> (state, effects)`: each test owns
    // a `ViewportState` outright, so none of them touches a static, a socket
    // or a daemon, and none of them needs `switch_test_guard`.

    /// A state with `stack` stacked (bottom first) and `current` attached.
    fn st(stack: &[(&str, SessionKind)], current: Option<&str>) -> ViewportState {
        ViewportState {
            stack: stack
                .iter()
                .map(|(t, k)| StackEntry { target: (*t).into(), kind: *k })
                .collect(),
            current: current.map(|t| Current {
                target: t.into(),
                kind: SessionKind::Unknown,
                anonymous_create: false,
            }),
            ..ViewportState::new()
        }
    }

    fn targets(stack: &[StackEntry]) -> Vec<&str> {
        stack.iter().map(|e| e.target.as_str()).collect()
    }

    fn ended(code: i32) -> AttachEnd {
        AttachEnd::Ended { code, cause: None }
    }

    /// THE invariant: entering a session always pushes the one being left —
    /// whether the user picked it, typed it, or a daemon re-homed the
    /// viewport in place. Only a POP takes something off instead.
    #[test]
    fn every_transition_pushes_and_only_a_pop_pops() {
        // A switch: the client records it, the front door dials, the attach
        // enters — and `:here` is on the stack because entering pushed it.
        let mut s = st(&[], Some(":here"));
        assert_eq!(apply(&mut s, Event::SwitchRequested { target: ":dev".into() }), []);
        assert_eq!(
            apply(&mut s, Event::AttachReturned),
            [Effect::Dial { target: ":dev".into() }]
        );
        assert_eq!(targets(&s.stack), [] as [&str; 0], "the push happens on Entered, not before");
        apply(&mut s, Event::Entered { target: ":dev".into() });
        assert_eq!(targets(&s.stack), [":here"]);
        assert_eq!(s.current.as_ref().map(|c| c.target.as_str()), Some(":dev"));

        // A pop returns to the top and takes it OFF; it never pushes.
        assert_eq!(apply(&mut s, Event::PopRequested), []);
        assert_eq!(
            apply(&mut s, Event::AttachReturned),
            [Effect::Dial { target: ":here".into() }]
        );
        apply(&mut s, Event::Entered { target: ":here".into() });
        assert!(s.stack.is_empty(), "a pop pops, and does not push what it leaves");
        assert_eq!(s.current.as_ref().map(|c| c.target.as_str()), Some(":here"));

        // A pop with nothing stacked records nothing at all.
        let mut empty = st(&[], Some(":only"));
        assert_eq!(apply(&mut empty, Event::PopRequested), []);
        assert_eq!(empty.pending, None, "nothing to go back to");
    }

    /// The first attach of a process pushes nothing — there is no session to
    /// leave — so no entry point needs a special case for it.
    #[test]
    fn a_first_attach_pushes_nothing() {
        let mut s = ViewportState::new();
        let effects = apply(&mut s, Event::Entered { target: ":first".into() });
        assert_eq!(effects, [Effect::RefreshStatus]);
        assert!(s.stack.is_empty());
        assert_eq!(s.current.as_ref().map(|c| c.target.as_str()), Some(":first"));
    }

    /// DEFECT A, made unrepresentable: the FDR 0012 in-place re-home does not
    /// pass through the front door's re-dial loop, so it never recorded a
    /// push and anonymous sessions vanished from the bookkeeping. It fires
    /// the same `Entered` as every other entry point, and pushing is no
    /// longer a separate call it can omit.
    #[test]
    fn an_in_place_rehome_pushes_what_it_left() {
        let mut s = st(&[], Some(":anon"));
        s.current.as_mut().unwrap().kind = SessionKind::Anonymous;
        // No SwitchRequested: the daemon routed the switch, nothing asked.
        apply(&mut s, Event::Entered { target: ":dev".into() });
        assert_eq!(
            s.stack,
            [StackEntry { target: ":anon".into(), kind: SessionKind::Anonymous }],
            "the re-homed viewport left a session behind; it belongs on the stack"
        );
    }

    /// A session that ENDED or was LOST returns the viewport to the one
    /// beneath it, unasked. A quit takes the viewport out instead, and so
    /// does an empty stack — with the ended session's own status.
    #[test]
    fn an_ended_session_pops_back_and_a_quit_does_not() {
        let mut s = st(&[(":below", SessionKind::Named)], Some(":top"));
        apply(&mut s, Event::AttachEnded(ended(1)));
        assert_eq!(
            apply(&mut s, Event::AttachReturned),
            [Effect::Dial { target: ":below".into() }]
        );
        assert_eq!(
            apply(&mut s, Event::Entered { target: ":below".into() }),
            [Effect::RefreshStatus],
            "the notice is held for the next attach, not an effect of arriving"
        );
        let notice = s.notice.take().expect("a notice for the next attach");
        assert_eq!(notice.ended, ended(1));
        assert_eq!(targets(&notice.gone), [":top"]);
        assert_eq!(notice.view.current.as_ref().map(|e| e.target.as_str()), Some(":below"));
        let banner = notice.banner();
        assert!(banner.contains("ended (exit 1)"), "{banner}");
        assert!(banner.contains("back to"), "{banner}");

        // A quit never pops, whatever is stacked.
        let mut q = st(&[(":below", SessionKind::Named)], Some(":top"));
        apply(&mut q, Event::AttachEnded(AttachEnd::Quit));
        assert_eq!(
            apply(&mut q, Event::AttachReturned),
            [Effect::Exit { end: Some(AttachEnd::Quit), skipped: Vec::new() }]
        );
        assert_eq!(targets(&q.stack), [":below"], "the stack goes with the viewport");

        // Nothing stacked: the ended session's status becomes the exit.
        let mut bare = st(&[], Some(":only"));
        apply(&mut bare, Event::AttachEnded(ended(3)));
        assert_eq!(
            apply(&mut bare, Event::AttachReturned),
            [Effect::Exit { end: Some(ended(3)), skipped: Vec::new() }]
        );

        // A LOST session pops too — its daemon may well still be alive.
        let mut lost = st(&[(":below", SessionKind::Named)], Some(":top"));
        apply(&mut lost, Event::AttachEnded(AttachEnd::Lost("mux channel closed".into())));
        assert_eq!(
            apply(&mut lost, Event::AttachReturned),
            [Effect::Dial { target: ":below".into() }]
        );
    }

    /// An explicit transition beats an automatic one: the user asked to go
    /// somewhere, so the viewport goes there even though the session ended.
    #[test]
    fn a_requested_transition_wins_over_the_automatic_pop() {
        let mut s = st(&[(":below", SessionKind::Named)], Some(":top"));
        apply(&mut s, Event::SwitchRequested { target: ":elsewhere".into() });
        apply(&mut s, Event::AttachEnded(ended(1)));
        assert_eq!(
            apply(&mut s, Event::AttachReturned),
            [Effect::Dial { target: ":elsewhere".into() }]
        );
        assert!(s.cascade.is_none(), "an asked-for move is not a cascade");
    }

    /// THE CASCADE: a pop whose target is gone pops again, skipping the dead
    /// ones, and reports the WHOLE chain in ONE notice — that list is the
    /// only record the user gets of sessions that died unobserved.
    #[test]
    fn a_dead_pop_target_keeps_popping_and_reports_the_chain_once() {
        let mut s = st(
            &[
                (":bottom", SessionKind::Named),
                (":middle", SessionKind::Anonymous),
            ],
            Some(":top"),
        );
        apply(&mut s, Event::AttachEnded(ended(1)));
        assert_eq!(
            apply(&mut s, Event::AttachReturned),
            [Effect::Dial { target: ":middle".into() }]
        );
        // `:middle` is gone too: pop past it rather than erroring.
        assert_eq!(
            apply(&mut s, Event::DialFailed { target: ":middle".into() }),
            [Effect::Dial { target: ":bottom".into() }]
        );
        assert_eq!(targets(&s.stack), [":bottom"], "the dead entry came off");
        // `:bottom` answers: ONE notice naming the whole chain.
        apply(&mut s, Event::Entered { target: ":bottom".into() });
        let notice = s.notice.take().expect("a notice for the next attach");
        assert_eq!(notice.ended, ended(1), "the chain's story is the first end");
        assert_eq!(
            targets(&notice.gone),
            [":top", ":middle"],
            "one notice for the whole chain, not one per step"
        );
        assert!(s.stack.is_empty());
        assert_eq!(s.current.as_ref().map(|c| c.target.as_str()), Some(":bottom"));
    }

    /// A stack that is dead all the way down exits with the status of the
    /// session that ORIGINALLY ended, and names what it skipped on the way.
    #[test]
    fn a_fully_dead_stack_exits_with_the_original_status() {
        let mut s = st(&[(":gone", SessionKind::Anonymous)], Some(":top"));
        apply(&mut s, Event::AttachEnded(ended(7)));
        apply(&mut s, Event::AttachReturned);
        let effects = apply(&mut s, Event::DialFailed { target: ":gone".into() });
        match &effects[..] {
            [Effect::Exit { end, skipped }] => {
                assert_eq!(end.as_ref(), Some(&ended(7)), "the FIRST end is the exit status");
                assert_eq!(targets(skipped), [":top", ":gone"]);
            }
            other => panic!("expected a single Exit, got {other:?}"),
        }
    }

    /// A plain re-dial that failed is not a cascade: the reducer hands back
    /// nothing and the front door reports the error.
    #[test]
    fn a_failed_switch_outside_a_cascade_yields_no_effects() {
        let mut s = st(&[], Some(":here"));
        apply(&mut s, Event::SwitchRequested { target: ":typo".into() });
        apply(&mut s, Event::AttachReturned);
        assert_eq!(apply(&mut s, Event::DialFailed { target: ":typo".into() }), []);
        assert_eq!(s.pending, None, "the transition is over, failed");
    }

    /// The design §2 fallback: an anonymous create (`:+`) against a daemon
    /// that never says reads Anonymous; a plain attach reads Unknown; a
    /// daemon that DOES say wins; a known kind is never downgraded. The flag
    /// is one-shot, consumed by the `Entered` it precedes.
    #[test]
    fn the_kind_is_reported_never_downgraded_and_falls_back_for_a_create() {
        let mut s = ViewportState::new();
        apply(&mut s, Event::AnonymousCreateArmed);
        assert!(s.anonymous_create);
        apply(&mut s, Event::Entered { target: ":s-9".into() });
        assert!(!s.anonymous_create, "one-shot: consumed by Entered");
        assert_eq!(current_entry_of(&s).map(|e| e.kind), Some(SessionKind::Anonymous));
        // The daemon's own report wins over the fallback.
        assert_eq!(
            apply(&mut s, Event::KindReported(SessionKind::Named)),
            [Effect::RefreshStatus]
        );
        assert_eq!(current_entry_of(&s).map(|e| e.kind), Some(SessionKind::Named));
        // Unknown never downgrades, and re-reporting the same kind is a no-op.
        assert_eq!(apply(&mut s, Event::KindReported(SessionKind::Unknown)), []);
        assert_eq!(apply(&mut s, Event::KindReported(SessionKind::Named)), []);
        assert_eq!(current_entry_of(&s).map(|e| e.kind), Some(SessionKind::Named));
        // A plain attach clears both the report and the fallback.
        apply(&mut s, Event::Entered { target: ":dev".into() });
        assert_eq!(current_entry_of(&s).map(|e| e.kind), Some(SessionKind::Unknown));
        // With no attach at all the report is a no-op.
        let mut none = ViewportState::new();
        assert_eq!(apply(&mut none, Event::KindReported(SessionKind::Named)), []);
        assert_eq!(current_entry_of(&none), None);
    }

    /// A pushed entry carries the kind the session was known to be when it
    /// was left — the daemon's report, or the create fallback.
    #[test]
    fn a_pushed_entry_carries_the_kind_it_was_left_with() {
        let mut s = ViewportState::new();
        apply(&mut s, Event::AnonymousCreateArmed);
        apply(&mut s, Event::Entered { target: ":anon".into() });
        apply(&mut s, Event::Entered { target: ":next".into() });
        assert_eq!(
            s.stack,
            [StackEntry { target: ":anon".into(), kind: SessionKind::Anonymous }]
        );
        apply(&mut s, Event::KindReported(SessionKind::Named));
        apply(&mut s, Event::Entered { target: ":third".into() });
        assert_eq!(s.stack[1].kind, SessionKind::Named);
    }

    /// Overlays record VISIBLE views over the attach they opened on; a close
    /// removes the most recent of its kind and a stray close is a no-op.
    #[test]
    fn overlays_track_visible_views() {
        let mut s = st(&[], Some(":ovl"));
        assert_eq!(apply(&mut s, Event::OverlayClosed("palette")), [Effect::RefreshStatus]);
        assert!(s.overlays.is_empty(), "a stray close is a no-op");
        apply(&mut s, Event::OverlayOpened("palette"));
        apply(&mut s, Event::OverlayOpened("picker"));
        assert_eq!(
            s.overlays.iter().map(|o| o.kind).collect::<Vec<_>>(),
            ["palette", "picker"]
        );
        assert_eq!(s.overlays[0].over.as_deref(), Some(":ovl"));
        apply(&mut s, Event::OverlayClosed("palette"));
        assert_eq!(s.overlays.iter().map(|o| o.kind).collect::<Vec<_>>(), ["picker"]);
    }

    /// The snapshot the RFC 0014 §6 status socket renders: the attach with
    /// its EFFECTIVE kind, the stack bottom first, the live overlays.
    #[test]
    fn the_snapshot_reads_everything_the_status_socket_reports() {
        let mut s = ViewportState::new();
        apply(&mut s, Event::Entered { target: ":s-1".into() });
        apply(&mut s, Event::KindReported(SessionKind::Named));
        apply(&mut s, Event::AnonymousCreateArmed);
        apply(&mut s, Event::Entered { target: ":ovl".into() });
        apply(&mut s, Event::OverlayOpened("picker"));
        let snap = snapshot_of(&s);
        assert_eq!(snap.current, Some((":ovl".into(), SessionKind::Anonymous, true)));
        assert_eq!(
            snap.stack,
            [StackEntry { target: ":s-1".into(), kind: SessionKind::Named }]
        );
        assert_eq!(snap.overlays.iter().map(|o| o.kind).collect::<Vec<_>>(), ["picker"]);
        assert_eq!(snapshot_of(&ViewportState::new()).current, None);
    }

    /// The view model carries the WHOLE stack, most recent first — `top` and
    /// `depth` are read off it, so they cannot disagree with the entries.
    #[test]
    fn the_stack_view_lists_every_entry_most_recent_first() {
        assert_eq!(view_of(&ViewportState::new()), no_stack());
        let s = st(
            &[(":a", SessionKind::Anonymous), (":b", SessionKind::Named)],
            Some(":here"),
        );
        let v = view_of(&s);
        assert_eq!(targets(&v.below), [":b", ":a"], "pushed :a then :b, so :b is on top");
        assert_eq!(v.depth(), 2);
        assert_eq!(v.top().map(|e| e.target.as_str()), Some(":b"));
        assert_eq!(v.current.as_ref().map(|e| e.target.as_str()), Some(":here"));
    }

    /// The end labels the banner and the exit line are built from (posh#194).
    #[test]
    fn attach_end_labels_say_what_happened() {
        use posh_proto::caps::SessionEnd;
        assert_eq!(ended(1).label().as_deref(), Some("ended (exit 1)"));
        assert_eq!(
            AttachEnd::Ended { code: 129, cause: Some(SessionEnd::Killed) }
                .label()
                .as_deref(),
            Some("killed (posh kill)")
        );
        assert_eq!(
            AttachEnd::Ended { code: 143, cause: Some(SessionEnd::Signaled(15)) }
                .label()
                .as_deref(),
            Some("ended (daemon got SIGTERM)")
        );
        assert_eq!(
            AttachEnd::Lost("mux channel closed".into()).label().as_deref(),
            Some("lost (mux channel closed)")
        );
        assert_eq!(AttachEnd::Quit.label(), None);
    }

    /// The banner spells a local `:session` with this machine's name, like
    /// every other notice and the picker's rows.
    #[test]
    fn the_pop_banner_names_the_host() {
        let notice = PopNotice {
            ended: ended(1),
            gone: vec![StackEntry { target: ":s-1".into(), kind: SessionKind::Anonymous }],
            view: StackView {
                below: Vec::new(),
                current: Some(StackEntry { target: ":s-2".into(), kind: SessionKind::Named }),
            },
        };
        let banner = notice.banner();
        assert!(banner.contains(" ended (exit 1) "), "the end phrases itself: {banner}");
        assert!(banner.starts_with("session "), "{banner}");
        assert!(!banner.contains(" :s-1"), "a local target names this machine: {banner}");
        assert!(banner.ends_with(&display_target(":s-2")), "{banner}");
    }

}
