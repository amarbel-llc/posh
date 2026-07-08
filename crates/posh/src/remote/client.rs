//! Roaming remote client (mosh-client/stmclient port): raw-mode tty, a
//! reliable input stream upload, a local terminal model rebuilt from
//! server frames, speculative local echo (predict.rs), and a minimal-diff
//! renderer (display.rs) so frames morph the screen without flicker.

use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Instant;

use posh_term::Terminal;
use serde_json::{json, Value};

use crate::pty::{self, RawMode};
use crate::remote::caps;
use crate::remote::crypto::Key;
use crate::remote::datagram::{Connection, Family};
use crate::remote::diag;
use crate::remote::display::{self, NotificationEngine, Snapshot};
use crate::remote::framesync::{self, ApplyOutcome, FrameApplier};
use crate::remote::kittykeys::{PaletteKeyNormalizer, ESCAPE_KEY};
use crate::remote::palette::{composite_palette, Palette, PaletteEvent};
use crate::remote::predict::{
    self, PredictionModel, PredictionRenderer, Predictor, RenderStyle,
};
use crate::remote::scrollview::{self, MouseFilter};
use crate::remote::stats::{FrameKind, PredictSample, Stats};
use crate::remote::sync::{
    self, ClientMessage, FragmentAssembly, Fragmenter, FrameBody, InputOutbox, ScrollbackRing,
    ServerFrame, HEARTBEAT_INTERVAL,
};
use crate::util::{self, now_ms, Error, Result};

const STDIN: i32 = libc::STDIN_FILENO;
const STDOUT: i32 = libc::STDOUT_FILENO;
const SHUTDOWN_GRACE: u64 = 5000; // ms to wait for the shutdown ack

/// Sticky banner raised when the server reports FLAG_WEDGE (the organic wedge
/// watchdog fired, #wedge). Sticky so it survives a fast self-recovery — the
/// user notices even if the stall already passed; dismissed on the next keystroke.
const WEDGE_BANNER: &str = "posh: session stall detected - captured to server log (press any key)";

/// Depth of the client's local scrollback ring (RFC 0002 §3), in rows.
/// Matches the server's default primary ring so a durable local reader can
/// hold roughly what the server syncs; bounds client memory.
const SCROLLBACK_RING_DEPTH: usize = 10_000;

/// The escape-passthrough key: a literal `^` after Ctrl-^ sends one Ctrl-^ to
/// the session (mosh parity). The Ctrl-^ summon key itself is
/// [`crate::remote::kittykeys::ESCAPE_KEY`] (`0x1e`), shared with the local client.
const ESCAPE_PASS_KEY: u8 = b'^';

/// Banner shown only when Ctrl-^ can't open the palette (renderer missing or
/// wedged): the degraded escape prefix, where Ctrl-^ . still quits.
const PALETTE_FALLBACK_HELP: &str =
    "command palette unavailable — \".\" quits, \"^\" gives literal Ctrl-^";

/// Rebuild the predictor/renderer for `next` in place and force a clean repaint
/// so stale predicted cells clear; banners the new model. Backs the palette's
/// `echo.set` action.
fn apply_echo_model(st: &mut ClientState, next: PredictionModel, now: u64) {
    let (predict, renderer) = predict::build(next, st.predict_render, st.predict_overwrite);
    st.predict = predict;
    st.renderer = renderer;
    st.predict_model = next;
    st.stats.set_gp_active(is_gp_species(next));
    st.initialized = false;
    st.notify.set_message(&format!("echo: {next:?}"), false, now);
}

/// Whether a prediction model is one of the evolved GP species (RFC 0007). The
/// metric bus assembles only for these, and their compute timing is collected
/// even with periodic logging off.
fn is_gp_species(model: PredictionModel) -> bool {
    matches!(
        model,
        PredictionModel::Controller | PredictionModel::FromScratch
    )
}

/// Set client-local debug logging to `enabled`: open the default per-pid sink
/// (reusing the SIGUSR2-dump path scheme) or close it, and flip the stats
/// collector so the periodic transport summaries flow while it is on. A no-op
/// (no banner) when already in the requested state.
fn set_logging(st: &mut ClientState, enabled: bool, now: u64) {
    if enabled == util::log_active() {
        return;
    }
    if enabled {
        let path = diag::enable_logging("client");
        st.stats.set_enabled(true);
        st.notify
            .set_message(&format!("debug logging: on ({})", path.display()), false, now);
    } else {
        util::log_disable();
        st.stats.set_enabled(false);
        st.notify.set_message("debug logging: off", false, now);
    }
}

/// The palette's command list (RFC 0005 §5): the discoverable surface for the
/// escape commands. Ctrl-^ opens this in lieu of a key-prefix menu, so every
/// escape action lives here. The logging entries reflect the current state —
/// client logging from this process, server logging from the last frame's
/// FLAG_SERVER_LOG (`server_log_on`).
fn palette_commands(server_log_on: bool, scroll_opt: bool) -> Value {
    // Imperative labels (the verb is the action): "on"/"off" read ambiguously as
    // status, so a user who saw "…: on" assumed it was already enabled.
    let (client_log_name, client_log_enabled): (&str, bool) = if util::log_active() {
        ("Disable client debug logging", false)
    } else {
        ("Enable client debug logging", true)
    };
    let (server_log_name, server_log_enabled): (&str, bool) = if server_log_on {
        ("Disable server debug logging", false)
    } else {
        ("Enable server debug logging", true)
    };
    // posh#100 diagnostic toggle: disabling the scroll-region optimization
    // forces full per-row repaints, avoiding the DECSTBM region scroll that
    // leaves a stuck background on some terminals.
    let (scroll_opt_name, scroll_opt_enabled): (&str, bool) = if scroll_opt {
        ("Disable scroll-region optimization", false)
    } else {
        ("Enable scroll-region optimization", true)
    };
    json!([
        { "name": "Echo: adaptive", "action": { "method": "echo.set", "params": { "model": "adaptive" } } },
        { "name": "Echo: optimistic", "action": { "method": "echo.set", "params": { "model": "optimistic" } } },
        { "name": "Echo: always", "action": { "method": "echo.set", "params": { "model": "always" } } },
        { "name": "Echo: never", "action": { "method": "echo.set", "params": { "model": "never" } } },
        // RFC 0007 evolved-predictor pilot: select `controller` to turn the GP
        // predictor on (it falls back to the adaptive shadow until it earns the
        // display, §7.1); pick any other Echo entry to turn it back off.
        { "name": "Echo: controller (evolved GP)", "action": { "method": "echo.set", "params": { "model": "controller" } } },
        { "name": "Echo: from-scratch (evolved GP)", "action": { "method": "echo.set", "params": { "model": "scratch" } } },
        { "name": client_log_name, "action": { "method": "logging.set", "params": { "enabled": client_log_enabled } } },
        { "name": server_log_name, "action": { "method": "logging.set", "params": { "scope": "server", "enabled": server_log_enabled } } },
        { "name": scroll_opt_name, "action": { "method": "render.scroll_opt", "params": { "enabled": scroll_opt_enabled } } },
        { "name": "Shell out (server)", "action": { "method": "shell.open" } },
        { "name": "Reset & resync (force redraw)", "action": { "method": "session.resync" } },
        { "name": "Dump wedge forensics", "action": { "method": "session.forensics" } },
        { "name": "Show wedge debug info", "action": { "method": "session.debuginfo" } },
        // #false-disconnect: the transport-liveness view — why the "Last contact"
        // banner fired (frame-arrival gaps, heartbeats, retransmits, srtt/rto),
        // distinct from the apply-stall wedge view above.
        { "name": "Show connection health", "action": { "method": "session.linkinfo" } },
        // RFC 0007: the local-echo prediction state — outcome gauges for every
        // model, plus the live evolution-loop stats (generations, champion,
        // hyphence champion record) when a GP species is selected.
        { "name": "Show echo prediction stats", "action": { "method": "session.predictinfo" } },
        { "name": "Show agent-forwarding debug info", "action": { "method": "session.agentinfo" } },
        { "name": "Suspend client", "action": { "method": "client.suspend" } },
        { "name": "Quit session", "action": { "method": "app.quit" } },
    ])
}

/// Summon the command palette: spawn the renderer on first use, then show it.
/// Returns whether it opened (false if the renderer can't be spawned, leaving
/// the caller to fall back to the emergency-quit prefix).
fn open_palette(st: &mut ClientState) -> bool {
    if st.palette.is_none() {
        st.palette = Palette::spawn(st.rows, st.cols);
    }
    let commands = palette_commands(st.server_log_on, st.scroll_opt);
    if let Some(p) = st.palette.as_mut() {
        // A persisted (spawned-then-closed) palette is not resized while closed,
        // so re-sync it to the current tty size before summoning — else it
        // renders at the size it had when last open, misaligned against a
        // since-resized screen (posh#135).
        p.resize(st.rows, st.cols);
        p.open("Commands", commands);
        st.initialized = false; // repaint to show the overlay
        true
    } else {
        false
    }
}

/// Write the full client transport snapshot — and a byte-level forensic bundle
/// if an apply-stall is pending — to the diagnostic sink. Shared by the SIGUSR2
/// handler and the "Show wedge debug info" palette command (#3), so both record
/// the identical forensic state on disk.
fn dump_client_state(st: &ClientState, now: u64) {
    let ps = predict_sample(&st.predict.stats());
    diag::ClientState {
        remote: st.conn.remote(),
        last_send_age_ms: (st.last_send != 0).then(|| now.saturating_sub(st.last_send)),
        last_heard_age_ms: now.saturating_sub(st.last_heard),
        applied_num: st.applied_num,
        outbox_base: st.outbox.base(),
        outbox_pending: st.outbox.pending().len(),
        scrollback_len: st.scrollback.len(),
        srtt: st.conn.srtt(),
        rto: st.conn.rto(),
        send_interval: st.conn.send_interval(),
        bytes_rx: st.conn.bytes_rx(),
        bytes_tx: st.conn.bytes_tx(),
        predict_active: ps.active,
        predict_shown: ps.shown,
        predict_epoch_lag: ps.epoch_lag,
        term_gen: st.server_term.generation(),
        rows: st.rows,
        cols: st.cols,
        echo_on: st.echo_on,
        codec: st.framesync.label(),
        title: st.server_term.title().to_string(),
        apply: st.stats.apply_snapshot(),
        link: st.stats.link_snapshot(),
        server_late: st.notify.server_late(now),
        server_diag: st.last_server_diag,
    }
    .dump();
    if let Some(reack) = st.last_reack.as_ref() {
        let _ = diag::capture_forensics(st.applied_num, &st.applied_data, reack);
    }
}

/// The compact on-screen wedge-triage line for the "Show wedge debug info"
/// palette command (#3): the handful of fields that distinguish a wedge's cause,
/// readable without a second terminal because the notify banner composites even
/// while the session content is frozen (the client loop, palette, and renderer
/// all keep running — only the apply path is stuck). `reack=yes` is the
/// apply-stall signal; `srv=` shows the far side when the client is in a debug
/// posture so the server piggyback (CAP_DIAG, #6) was negotiated, else `off`.
fn wedge_debug_summary(st: &ClientState, now: u64) -> String {
    let srv = match st.last_server_diag {
        Some(d) => format!(
            "(pid={} num={} acked={} out={} pty={})",
            d.pid, d.current_num, d.acked_num, d.outstanding, d.pty_open as u8
        ),
        None => "off".to_string(),
    };
    format!(
        "debug: pid={} applied={} heard={}ms gen={} reack={} srv={}",
        std::process::id(),
        st.applied_num,
        now.saturating_sub(st.last_heard),
        st.server_term.generation(),
        if st.last_reack.is_some() { "yes" } else { "no" },
        srv,
    )
}

/// The connection-health summary for the "Connection health" palette command
/// (#false-disconnect): the transport-LIVENESS fields, distinct from the
/// apply-stall wedge view above. It answers "why did posh think the server
/// disconnected?" — the banner (`late=`) keys purely on the gap since the last
/// decoded frame (`heard=`) crossing `SERVER_LATE_AFTER`, NOT on measured loss.
/// So a large `gap_max` / nonzero `late_gaps` on a link with healthy
/// `retransmits` and steady `heartbeats` (Empty frames, the RFC 0008 §3
/// keepalive) is the false-disconnect fingerprint: the banner tripped on a
/// lost/late heartbeat while the session was alive underneath. `srtt`/`rto`
/// contextualize the pacing; `send_iv` is the client's own send cadence.
/// Composites over a frozen session like the wedge view — readable in-session
/// without a second terminal or knowing the pid.
fn link_debug_summary(st: &ClientState, now: u64) -> String {
    let l = st.stats.link_snapshot();
    let heard = now.saturating_sub(st.last_heard);
    let late = st.notify.server_late(now);
    format!(
        "link: pid={} remote={} heard={heard}ms late={} (banner>{}ms)\n\
         rx: total={} full={} diff={} heartbeats={} scrollback={} retransmits={}\n\
         gaps: max={}ms late_gaps={} srtt={:.0}ms rto={}ms send_iv={}ms",
        std::process::id(),
        st.conn
            .remote()
            .map_or_else(|| "none".to_string(), |a| a.to_string()),
        if late { "yes" } else { "no" },
        display::SERVER_LATE_AFTER,
        l.frames_total,
        l.frames_full,
        l.frames_diff,
        l.heartbeats_rx,
        l.frames_scrollback,
        l.retransmits,
        l.frame_gap_ms_max,
        l.frame_gaps_late,
        st.conn.srtt(),
        st.conn.rto(),
        st.conn.send_interval(),
    )
}

/// Two-line SSH agent-forwarding diagnostic (FDR 0004). The client line: whether
/// forwarding is configured + the local agent socket, whether the peer advertised
/// `CAP_AGENT_FORWARD` (the "is the server forwarding at all?" signal — `no` here
/// is the most common misconfig), the live forwarded-channel count, and the agent
/// byte-stream offsets (`out_base` / unacked `pending` / `in_ack`; a growing
/// `pending` with a stuck `in_ack` means the peer is not consuming the stream).
/// The server line: the remote `AgentEndpoint`'s own channel count, next
/// channel id, and well-known-symlink health, forwarded over `CAP_DIAG`. Backs the
/// palette `session.agentinfo` action.
fn agent_debug_summary(st: &ClientState) -> String {
    let Some(agent) = st.agent.as_ref() else {
        return "agent-fwd: off (no local SSH agent, or disabled by policy)".to_string();
    };
    let client = format!(
        "agent-fwd: on sock={} peer-advertised={} channels={} out_base={} pending={}B in_ack={}",
        agent.source().display(),
        if st.agent_seen { "yes" } else { "no" },
        agent.live_channel_count(),
        st.agent_stream.send_base(),
        st.agent_stream.pending().len(),
        st.agent_stream.recv_ack(),
    );
    // The server endpoint's own state, forwarded over CAP_DIAG (FDR 0004).
    // Absent until the first diag frame arrives; `agent: None` means the
    // server is not forwarding (or predates this extension).
    let server = match st.last_server_diag.and_then(|d| d.agent) {
        Some(a) => format!(
            "server: endpoint=up channels={} next_chan={} symlink={}",
            a.live_channels,
            a.next_channel_id,
            if a.symlink_ok { "ok" } else { "broken" },
        ),
        None if st.last_server_diag.is_some() => {
            "server: endpoint=down (no server-side forwarding, or an older server)".to_string()
        }
        None => "server: (state not yet received)".to_string(),
    };
    format!("{client}\n{server}")
}

/// The local-echo prediction summary for the "Show echo prediction stats"
/// palette command (RFC 0007): the selected model and which predictor is
/// actually displayed, the outcome gauges every model reports, and — when an
/// evolved GP species is running — the evolution-loop state: generations
/// stepped, population/window sizes, the champion's rank/size and its §7.1
/// standing vs the adaptive shadow, and the champion hyphence-doc record under
/// `$XDG_DATA_HOME` (§8).
fn predict_debug_summary(st: &ClientState) -> String {
    let ps = st.predict.stats();
    let (correct, nocredit, incorrect) = ps.outcomes;
    let mut out = format!(
        "echo model: {:?}\noutcomes: correct={correct} nocredit={nocredit} incorrect={incorrect} \
         resets={} epoch_lag={} shown_cells={} srtt_trigger={}",
        st.predict_model,
        ps.mispredict_resets,
        ps.epoch_lag,
        ps.shown_cells,
        if ps.srtt_trigger { "on" } else { "off" },
    );
    match st.predict.evolution() {
        Some(ev) => {
            out.push_str(&format!(
                "\nevolution: gen={} pop={} window={} displayed={} streak={:+}\
                 \nchampion: rank={} size={} nodes, saves={}",
                ev.generations,
                ev.population,
                ev.window,
                if ev.champion_displayed {
                    "champion (GP)"
                } else {
                    "shadow (adaptive floor)"
                },
                ev.champion_streak,
                ev.champion_rank,
                ev.champion_size,
                ev.champion_saves,
            ));
            match ev.last_champion_doc {
                Some(p) => out.push_str(&format!("\nchampion doc: {}", p.display())),
                None => out.push_str("\nchampion doc: (none written yet)"),
            }
        }
        None => out.push_str("\nevolution: inactive (select an evolved GP echo model)"),
    }
    out
}

/// Present a debug-info summary (#99): in a copyable, dismissable palette dialog
/// when the renderer is available, else the notification banner as a fallback so
/// the command never silently no-ops. Either surface composites over the session,
/// so it stays readable even while the session is frozen.
fn show_debug_info(st: &mut ClientState, title: &str, body: &str, now: u64) {
    if let Some(p) = st.palette.as_mut() {
        p.show_dialog(title, body);
    } else {
        st.notify.set_message(body, false, now);
    }
}

/// Dispatch a palette-selected command action (RFC 0005 §7). Returns whether the
/// client should send to the server promptly (the escape-to-shell flag or quit).
fn dispatch_palette_action(
    st: &mut ClientState,
    raw: &RawMode,
    method: &str,
    params: &Value,
    now: u64,
) -> bool {
    match method {
        "echo.set" => {
            if let Some(model) = params
                .get("model")
                .and_then(Value::as_str)
                .and_then(|m| PredictionModel::parse(Some(m)).ok())
            {
                apply_echo_model(st, model, now);
            }
            false
        }
        "logging.set" => {
            let enabled = params.get("enabled").and_then(Value::as_bool);
            if params.get("scope").and_then(Value::as_str) == Some("server") {
                // Server-side toggle (#3): request the state change over the wire;
                // the server applies it and reports back via FLAG_SERVER_LOG.
                if let Some(en) = enabled {
                    st.flags |= if en {
                        sync::CLIENT_FLAG_LOG_ON
                    } else {
                        sync::CLIENT_FLAG_LOG_OFF
                    };
                    let msg = if en {
                        "server debug logging: on (requested)"
                    } else {
                        "server debug logging: off (requested)"
                    };
                    st.notify.set_message(msg, false, now);
                }
                return true; // send the request flag promptly
            }
            if let Some(en) = enabled {
                set_logging(st, en, now); // client-local (default scope)
            }
            false
        }
        "render.scroll_opt" => {
            // posh#100 diagnostic: flip new_frame's scroll-shortcut optimization
            // and force a full repaint so the new mode takes effect immediately.
            if let Some(en) = params.get("enabled").and_then(Value::as_bool) {
                st.scroll_opt = en;
                st.initialized = false;
                st.notify.set_message(
                    if en {
                        "scroll-region optimization: on"
                    } else {
                        "scroll-region optimization: off"
                    },
                    false,
                    now,
                );
            }
            false
        }
        "shell.open" => {
            // FDR 0008 escape-to-shell: a one-shot sticky flag the next message
            // carries; the server spawns the overlay shell in the session cwd.
            st.flags |= sync::CLIENT_FLAG_ESCAPE;
            st.notify.set_message("opening shell\u{2026}", true, now);
            true
        }
        "session.resync" => {
            // Force the server to send a Full keyframe (#wedge): the client is
            // wedged rejecting diffs against a base it isn't at and the automatic
            // stale-ack -> Full recovery didn't fire. One-shot flag like
            // CLIENT_FLAG_ESCAPE; the server drops its acked baseline so the next
            // frame is a Full this client applies unconditionally.
            st.flags |= sync::CLIENT_FLAG_RESYNC;
            st.notify.set_message("resyncing\u{2026}", true, now);
            true
        }
        "session.forensics" => {
            // Write a byte-level apply-stall forensic bundle on demand (#90/#94).
            // Only meaningful while wedged (a reack is pending); otherwise says so.
            let msg = match st.last_reack.as_ref() {
                Some(reack) => {
                    match diag::capture_forensics(st.applied_num, &st.applied_data, reack) {
                        Some(path) => format!("wedge forensics: {}", path.display()),
                        None => "wedge forensics: write failed".to_string(),
                    }
                }
                None => "wedge forensics: nothing pending (not wedged)".to_string(),
            };
            st.notify.set_message(&msg, false, now);
            false
        }
        "session.debuginfo" => {
            // Show the wedge-triage info (visible even while frozen) and write the
            // full transport snapshot to the diagnostic sink (#3), so the user can
            // read the cause in-session and keep the forensic record without a
            // second terminal or knowing the pid.
            let summary = wedge_debug_summary(st, now);
            dump_client_state(st, now);
            show_debug_info(st, "wedge debug", &summary, now);
            false
        }
        "session.linkinfo" => {
            // #false-disconnect: show the transport-liveness view (why the "Last
            // contact" banner fired) and mirror the full snapshot to the sink —
            // same pattern as session.debuginfo, but the liveness class rather
            // than the apply-stall class. Readable while frozen; no wire send.
            let summary = link_debug_summary(st, now);
            dump_client_state(st, now);
            show_debug_info(st, "connection health", &summary, now);
            false
        }
        "session.predictinfo" => {
            // RFC 0007: show the local-echo prediction state (model, outcome
            // gauges, evolution-loop stats + champion record) and log it, so
            // the evolved predictor can be inspected from the palette
            // in-session, without a debug sink or a second terminal.
            let summary = predict_debug_summary(st);
            util::log_write("predictinfo", &summary);
            show_debug_info(st, "echo prediction", &summary, now);
            false
        }
        "session.agentinfo" => {
            // FDR 0004: show the client-side agent-forwarding state and log it, so
            // forwarding can be diagnosed from the palette in-session.
            let summary = agent_debug_summary(st);
            util::log_write("agentinfo", &summary);
            show_debug_info(st, "agent forwarding", &summary, now);
            false
        }
        "client.suspend" => {
            suspend(st, raw);
            false
        }
        "app.quit" => {
            request_shutdown(st);
            true
        }
        _ => false, // unknown method: the renderer already closed; ignore
    }
}

/// $POSH_GRAB_MOUSE: whether to grab the wheel on the outer terminal when the
/// session app has no mouse mode of its own, translating wheel-up/down into
/// arrow keys client-side. Off by default — grabbing costs the outer
/// terminal's native click-to-select. See posh#50/#3/#28; the faithful
/// wheel→scrollback behavior is posh#43.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrabMouse {
    Off,
    On,
}

impl GrabMouse {
    fn parse(value: Option<&str>) -> Result<GrabMouse> {
        match value {
            None | Some("") | Some("off") | Some("never") | Some("0") | Some("false") => {
                Ok(GrabMouse::Off)
            }
            Some("on") | Some("always") | Some("1") | Some("true") => Ok(GrabMouse::On),
            Some(other) => Err(Error::Msg(format!("unknown POSH_GRAB_MOUSE setting ({other})"))),
        }
    }
}

pub fn run(
    host: &str,
    port: u16,
    family: Family,
    agent_source: Option<std::path::PathBuf>,
) -> Result<()> {
    util::check_utf8_locale("posh-client")?;

    // mosh convention: the key travels in the environment, never on argv
    // (argv is world-readable via ps).
    let key_str = std::env::var("POSH_KEY")
        .map_err(|_| Error::from("POSH_KEY environment variable not set"))?;
    std::env::remove_var("POSH_KEY");
    let key = Key::from_base64(key_str.trim())?;

    // Model selection: $POSH_PREDICTION_MODEL, falling back to the deprecated
    // $POSH_PREDICTION alias. Render style: $POSH_PREDICTION_RENDER (default
    // replace).
    let model_env = std::env::var("POSH_PREDICTION_MODEL")
        .ok()
        .or_else(|| std::env::var("POSH_PREDICTION").ok());
    let model = PredictionModel::parse(model_env.as_deref()).map_err(Error::Msg)?;
    let render_env = std::env::var("POSH_PREDICTION_RENDER").ok();
    let render = RenderStyle::parse(render_env.as_deref()).map_err(Error::Msg)?;
    let predict_overwrite = std::env::var("POSH_PREDICTION_OVERWRITE")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let grab_mouse = GrabMouse::parse(std::env::var("POSH_GRAB_MOUSE").ok().as_deref())?;

    let addr = resolve(host, port, family)?;
    let conn = Connection::client(addr, &key)?;

    // Handlers go in before raw mode and the takeover write: the first
    // byte on the tty is the outside world's readiness signal, and a
    // SIGTERM racing it must find the handler installed, not the default
    // disposition (github #48).
    util::install_client_signal_handlers();
    // SIGUSR2 dumps live transport state on demand (remote::diag); installed
    // here (not in the shared client bundle) so only the roaming client, which
    // consumes the flag in drive_client, arms it.
    util::install_sigusr2_handler();
    let raw = RawMode::enable(STDIN)?;
    // Take over the alternate screen (mosh smcup); close() below restores
    // the user's pre-connect shell screen on the way out.
    write_display_control("smcup (connect)", &display::open());
    let result = client_loop(
        conn,
        model,
        render,
        predict_overwrite,
        grab_mouse,
        &raw,
        addr.port(),
        agent_source,
        host,
    );
    write_display_control("rmcup (exit)", &display::close());
    drop(raw);
    eprintln!("\nposh: [client exited]");
    // Carry the remote session's exit status (EXIT_STATUS capability,
    // RFC 0001 §3) into our own, mirroring the local attach path (#18).
    match result {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code),
        Err(e) => Err(e),
    }
}

fn resolve(host: &str, port: u16, family: Family) -> Result<SocketAddr> {
    // System resolver first — this honors Tailscale MagicDNS when tailscaled
    // has wired it into the resolver (the default on most hosts).
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .unwrap_or_default();
    let pick = match family {
        Family::Inet => addrs.iter().find(|a| a.is_ipv4()),
        Family::Inet6 => addrs.iter().find(|a| a.is_ipv6()),
        // Prefer IPv4 (the common path for roaming UDP), fall back to v6.
        Family::Auto => addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first()),
    };
    if let Some(addr) = pick.copied() {
        return Ok(addr);
    }

    // Fallback: a tailnet MagicDNS name the system resolver couldn't reach
    // (MagicDNS off, a container, split-DNS). `tailnet::resolve` shells out to
    // `tailscale status --json` and degrades to None when unavailable.
    if let Some(ip) = crate::tailnet::resolve(host) {
        let family_ok = match family {
            Family::Inet => ip.is_ipv4(),
            Family::Inet6 => ip.is_ipv6(),
            Family::Auto => true,
        };
        if family_ok {
            return Ok(SocketAddr::new(ip, port));
        }
    }

    Err(Error::Msg(format!(
        "could not resolve {host} (system resolver and tailnet)"
    )))
}

struct ClientState {
    conn: Connection,
    fragmenter: Fragmenter,
    outbox: InputOutbox,
    rows: u16,
    cols: u16,
    flags: u8,
    last_send: u64,
    /// Wall-clock ms when we last decoded a frame from the server. Its age
    /// (now - last_heard) distinguishes a transport-dead freeze (server silent /
    /// path down) from an apply-stall (frames arriving, none applied) -- the
    /// first thing to check on a wedge. Mirrors the server's own last_heard.
    last_heard: u64,
    // Frame 0 is the implicit empty initial state.
    applied_num: u64,
    applied_data: Vec<u8>,
    /// Server screen state, rebuilt from the latest applied frame.
    server_term: Terminal,
    /// Local, partial, monotonically-growing accumulation of the session's
    /// primary-screen scrollback (RFC 0002 §3). Fed by `BODY_SCROLLBACK`
    /// frames; survives `Full` visible resets; cleared on a width resize.
    /// Rendered by the wheel scroll-view (FDR 0005): `compose_scroll_frame`
    /// reads a window of it when `scroll_offset > 0`.
    scrollback: ScrollbackRing,
    /// Set on resize to drop the `SCROLLBACK` advertisement for exactly the
    /// next outgoing message (RFC 0002 §4: a resize ceases scrollback so the
    /// server restarts appended-row counting afresh at the new width).
    suppress_scrollback_once: bool,
    /// Scrollback stream v2 (RFC 0009): the epoch we are accumulating in —
    /// adopted from the server's SCROLLBACK2 ack cap, `None` until the first
    /// ack arrives or after a local resize (stale in-flight bodies are
    /// discarded until the server, seeing our new size, opens a new epoch).
    sb2_epoch: Option<u8>,
    /// Cumulative v2 rows accepted this epoch (`T`, RFC 0009 §3) — includes
    /// rows the ring has since evicted; reported as `acked_sb_rows` in every
    /// outgoing SCROLLBACK2 entry. Never advances `applied_num`.
    sb2_rows: u64,
    /// What the physical tty currently shows.
    last_drawn: Snapshot,
    /// False when the outer terminal state is unknown (startup, resize,
    /// Ctrl-L): the next frame repaints from scratch.
    initialized: bool,
    /// The wheel intent of the last live render, so the shared renderer tears
    /// the wheel-grab down (or re-arms it) on a want_wheel transition that is
    /// not also a mouse_mode change — an app entering the alt-screen without a
    /// mouse mode (github #106).
    last_wheel: bool,
    predict: Box<dyn Predictor>,
    renderer: Box<dyn PredictionRenderer>,
    /// Cached prediction config so the model can be rebuilt live (Ctrl-^ e
    /// cycles it). The trait objects above are swapped; these record what to
    /// rebuild from.
    predict_model: PredictionModel,
    predict_render: RenderStyle,
    predict_overwrite: bool,
    /// Latest metric vector (RFC 0007), reassembled each compose while a GP
    /// species is active; the seam the evolved program reads once wired.
    #[allow(dead_code)] // consumed once the GP program is wired (RFC 0007 §7)
    last_metrics: predict::MetricVector,
    /// Latest decoded `CAP_METRICS` remote terminals (RFC 0007 §3), folded into
    /// `last_metrics` each compose. `NaN` until the server forwards them.
    remote_metrics: [f64; caps::METRICS_FIELDS],
    notify: NotificationEngine,
    /// $POSH_GRAB_MOUSE policy; on, intercepted wheel events become arrow keys
    /// instead of driving the scrollback scroll-view (the legacy posh#50 grab).
    grab_mouse: GrabMouse,
    /// Byte-fed state machine over the intercepted wheel: reports scroll ticks
    /// (default) or translates to arrows (grab); its persistent state
    /// reassembles sequences split across reads (posh#52).
    mouse_filter: MouseFilter,
    /// Rewrites a kitty-CSI-u palette key (`\x1b[54;5u`) to raw `0x1e` before the
    /// wheel filter and the byte loop, so Ctrl-^ opens the palette under kitty
    /// keyboard mode too (posh#131). Its carry reassembles a CSI-u split across
    /// reads. Sibling to `mouse_filter`: both are per-connection input state.
    palette_keys: PaletteKeyNormalizer,
    quit_pending: bool,
    shutdown_requested: bool,
    shutdown_requested_at: u64,
    shutdown_seen: bool,
    /// Remote session exit code from the EXIT_STATUS capability on the
    /// shutdown frame; 0 against baseline servers or on user-quit.
    exit_status: i32,
    /// (applied_num, server_term generation) at the last compose, plus
    /// whether any overlay was live then — the idle fast-path key. github #35.
    last_render_state: (u64, u64),
    last_render_overlays: bool,
    /// #wedge (#83): server_term generation at the last NON-empty tty paint, and
    /// the last generation a fast-path-skip-while-unpainted line was logged for
    /// (edge-trigger so the freeze doesn't firehose). A fast-path skip where
    /// `server_term.generation() != last_painted_gen` means content sits in the
    /// model that never reached the tty — the render-skip freeze fingerprint.
    last_painted_gen: u64,
    last_skip_log_gen: u64,
    /// Scroll-view position (FDR 0005): rows the viewport top sits above the
    /// live bottom. 0 = live view; > 0 freezes the live view and renders a
    /// window of `scrollback` via `compose_scroll_frame`.
    scroll_offset: usize,
    /// Idle fast-path key for the scroll view: (offset, ring len, generation)
    /// at the last scroll compose. None forces the next scroll frame to repaint.
    last_scroll_state: Option<(usize, usize, u64)>,
    /// Latest server-reported remote-PTY ECHO state (FLAG_ECHO). Gates
    /// optimistic local echo (FDR 0006); defaults off until the first frame.
    echo_on: bool,
    /// Latest server-reported debug-logging state (FLAG_SERVER_LOG, #3); drives
    /// the palette's "Server debug logging" command label. Off until reported.
    server_log_on: bool,
    /// Whether `new_frame`'s scroll-shortcut optimization is enabled (default
    /// on). The palette's "scroll-region optimization" toggle flips it; off
    /// forces full per-row repaints, avoiding the DECSTBM region scroll that
    /// leaves a stuck background on some terminals (posh#100).
    scroll_opt: bool,
    /// Pending keystroke timestamps for the input-latency gauge: (outbox
    /// end-offset after a stdin read, queue time ms). Drained as the server's
    /// `input_ack` covers each offset. Capped so a stalled link can't grow it.
    input_sent: VecDeque<(u64, u64)>,
    /// Frame-sync codec selection (`POSH_FRAMESYNC`, #15). Drives whether we
    /// advertise `CAP_MORPH` and which `applier` we route visible-frame bodies
    /// through. Defaults to DumpDiff (today's behavior) when the env is unset.
    framesync: framesync::FrameSync,
    /// Client-side codec that applies a received visible-frame body to
    /// `server_term`. DumpDiff reparses a fresh model; MorphDelta morphs the
    /// existing one in place (and falls back to a reparse for `Full` keyframes).
    applier: Box<dyn FrameApplier>,
    /// Optional performance instrumentation (POSH_DEBUG_LOG); inert when unset.
    stats: Stats,
    /// The command-palette overlay renderer (Ctrl-^ p), spawned lazily on first
    /// summon and kept resident; `None` until then or if it can't be launched.
    palette: Option<Palette>,
    /// The last visible-frame body the applier rejected via `ReackAndWait`
    /// (#90/#94 forensics): `(rx_num, rx_base, kind, body_bytes)`. Captured to
    /// disk once per wedge episode so the divergent base can be analysed
    /// offline (`diag::capture_forensics`).
    last_reack: Option<(u64, u64, FrameKind, Vec<u8>)>,
    /// One-shot guard so a wedge writes a single forensic bundle, not one per
    /// retransmit (the reack loop fires constantly); reset when apply advances.
    forensic_captured: bool,
    /// Whether to advertise CAP_DIAG so the server piggybacks its transport
    /// state (#6). True only in a debug posture (POSH_DEBUG_LOG set, or
    /// POSH_WEDGE_WATCHDOG EXPLICITLY on — the watchdog default (#117) does not
    /// count), so a default session never asks and the server pays no per-frame
    /// overhead. Computed once from `stats` at startup.
    want_server_diag: bool,
    /// Latest server transport state from CAP_DIAG (#6), surfaced in the SIGUSR2
    /// dump so a wedge shows both sides. `None` until the server first reports
    /// (only when we advertised CAP_DIAG); never sent on a default session.
    last_server_diag: Option<caps::ServerDiag>,
    /// SSH agent forwarding (FDR 0004): the local-agent proxy + the
    /// bidirectional agent byte stream, and whether the server has advertised
    /// AGENT_FORWARD yet (gates our own AGENT_DATA/ACK; caps don't persist, so
    /// this latches once seen). `None` proxy == forwarding off this connection.
    agent: Option<crate::remote::agent::AgentClient>,
    agent_stream: sync::AgentStream,
    agent_seen: bool,
    /// Per-request agent-use notice (FDR 0004; #96): the rate-limited banner
    /// shown when a forwarded-agent channel opens. `Some` only when forwarding
    /// is active (it rides on the proxy and owns the host name it reports).
    agent_notice: Option<crate::remote::agent::AgentNotice>,
    /// Edge-detect latch for the server's FLAG_WEDGE (#wedge): true once the
    /// sticky stall banner has been raised for the current capture episode, reset
    /// when the flag clears so a later episode re-raises it.
    wedge_seen: bool,
}

#[allow(clippy::too_many_arguments)]
fn client_loop(
    conn: Connection,
    model: PredictionModel,
    render: RenderStyle,
    predict_overwrite: bool,
    grab_mouse: GrabMouse,
    raw: &RawMode,
    port: u16,
    agent_source: Option<std::path::PathBuf>,
    host: &str,
) -> Result<i32> {
    util::set_nonblocking(STDIN)?;

    let (rows, cols) = pty::term_size(STDOUT);
    let now = now_ms();
    let (predict, renderer) = predict::build(model, render, predict_overwrite);
    // Frame-sync codec (#15): opt into MorphDelta with POSH_FRAMESYNC=morph;
    // unset/empty/other stays on DumpDiff (today's behavior, default-off).
    let framesync = framesync::FrameSync::parse(std::env::var("POSH_FRAMESYNC").ok().as_deref());
    let applier = framesync.applier();
    // Request server transport-state piggyback (#6) only in a debug posture, so
    // a default session never negotiates it. The watchdog RECOVERY is default-on
    // (#117), so the posture signal is an EXPLICIT POSH_WEDGE_WATCHDOG, not the
    // watchdog being armed.
    let stats = Stats::new();
    let want_server_diag = stats.enabled() || stats.wedge_watchdog_explicit();
    let mut st = ClientState {
        conn,
        fragmenter: Fragmenter::new(),
        outbox: InputOutbox::new(),
        rows,
        cols,
        flags: 0,
        last_send: 0,
        last_heard: now,
        applied_num: 0,
        applied_data: Vec::new(),
        server_term: Terminal::with_scrollback(rows, cols, 0),
        scrollback: ScrollbackRing::new(SCROLLBACK_RING_DEPTH),
        suppress_scrollback_once: false,
        sb2_epoch: None,
        sb2_rows: 0,
        last_drawn: Snapshot::blank(rows, cols),
        initialized: false,
        last_wheel: false,
        predict,
        renderer,
        predict_model: model,
        predict_render: render,
        predict_overwrite,
        last_metrics: predict::MetricVector::unavailable(),
        remote_metrics: [f64::NAN; caps::METRICS_FIELDS],
        notify: NotificationEngine::new(now),
        grab_mouse,
        mouse_filter: MouseFilter::default(),
        palette_keys: PaletteKeyNormalizer::default(),
        quit_pending: false,
        shutdown_requested: false,
        shutdown_requested_at: 0,
        shutdown_seen: false,
        exit_status: 0,
        last_render_state: (u64::MAX, u64::MAX),
        last_render_overlays: false,
        last_painted_gen: 0,
        last_skip_log_gen: u64::MAX,
        scroll_offset: 0,
        last_scroll_state: None,
        echo_on: false,
        server_log_on: false,
        scroll_opt: true,
        input_sent: VecDeque::new(),
        framesync,
        applier,
        stats,
        palette: None,
        last_reack: None,
        forensic_captured: false,
        want_server_diag,
        last_server_diag: None,
        // Agent forwarding (FDR 0004): the proxy forwards the resolved source
        // socket; `None` source == forwarding off this connection. The notice
        // (FDR 0004; #96) rides with the proxy — armed only when forwarding is
        // on, reading POSH_AGENT_NOTICE for its silence default and owning the
        // host name it reports.
        agent_notice: agent_source
            .as_ref()
            .map(|_| crate::remote::agent::AgentNotice::from_env(host)),
        agent: agent_source.map(crate::remote::agent::AgentClient::new),
        agent_stream: sync::AgentStream::new(),
        agent_seen: false,
        wedge_seen: false,
    };
    // RFC 0007: collect the compute-timing terminals when a GP species is the
    // startup model, independent of POSH_DEBUG_LOG.
    st.stats.set_gp_active(is_gp_species(model));
    let result = drive_client(&mut st, raw, port);
    // Tear down the palette renderer (if any) before the final stats flush.
    if let Some(p) = st.palette.take() {
        p.shutdown();
    }
    // One final summary regardless of how the loop exited (graceful, timeout,
    // or error), so the log always ends with the last-observed transport state.
    let now = now_ms();
    let predict_stats = st.predict.stats();
    st.stats.final_client(
        now,
        st.conn.srtt(),
        st.conn.rto(),
        st.conn.send_interval(),
        predict_sample(&predict_stats),
        predict_stats.srtt_trigger,
        st.conn.bytes_rx(),
        st.conn.bytes_tx(),
    );
    result
}

/// Drives the client event loop until detach, shell exit, timeout, or error.
/// Split from `client_loop` so the final stats flush runs on every exit path.
fn drive_client(st: &mut ClientState, raw: &RawMode, port: u16) -> Result<i32> {
    let mut assembly = FragmentAssembly::new();

    // Connect diagnostics (mosh stmclient): before the first authentic
    // frame, hint after 250ms and give up after POSH_CONNECT_TMOUT seconds
    // (default 15, 0 disables) instead of waiting forever on a firewalled
    // port or a server that failed to start.
    let started = now_ms();
    let connect_timeout: u64 = std::env::var("POSH_CONNECT_TMOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|s| s.saturating_mul(1000))
        .unwrap_or(15_000);
    let mut heard = false;

    // Hello: teaches the server our address and terminal size.
    send_message(st);

    let result: Result<i32> = 'client: loop {
        let iter_start = st.stats.instrument().then(Instant::now);
        let now = now_ms();
        let mut deadline = st.last_send + HEARTBEAT_INTERVAL;
        if !st.outbox.is_empty() || st.flags != 0 {
            deadline = deadline.min(st.last_send + st.conn.rto());
        }
        deadline = deadline.min(now + st.notify.wait_time(now));
        if st.predict.needs_timer() {
            // Outstanding predictions need 50ms ticks for glitch detection.
            deadline = deadline.min(now + 50);
        }
        if !heard {
            // Pre-contact: tick for the 250ms hint / connect timeout.
            deadline = deadline.min(now + 250);
        }
        let timeout = deadline.saturating_sub(now).min(1000) as i32;

        let mut fds = vec![
            util::pollfd(STDIN, libc::POLLIN),
            util::pollfd(st.conn.raw_fd(), libc::POLLIN),
        ];
        // Poll the palette renderer's fds (its PTY + control socket) while it is
        // resident, so its output drains and selections are seen promptly.
        let palette_base = st.palette.as_ref().map(|p| {
            let base = fds.len();
            fds.push(util::pollfd(p.master_fd(), libc::POLLIN));
            fds.push(util::pollfd(p.ctrl_fd(), libc::POLLIN));
            base
        });
        // Agent-forwarding channel fds (FDR 0004): the local-agent connections
        // the proxy has open. `agent_base`/count map their `revents` back.
        let (agent_base, agent_count) = match &st.agent {
            Some(a) => {
                let agent_fds = a.pollfds();
                let base = fds.len();
                fds.extend_from_slice(&agent_fds);
                (base, agent_fds.len())
            }
            None => (usize::MAX, 0),
        };
        let mut send_now = false;
        let poll_start = st.stats.instrument().then(Instant::now);
        match util::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => break 'client Err(e.into()),
        }
        let idle_us = poll_start.map_or(0, |t| t.elapsed().as_micros() as u64);

        if util::take_flag(&util::SIGWINCH_RECEIVED) {
            let size = pty::term_size(STDOUT);
            // A SIGWINCH does not guarantee the geometry changed; terminals and
            // multiplexers deliver redundant same-size WINCHes. Acting on those
            // dropped the scrollback ring and forced a resync for no reason (the
            // no-detach backscroll reset, posh#134). Skip the whole body when the
            // size is unchanged; only a real resize reflows/repaints/re-syncs.
            let changed = size != (st.rows, st.cols);
            if util::log_active() {
                util::log_write(
                    "winch",
                    &format!(
                        "SIGWINCH {}x{} -> {}x{} changed={changed}",
                        st.rows, st.cols, size.0, size.1,
                    ),
                );
            }
            if changed {
                st.rows = size.0;
                st.cols = size.1;
                if let Some(p) = st.palette.as_mut() {
                    p.resize(st.rows, st.cols);
                }
                st.predict.reset();
                st.initialized = false; // full repaint at the new size
                // RFC 0002 §4: a width change rewraps the server's ring, so
                // absolute row continuity ends. Drop the accumulated ring,
                // discard the (not-yet-built) scroll view by virtue of the
                // repaint, and stop advertising SCROLLBACK for the resize
                // message so the server restarts appended-row counting afresh.
                st.scrollback.clear();
                st.scroll_offset = 0; // FDR 0005: a resize returns to the live view
                st.suppress_scrollback_once = true;
                // v2 (RFC 0009): expect a fresh epoch — the server, seeing our
                // new size, bumps it; until its ack arrives, in-flight v2 bodies
                // from the superseded row space are discarded (unknown epoch).
                st.sb2_epoch = None;
                st.sb2_rows = 0;
                send_now = true;
            }
        }

        if util::take_flag(&util::SIGTERM_RECEIVED) {
            // SIGTERM/SIGINT/SIGHUP: wind down through the normal shutdown
            // handshake so run() restores the tty and the server hangs up
            // the shell instead of lingering until the network timeout.
            request_shutdown(st);
            send_now = true;
        }

        if util::take_flag(&util::SIGCONT_RECEIVED) {
            // Resumed after SIGSTOP/fg: the outer terminal state is unknown.
            st.initialized = false;
        }

        // SIGUSR2: snapshot live transport state to the diagnostic sink. Goes to
        // a file, never the tty — stdout is the alternate-screen TUI and stderr
        // is the user's outer shell, so writing either would corrupt the display.
        if util::take_flag(&util::SIGUSR2_RECEIVED) {
            dump_client_state(st, now);
        }

        // Keystrokes -> quit sequence / prediction / reliable input stream.
        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(STDIN, &mut buf) {
                Ok(0) => {
                    // EOF on the local tty: ask the server to wind down.
                    request_shutdown(st);
                    send_now = true;
                }
                Ok(n) => {
                    if process_user_input(st, &buf[..n]) {
                        send_now = true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break 'client Err(e.into()),
            }
        }

        // Server frames.
        if fds[1].revents & libc::POLLIN != 0 {
            loop {
                match st.conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                            continue;
                        };
                        let Some(assembled) = assembly.add(frag) else {
                            continue;
                        };
                        let Ok(frame) = ServerFrame::decode(&assembled) else {
                            continue;
                        };
                        st.last_heard = now_ms();
                        if !heard {
                            heard = true;
                            if st.notify.message().starts_with("Nothing received") {
                                st.notify.set_message("", false, now_ms());
                            }
                        }
                        if process_frame(st, &frame) {
                            send_now = true; // ack the new state promptly
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // Agent-forwarding channels (FDR 0004): read local-agent reply bytes
        // and frame them onto the outbound agent stream. `read_channels` scans
        // every channel, so a single signalled agent fd drives it.
        if agent_base != usize::MAX {
            if let Some(agent) = st.agent.as_mut() {
                let readable = (agent_base..agent_base + agent_count)
                    .any(|i| fds[i].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0);
                if readable {
                    for rec in agent.read_channels() {
                        st.agent_stream.send(&rec);
                        send_now = true; // flush agent chunks promptly (design §2)
                    }
                }
            }
        }

        // Palette overlay: drain the renderer's screen so the next compose
        // reflects it, and act on any selection/cancel it reported.
        if let Some(base) = palette_base {
            if fds[base].revents & libc::POLLIN != 0 {
                if let Some(p) = st.palette.as_mut() {
                    p.pump();
                }
            }
            if fds[base + 1].revents & libc::POLLIN != 0 {
                match st.palette.as_mut().map(Palette::poll_events) {
                    Some(PaletteEvent::Action { method, params }) => {
                        if dispatch_palette_action(st, raw, &method, &params, now_ms()) {
                            send_now = true;
                        }
                        st.initialized = false; // palette closed -> repaint session
                    }
                    Some(PaletteEvent::Cancelled) => {
                        st.initialized = false; // palette closed -> repaint session
                    }
                    Some(PaletteEvent::Copy) => {
                        // Dialog copy (#99): the renderer can't reach the real
                        // terminal, so emit the OSC 52 here with the body we sent.
                        let body = st
                            .palette
                            .as_ref()
                            .map(|p| p.dialog_body().to_string())
                            .unwrap_or_default();
                        if !body.is_empty() {
                            let osc = format!(
                                "\x1b]52;c;{}\x1b\\",
                                posh_term::base64::encode(body.as_bytes())
                            );
                            let _ = util::write_all_retry(STDOUT, osc.as_bytes(), 1000);
                        }
                    }
                    _ => {}
                }
            }
        }

        let now = now_ms();
        if !heard {
            let waited = now.saturating_sub(started);
            if connect_timeout > 0 && waited >= connect_timeout {
                break 'client Err(Error::Msg(format!(
                    "Timed out waiting for server on UDP port {port}."
                )));
            }
            if waited >= 250 && st.notify.message().is_empty() {
                st.notify.set_message(
                    &format!("Nothing received from server on UDP port {port}."),
                    true,
                    now,
                );
            }
        }
        render(st, now);
        // Apply-stall detector (#wedge): a visible model frozen past the
        // threshold while diff frames keep arriving. Detection is unconditional
        // and cheap; the log line inside is POSH_DEBUG_LOG-gated.
        let wedged = st.stats.check_wedge(
            now,
            st.server_term.generation(),
            st.applied_num,
            st.framesync.label(),
        );
        if wedged && st.stats.wedge_watchdog() {
            // #117 default-on auto-recovery (opt out: POSH_WEDGE_WATCHDOG=0): a
            // frozen model with frames still arriving is a stall some silent-drop
            // apply path let through (stale-drop, scrollback/base mismatch,
            // repeated dup/nochange, a transport-side stall) — the uniform net
            // over the whole #95/#117 class. Capture forensics if a body is
            // pending, arm the diagnostic sink, and force a resync to break it.
            // Fires once per episode (check_wedge latches until the model
            // advances), so a long freeze cannot storm resyncs.
            if let Some(reack) = st.last_reack.as_ref() {
                let _ = diag::capture_forensics(st.applied_num, &st.applied_data, reack);
            }
            let _ = diag::enable_logging("client");
            st.flags |= sync::CLIENT_FLAG_RESYNC;
        }
        let predict_stats = st.predict.stats();
        st.stats.flush_client(
            now,
            st.conn.srtt(),
            st.conn.rto(),
            st.conn.send_interval(),
            predict_sample(&predict_stats),
            predict_stats.srtt_trigger,
            st.conn.bytes_rx(),
            st.conn.bytes_tx(),
        );

        if send_now
            || ((!st.outbox.is_empty() || st.flags != 0)
                && now.saturating_sub(st.last_send) >= st.conn.rto())
            || now.saturating_sub(st.last_send) >= HEARTBEAT_INTERVAL
        {
            send_message(st);
        }

        if st.shutdown_seen {
            // Shell exited (or our quit was acknowledged); the final-state
            // ack went out just above.
            break 'client Ok(st.exit_status);
        }
        if st.shutdown_requested && now.saturating_sub(st.shutdown_requested_at) >= SHUTDOWN_GRACE {
            break 'client Ok(0); // server unreachable; leave anyway
        }

        // Per-iteration loop timing (perf instrumentation): busy = the whole
        // iteration minus the poll wait.
        if let Some(start) = iter_start {
            let total = start.elapsed().as_micros() as u64;
            st.stats.record_loop_iter(total.saturating_sub(idle_us), idle_us);
        }
    };
    result
}

/// mosh stmclient.cc suspend sequence: restore the outer terminal and the
/// tty driver, stop our process group, and on SIGCONT re-enter raw mode and
/// force a full repaint.
fn suspend(st: &mut ClientState, raw: &RawMode) {
    let _ = util::write_all_retry(STDOUT, &display::close(), 1000);
    raw.restore();
    let _ = util::write_all_retry(STDOUT, b"\r\n\x1b[37;44m[posh is suspended.]\x1b[m\r\n", 1000);
    util::stop_own_pgroup();
    // Execution resumes here after SIGCONT (fg): back onto the alternate
    // screen before the forced repaint below redraws it.
    raw.reapply();
    let _ = util::write_all_retry(STDOUT, &display::open(), 1000);
    st.predict.reset();
    st.initialized = false;
}

fn request_shutdown(st: &mut ClientState) {
    if !st.shutdown_requested {
        st.shutdown_requested = true;
        st.shutdown_requested_at = now_ms();
        st.flags |= sync::CLIENT_FLAG_SHUTDOWN;
        st.notify
            .set_message("Exiting on user request...", true, now_ms());
    }
}

/// Whether the client intercepts the outer terminal's wheel right now (delegates
/// to the shared [`scrollview::wheel_active`]): the inner app has set no mouse
/// mode of its own AND it is on the primary screen. True at a bare prompt —
/// where the wheel drives the scrollback scroll-view (FDR 0005), or the legacy
/// wheel→arrow grab when `POSH_GRAB_MOUSE=on` (posh#50).
fn wheel_active(st: &ClientState) -> bool {
    scrollview::wheel_active(&st.server_term)
}

/// Sets the scroll-view offset via the shared [`scrollview::set_scroll`],
/// clamped to the ring depth. On a real change the shared helper invalidates the
/// scroll memo; here we additionally invalidate the live-render memo (a remote
/// client concern) so the next live render repaints on return to offset 0.
fn set_scroll(st: &mut ClientState, offset: usize) {
    let ring_len = st.scrollback.len();
    if scrollview::set_scroll(&mut st.scroll_offset, &mut st.last_scroll_state, ring_len, offset) {
        st.last_render_state = (u64::MAX, u64::MAX);
    }
}

/// Applies wheel ticks to the scroll offset via the shared
/// [`scrollview::scroll_by`]: + = up (into history), - = down (toward live).
fn scroll_by(st: &mut ClientState, ticks: i32) {
    let ring_len = st.scrollback.len();
    if scrollview::scroll_by(&mut st.scroll_offset, &mut st.last_scroll_state, ring_len, ticks) {
        st.last_render_state = (u64::MAX, u64::MAX);
    }
}

/// Whether local echo is safe to show right now: the remote PTY is echoing
/// (server-reported FLAG_ECHO) and the primary screen is active (not a
/// full-screen app). The optimistic model uses this (via `set_echo_safe`) to
/// suppress echo for passwords and TUIs; other models ignore it.
fn optimistic_echo_on(st: &ClientState) -> bool {
    st.echo_on && !st.server_term.is_alt_screen()
}

// The wheel-intercept `MouseFilter` (with its `FilterOut` and `MAX_MOUSE_SEQ`)
// now lives in `remote::scrollview` so the local session frame client shares one
// implementation; it is imported at the module top and stored in
// `ClientState::mouse_filter`.

/// Feeds user bytes through the Ctrl-^ quit-sequence state machine, the
/// prediction engine, and into the reliable input stream. Returns true when
/// anything needs sending.
fn process_user_input(st: &mut ClientState, buf: &[u8]) -> bool {
    let now = now_ms();
    let mut dirty = false;

    // Dismiss the sticky wedge banner (#wedge) on the user's next keystroke: they
    // have seen it, and typing is also the action that tends to break the stall.
    // Gated on the message so it only clears our own banner, not another notice.
    if !buf.is_empty() && st.notify.message() == WEDGE_BANNER {
        st.notify.set_message("", false, now);
    }

    // While the palette overlay is up, the renderer owns the keyboard: forward
    // raw keystrokes to it and send nothing to the session. It reports the
    // selection/cancel over the control channel (handled in the poll loop).
    if let Some(p) = st.palette.as_ref().filter(|p| p.is_open()) {
        p.forward_input(buf);
        return false;
    }

    // Input-latency baseline (perf): the outbox offset before this read's
    // keystrokes, so the bytes queued below can be timestamped and their
    // keystroke→consumed round-trip measured when the server acks them.
    let outbox_start = st.outbox.end_offset();

    // posh#131: collapse a kitty-CSI-u palette key (`\x1b[54;5u` / `:1u`) to raw
    // `0x1e` before the wheel filter and the byte loop, so Ctrl-^ opens the
    // palette under kitty keyboard mode (over roaming the client terminal enters
    // kitty mode via the frame mirror, so the key arrives as CSI-u, not `0x1e`).
    // Raw `0x1e` and every non-palette CSI pass through untouched; the carry
    // reassembles a CSI-u torn across reads. Placed BEFORE the mouse filter
    // because that filter forwards a non-mouse CSI scattered (flushes `\x1b[`,
    // reprocesses the rest — scrollview.rs), which would hide the key from the
    // byte loop. This runs after the open-palette guard above, so CSI-u typed
    // INTO an open palette (navigation, not a summon) is forwarded raw, unaltered.
    let normalized = st.palette_keys.feed(buf);
    let buf: &[u8] = &normalized;

    // When we are intercepting the wheel (bare prompt, primary screen), run
    // input through the mouse filter before the byte loop. The wheel drives the
    // scrollback scroll-view by default, or the legacy wheel→arrow grab when
    // POSH_GRAB_MOUSE=on; other mouse events are dropped. The filter's
    // persistent state reassembles sequences split across reads (posh#52).
    let grabbed;
    let buf: &[u8] = if wheel_active(st) {
        let app_cursor_keys = st.server_term.app_cursor_keys();
        let scroll_mode = st.grab_mouse != GrabMouse::On;
        let out = st.mouse_filter.feed(buf, app_cursor_keys, scroll_mode);
        if out.wheel != 0 {
            scroll_by(st, out.wheel); // local view change; no network send
        }
        grabbed = out.bytes;
        &grabbed
    } else {
        // Not intercepting. If the filter holds a partial from when interception
        // was last active (the app enabled its own mouse mode mid-sequence,
        // flipping it off between reads), hand those bytes back and prepend them
        // so the app — which now wants mouse events — receives the complete
        // sequence, rather than us dropping the prefix and leaking the tail.
        let pending = st.mouse_filter.take_pending();
        if pending.is_empty() {
            buf
        } else {
            let mut joined = pending;
            joined.extend_from_slice(buf);
            grabbed = joined;
            &grabbed
        }
    };

    // Any keystroke while scrolled returns to the live view (FDR 0005), then is
    // forwarded normally below — you are about to type at the prompt.
    if !buf.is_empty() && st.scroll_offset > 0 {
        set_scroll(st, 0);
    }

    // Don't predict for bulk pastes.
    let paste = buf.len() > 100;
    if paste {
        st.predict.reset();
    }

    // Optimistic echo is gated on the remote PTY echoing and the primary screen
    // being active (FDR 0006): tell the predictor whether echo is safe so the
    // optimistic model suppresses (passwords / full-screen apps). Other models
    // ignore this. The compose path re-asserts the same gate before rendering.
    st.predict.set_echo_safe(optimistic_echo_on(st));

    let push = |st: &mut ClientState, byte: u8| {
        if !paste {
            st.predict.set_frame_sent(st.outbox.end_offset());
            st.predict.on_user_byte(byte, &st.last_drawn, now);
        }
        st.outbox.push(&[byte]);
    };

    for &byte in buf {
        if st.quit_pending {
            // Degraded escape prefix: only entered when the palette could not
            // open, so Ctrl-^ . still quits (the rest pass through literally).
            st.quit_pending = false;
            match byte {
                b'.' => {
                    request_shutdown(st);
                    dirty = true;
                    continue;
                }
                ESCAPE_KEY | ESCAPE_PASS_KEY => {
                    // Ctrl-^ twice (or Ctrl-^ ^) sends a literal Ctrl-^.
                    push(st, ESCAPE_KEY);
                }
                other => {
                    // Anything else is sent literally, escape key included.
                    push(st, ESCAPE_KEY);
                    push(st, other);
                }
            }
            if st.notify.message() == PALETTE_FALLBACK_HELP {
                st.notify.set_message("", false, now);
            }
            dirty = true;
            continue;
        }

        if byte == ESCAPE_KEY {
            // Ctrl-^ opens the command palette directly (it is the escape menu:
            // echo, logging, shell-out, suspend, quit all live there). If the
            // renderer can't be spawned, fall back to the emergency-quit prefix.
            if open_palette(st) {
                dirty = true;
            } else {
                st.quit_pending = true;
                st.notify.set_message(PALETTE_FALLBACK_HELP, true, now);
            }
            continue;
        }

        if byte == 0x0c {
            // Ctrl-L: ask for a full repaint of the outer terminal.
            st.initialized = false;
        }

        push(st, byte);
        dirty = true;
    }
    // If this read queued input, timestamp the resulting outbox offset for the
    // input-latency gauge (drained in process_frame when input_ack covers it).
    if st.stats.enabled() {
        let end = st.outbox.end_offset();
        if end > outbox_start {
            st.input_sent.push_back((end, now));
            if st.input_sent.len() > 256 {
                st.input_sent.pop_front(); // bound a stalled link's backlog
            }
        }
    }
    dirty
}

/// Consumes the server's agent-forwarding caps from a frame (FDR 0004): latch
/// AGENT_FORWARD, feed AGENT_DATA chunks through the stream into the local-agent
/// proxy (any FAIL replies the proxy produces — unreachable agent, channel cap
/// — go back onto our outbound stream), and drain our outbox on AGENT_ACK. A
/// decoder error means the authenticated stream is corrupt; drop the proxy.
fn consume_agent_caps(st: &mut ClientState, frame: &ServerFrame) {
    if st.agent.is_none() {
        return;
    }
    if caps::find(&frame.caps, caps::CAP_AGENT_FORWARD).is_some() {
        st.agent_seen = true;
    }
    let mut decode_failed = false;
    // A channel OPEN from the server means a remote process started using the
    // forwarded agent — the event the notice (#96) reports.
    let mut saw_open = false;
    for cap in caps::find_all(&frame.caps, caps::CAP_AGENT_DATA) {
        let Ok((offset, bytes)) = caps::decode_agent_data(&cap.payload) else {
            decode_failed = true;
            break;
        };
        match st.agent_stream.recv(offset, bytes) {
            Ok(records) => {
                saw_open |= records
                    .iter()
                    .any(|r| r.kind == crate::remote::sync::RecordKind::Open);
                // The proxy borrow is scoped tight so the notice below can
                // touch other ClientState fields.
                let replies = st.agent.as_mut().unwrap().apply_records(&records);
                for reply in replies {
                    st.agent_stream.send(&reply);
                }
            }
            Err(_) => {
                decode_failed = true;
                break;
            }
        }
    }
    if let Some(cap) = caps::find(&frame.caps, caps::CAP_AGENT_ACK) {
        if let Ok(upto) = caps::decode_agent_ack(&cap.payload) {
            st.agent_stream.ack(upto);
        }
    }
    // Surface the rate-limited agent-use notice (FDR 0004; #96) on a new
    // channel — but not when the stream just went corrupt (we're tearing down).
    if saw_open && !decode_failed {
        if let Some(notice) = st.agent_notice.as_mut() {
            if let Some(msg) = notice.on_channel_open(now_ms()) {
                st.notify.set_message(&msg, false, now_ms());
            }
        }
    }
    if decode_failed {
        st.agent = None;
        st.agent_seen = false;
    }
}

/// Handles one decoded server frame: acks, prediction bookkeeping, and
/// state application. Returns true when an ack should go out.
fn process_frame(st: &mut ClientState, frame: &ServerFrame) -> bool {
    let now = now_ms();
    // Transport-liveness (#false-disconnect): fold the gap since the previous
    // decoded frame into the max/late-gap gauges before the per-kind counters,
    // so the connection-health view can distinguish a false-disconnect (a long
    // arrival gap tripped the banner on a healthy link) from a genuinely dead
    // peer. Uses the same clock the "Last contact" banner reads (`server_heard`
    // below), so the counted gaps line up with the banner's own decision.
    st.stats.record_frame_arrival(now);
    // Classify every received frame by wire body (includes retransmissions and
    // duplicates — that is what arrived on the link).
    match &frame.body {
        FrameBody::Full(_) => st.stats.record_frame_full(),
        // A Morph body is the incremental analog of a Diff (a delta against the
        // acked base), so it shares the Diff economics counter (#15).
        FrameBody::Diff { .. } | FrameBody::Morph { .. } => st.stats.record_frame_diff(),
        FrameBody::Empty => st.stats.record_frame_empty(),
        // Scrollback bodies carry no visible-screen change, so they stay out of
        // the Full/Diff/Empty economics — but are counted separately (#wedge) so
        // a scrollback storm/reset is not invisible.
        FrameBody::Scrollback { .. } | FrameBody::Scrollback2 { .. } => {
            st.stats.record_frame_scrollback()
        }
    }
    st.notify.server_heard(now);
    st.outbox.ack(frame.input_ack);
    // Input latency (perf): the server has now consumed input up to input_ack,
    // so record the keystroke→consumed round-trip for any pending entries it
    // covers (the deque is offset-ordered, so drain from the front).
    while let Some(&(offset, queued)) = st.input_sent.front() {
        if offset > frame.input_ack {
            break;
        }
        st.input_sent.pop_front();
        st.stats.record_input_ms(now.saturating_sub(queued));
    }
    st.predict
        .on_server_frame(frame.input_ack, frame.echo_ack, st.conn.send_interval());
    // Remote PTY echo state for the optimistic-echo gate (FDR 0006).
    st.echo_on = frame.flags & sync::FLAG_ECHO != 0;
    // Server debug-logging state for the palette's "Server debug logging" label (#3).
    st.server_log_on = frame.flags & sync::FLAG_SERVER_LOG != 0;
    // Organic wedge watchdog (#wedge): the server sets FLAG_WEDGE for the life of
    // an auto-capture episode. Edge-triggered — raise the sticky banner once when
    // it first appears; reset the latch when it clears so a later episode
    // re-raises. The banner itself persists (sticky) until a keystroke dismisses
    // it, so a fast self-recovery doesn't rob the user of the notice.
    let wedge_now = frame.flags & sync::FLAG_WEDGE != 0;
    if wedge_now && !st.wedge_seen {
        st.wedge_seen = true;
        st.notify.set_message(WEDGE_BANNER, true, now);
    } else if !wedge_now && st.wedge_seen {
        st.wedge_seen = false;
    }
    // Server transport-state piggyback (#6): record the latest report when present
    // (only after we advertised CAP_DIAG). Keep the prior value on a malformed or
    // absent payload — the dump shows the most recent good report.
    if let Some(cap) = caps::find(&frame.caps, caps::CAP_DIAG) {
        if let Ok(d) = caps::decode_server_diag(&cap.payload) {
            st.last_server_diag = Some(d);
        }
    }
    // Scrollback stream v2 (RFC 0009 §1): adopt the server's epoch from its
    // SCROLLBACK2 ack. A change (or a first ack, or the ack after our local
    // resize cleared `sb2_epoch`) opens a fresh epoch: clear the ring and zero
    // the cumulative count — the server counts row 0 from the epoch start.
    if let Some(cap) = caps::find(&frame.caps, caps::CAP_SCROLLBACK2) {
        if let Ok(epoch) = caps::decode_scrollback2_ack(&cap.payload) {
            if st.sb2_epoch != Some(epoch) {
                st.scrollback.clear();
                st.scroll_offset = 0;
                st.sb2_rows = 0;
                st.sb2_epoch = Some(epoch);
            }
        }
    }
    // The escape-to-shell overlay is up (FDR 0008): the request was honored, so
    // drop the "opening shell…" notice (the request flag is already one-shot).
    if frame.flags & sync::FLAG_OVERLAY != 0 && st.notify.message() == "opening shell\u{2026}" {
        st.notify.set_message("", false, now);
    }
    if frame.flags & sync::FLAG_SHUTDOWN != 0 {
        st.shutdown_seen = true;
        // EXIT_STATUS rides the shutdown frame's capability table; the
        // server only sends it because we advertised it (RFC 0001 §3).
        if let Some(cap) = caps::find(&frame.caps, caps::CAP_EXIT_STATUS) {
            if let Some(&code) = cap.payload.first() {
                st.exit_status = code as i32;
            }
        }
    }
    // Evolved-predictor remote metrics (RFC 0007 §3): the server attaches
    // CAP_METRICS only because we advertised it (a GP species is active).
    if let Some(cap) = caps::find(&frame.caps, caps::CAP_METRICS) {
        if let Some(fields) = caps::decode_metrics(&cap.payload) {
            st.remote_metrics = fields;
        }
    }
    consume_agent_caps(st, frame);
    apply_frame(st, frame)
}

/// Applies a frame to the local terminal model. Frames reconstruct complete
/// screen state, so application is: fresh Terminal, then feed the dump_vt
/// stream. Returns true when the frame advanced (or repeated) server state
/// and an ack should go out.
/// Clear the apply-stall forensic latch once the client makes forward progress,
/// so a later wedge episode captures a fresh bundle and a manual/SIGUSR2 capture
/// after recovery does not write a stale one.
fn clear_reack(st: &mut ClientState) {
    st.last_reack = None;
    st.forensic_captured = false;
}

fn apply_frame(st: &mut ClientState, frame: &ServerFrame) -> bool {
    // Apply-path instrumentation (#wedge): record the visible/scrollback frame
    // at the gate so a frozen client self-reports which frame it is rejecting.
    // Empty heartbeats carry no visible body and are skipped to keep the signal
    // clean (and to keep `last_rx` pointing at the last real frame).
    match &frame.body {
        FrameBody::Empty => {}
        FrameBody::Full(_) => {
            st.stats
                .record_apply_rx(frame.frame_num, frame.frame_num, FrameKind::Full)
        }
        FrameBody::Diff { base, .. } => {
            st.stats.record_apply_rx(frame.frame_num, *base, FrameKind::Diff)
        }
        FrameBody::Morph { base, .. } => {
            st.stats.record_apply_rx(frame.frame_num, *base, FrameKind::Morph)
        }
        FrameBody::Scrollback { base, .. } => {
            st.stats
                .record_apply_rx(frame.frame_num, *base, FrameKind::Scrollback)
        }
        FrameBody::Scrollback2 { row_offset, .. } => {
            st.stats
                .record_apply_rx(frame.frame_num, *row_offset, FrameKind::Scrollback)
        }
    }
    // Scrollback stream v2 (RFC 0009 §3): handled BEFORE the stale gate — its
    // carrying frame_num is a diagnostic annotation, never state, and this
    // body MUST NOT touch applied_num (the whole point of the separation: no
    // scrollback traffic can launder or stale the visible baseline).
    if let FrameBody::Scrollback2 {
        epoch,
        row_offset,
        rows,
    } = &frame.body
    {
        // Wrong or unknown epoch (pre-first-ack, mid-resize): a stale in-flight
        // body from a superseded row space — discard, never append.
        if st.sb2_epoch != Some(*epoch) {
            st.stats.record_apply_stale();
            return true;
        }
        let end = *row_offset + rows.len() as u64;
        if end <= st.sb2_rows {
            st.stats.record_apply_dup();
            return true; // fully covered retransmission
        }
        if *row_offset < st.sb2_rows {
            // Partial overlap: a conforming server anchors at our ack, so this
            // is transient reordering — discard and let the re-anchor arrive.
            st.stats.record_apply_basemis();
            return true;
        }
        // In-order append, or a forward jump (server-ring eviction): the gap is
        // permanently lost and the partial view is first-class (FDR 0005).
        let grew = rows.len();
        st.scrollback.append(rows);
        if st.scroll_offset > 0 {
            set_scroll(st, st.scroll_offset + grew);
        }
        st.sb2_rows = end;
        st.stats.record_apply_advanced();
        return true;
    }
    if frame.frame_num < st.applied_num {
        st.stats.record_apply_stale();
        return true; // stale retransmission: re-ack our newer state
    }
    // Scrollback growth (RFC 0002 §3): append rows to the local ring without
    // disturbing the visible model. `base` is the frame the growth was
    // measured from; we apply only when we are exactly at it, so a
    // retransmitted or superseding body never double-appends. The visible
    // `applied_data` is unchanged by a scrollback frame and stays valid as
    // the base for a later `Diff` that builds on this frame number.
    if let FrameBody::Scrollback { base, rows } = &frame.body {
        if frame.frame_num == st.applied_num {
            st.stats.record_apply_dup();
            return true; // duplicate retransmission: re-ack, don't reapply
        }
        if *base != st.applied_num {
            st.stats.record_apply_basemis();
            return true; // growth against a state we are not at; re-ack
        }
        let grew = rows.len();
        st.scrollback.append(rows);
        if st.scroll_offset > 0 {
            // Keep the frozen viewport anchored on the same content as new rows
            // arrive (FDR 0005: output accumulates but does not yank to bottom).
            set_scroll(st, st.scroll_offset + grew);
        }
        st.applied_num = frame.frame_num;
        st.stats.record_apply_advanced();
        clear_reack(st);
        return true;
    }
    // Base-anchored bodies (Diff, Morph) apply only when we are exactly at
    // their base; otherwise re-ack our (stale) state and let the server fall
    // back to a Full keyframe once it sees the ack. Same base-mismatch rule
    // for both, so a lost frame is handled identically whichever codec is in
    // use (#15).
    match &frame.body {
        FrameBody::Empty => return false,
        FrameBody::Diff { base, base_sum, .. } | FrameBody::Morph { base, base_sum, .. } => {
            if *base != st.applied_num {
                st.stats.record_apply_basemis();
                // #95 recovery: a base BEHIND our applied_num means a scrollback
                // frame leapt us past this (unapplied) visible frame — our visible
                // baseline is stale, and the plain re-ack reports an applied_num we
                // do not truly hold, so the server never falls back to a Full. That
                // desync self-heals via neither reack nor base_sum, so actively
                // request a resync. (base > applied_num is the benign we-are-behind
                // case a retransmit resolves, so it keeps the passive re-ack.)
                if *base < st.applied_num {
                    st.flags |= sync::CLIENT_FLAG_RESYNC;
                }
                return true;
            }
            // RFC 0006: the base NUMBER matches; when the server stamped a base
            // checksum (CAP_BASE_SUM, Diff only) verify the base CONTENT too. A
            // mismatch means our applied_data diverged from the server's diff
            // base -- applying would short-base wedge or silently corrupt (#94) --
            // so re-ack and request a Full keyframe instead. (Morph base_sum is
            // always None, so this is inert there.)
            if let Some(sum) = base_sum {
                if sync::base_checksum(&st.applied_data) != *sum {
                    st.stats.record_apply_base_sum_mismatch();
                    st.flags |= sync::CLIENT_FLAG_RESYNC;
                    return true;
                }
            }
        }
        FrameBody::Full(_) => {}
        // Handled above (returns early); listed so the match stays total.
        FrameBody::Scrollback { .. } | FrameBody::Scrollback2 { .. } => {
            unreachable!("scrollback handled above")
        }
    }
    if frame.frame_num == st.applied_num {
        st.stats.record_apply_dup();
        return true; // duplicate retransmission: re-ack, don't reapply
    }
    // Route the body through the selected codec's applier. Time the apply —
    // the client-side mirror of the server's dump_vt_us. For DumpDiff this is
    // the full-dump reparse (the suspected hot spot); for MorphDelta it is the
    // forward `process(escapes)` on the existing model (the optimization).
    let apply_timer = st.stats.instrument().then(Instant::now);
    let outcome = st.applier.apply(
        st.rows,
        st.cols,
        &st.applied_data,
        &mut st.server_term,
        &frame.body,
    );
    if let Some(t) = apply_timer {
        st.stats.record_apply_us(t.elapsed().as_micros() as u64);
    }
    match outcome {
        ApplyOutcome::Advanced { dump } => {
            st.applied_num = frame.frame_num;
            st.applied_data = dump;
            st.stats.record_apply_advanced();
            // The manual "Reset & resync" (CLIENT_FLAG_RESYNC) forces a Full
            // keyframe; once that Full applies and advances us, the recovery is
            // complete, so drop the sticky "resyncing…" notice (#93). Mirrors
            // the one-shot "opening shell…" drop above. Only a Full resolves a
            // resync — a Diff/Morph advancing does not.
            if matches!(frame.body, FrameBody::Full(_))
                && st.notify.message() == "resyncing\u{2026}"
            {
                st.notify.set_message("", false, now_ms());
            }
            clear_reack(st);
            true
        }
        // MorphDelta advanced the model in place without re-dumping it (#15).
        // applied_data stays at the last Full keyframe's dump; a Morph session
        // never sends a Diff body that would read it.
        ApplyOutcome::AdvancedNoDump => {
            st.applied_num = frame.frame_num;
            st.stats.record_apply_advanced();
            clear_reack(st);
            true
        }
        // Undecodable diff against a matching-base frame: re-ack and wait for a
        // Full keyframe, model untouched. This is the #90 apply-stall; stash the
        // rejected body and capture a forensic bundle once per episode so the
        // divergent base can be analysed offline (the divergence origin is not
        // unit-reproducible -- it needs the live bytes).
        ApplyOutcome::ReackAndWait => {
            st.stats.record_apply_reack();
            let captured = match &frame.body {
                FrameBody::Diff { base, diff, .. } => Some((*base, FrameKind::Diff, diff.clone())),
                FrameBody::Morph { base, escapes, .. } => {
                    Some((*base, FrameKind::Morph, escapes.clone()))
                }
                _ => None,
            };
            if let Some((base, kind, bytes)) = captured {
                st.last_reack = Some((frame.frame_num, base, kind, bytes));
                if !st.forensic_captured {
                    st.forensic_captured = true;
                    if let Some(reack) = st.last_reack.as_ref() {
                        let _ = diag::capture_forensics(st.applied_num, &st.applied_data, reack);
                    }
                }
            }
            // Auto-escalate (#90): an undecodable diff against a matching-base
            // frame never self-heals -- the server keeps rebuilding the same
            // diff against a base the client cannot reconstruct. Actively
            // request a Full keyframe instead of passively waiting for one that
            // never comes. One-shot flag (cleared on send); it coalesces across
            // the retransmit storm and rides the next ack, so the manual "Reset
            // & resync" command is no longer the only way out of an apply-stall.
            st.flags |= sync::CLIENT_FLAG_RESYNC;
            true
        }
        // Empty is returned early above; an applier should not produce this
        // for a visible body, but if it does, treat it as no advance.
        ApplyOutcome::NoChange => {
            st.stats.record_apply_nochange();
            false
        }
    }
}

/// The paint destination `render_to` writes through. `write_budget` mirrors
/// [`util::write_all_retry`]'s contract: return the number of bytes actually
/// written, where a count below `bytes.len()` means the rest was DROPPED. The
/// abstraction exists so the render path — which models the tty as a
/// differential surface and must react to a dropped paint — can be exercised by
/// tests with a lossy in-memory sink instead of the real fd (#127).
trait TtySink {
    fn write_budget(&mut self, bytes: &[u8]) -> std::io::Result<usize>;
}

/// The production sink: the real terminal on `STDOUT`, with `write_all_retry`'s
/// 1000ms drain budget. Holds a raw fd (STDOUT is an `i32` const in this module).
struct FdSink(i32);

impl TtySink for FdSink {
    fn write_budget(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        util::write_all_retry(self.0, bytes, 1000)
    }
}

/// Write a critical one-shot terminal-control sequence (smcup on connect, rmcup
/// on exit) to STDOUT. These are not part of the differential frame stream, so a
/// dropped write can't be repainted — a lost smcup lands the first frame on the
/// primary screen, a lost rmcup strands the alt screen on exit. There is no
/// recovery for a one-shot teardown, but a short write must not vanish silently:
/// log it (a no-op unless POSH_DEBUG_LOG is armed) so it is diagnosable (#127).
fn write_display_control(what: &str, bytes: &[u8]) {
    match util::write_all_retry(STDOUT, bytes, 1000) {
        Ok(n) if n < bytes.len() => util::log_write(
            "render",
            &format!("dropped {} bytes of {what} (tty write budget spent)", bytes.len() - n),
        ),
        Ok(_) => {}
        Err(e) => util::log_write("render", &format!("{what} write failed: {e}")),
    }
}

/// mosh's output_new_frame: server state + prediction overlay + status
/// banner, diffed against what the tty currently shows. Thin wrapper over
/// [`render_to`] that paints to the real `STDOUT`.
fn render(st: &mut ClientState, now: u64) {
    render_to(st, now, &mut FdSink(STDOUT));
}

/// The render body, parameterized over the paint destination so it is testable
/// without the real fd. Composes the frame, writes it through `sink`, and — this
/// is the load-bearing part — reacts to a dropped write.
///
/// compose_frame already committed `last_drawn = next` (it models the tty as a
/// differential surface). If the sink can't drain the tty within its budget it
/// DROPS the un-written bytes — so on a short/failed write the physical screen
/// no longer matches last_drawn, and because we only ever emit diffs nothing
/// would ever repaint those cells again (the permanent top-line desync: a
/// dropped banner-clear leaves stale banner text under later output). Force a
/// full repaint next tick to resync, and don't advance last_painted_gen — this
/// generation did NOT (fully) reach the tty.
fn render_to<S: TtySink>(st: &mut ClientState, now: u64, sink: &mut S) {
    let bytes = if st.scroll_offset > 0 {
        compose_scroll_frame(st)
    } else {
        compose_frame(st, now)
    };
    if bytes.is_empty() {
        st.stats.record_render_skip();
    } else {
        st.stats.record_render(bytes.len());
        match sink.write_budget(&bytes) {
            Ok(n) if n == bytes.len() => {
                // #wedge (#83): the model generation now actually on the tty.
                st.last_painted_gen = st.server_term.generation();
            }
            // A short write (n < len) dropped the rest, or a real I/O error hit
            // mid-paint: either way the tty diverged from last_drawn. Resync.
            _ => st.initialized = false,
        }
    }
}

/// Builds this tick's escape stream (empty when the screen already
/// matches). Idle ticks skip the full-grid snapshot: with the model
/// unadvanced, the screen initialized, and no overlay live now or at the
/// previous compose, the diff is provably empty. Overlays are
/// time-driven, so "live" includes the lateness banner being DUE
/// (server_late), not just shown — predictions only change while active,
/// and a just-cleared overlay still gets one closing compose via
/// last_render_overlays. github #35.
fn compose_frame(st: &mut ClientState, now: u64) -> Vec<u8> {
    let model_state = (st.applied_num, st.server_term.generation());
    let palette_open = st.palette.as_ref().is_some_and(Palette::is_open);
    let overlays_live = st.predict.active()
        || !st.notify.message().is_empty()
        || st.notify.server_late(now)
        || palette_open;
    if st.initialized
        && model_state == st.last_render_state
        && !overlays_live
        && !st.last_render_overlays
    {
        // #wedge fast-path skip (#83): we're skipping because the model looks
        // unchanged since the last compose — but if that generation was never
        // actually painted (`last_painted_gen` lags), content sits in the model
        // unpainted and every idle tick re-skips it: the render-skip freeze.
        // Edge-triggered per generation so a long freeze logs once, not a storm.
        let gen = st.server_term.generation();
        if util::log_active() && gen != st.last_painted_gen && gen != st.last_skip_log_gen {
            st.last_skip_log_gen = gen;
            util::log_write(
                "render",
                &format!(
                    "fastpath skip while unpainted: frame={} gen={} painted={} alt={}",
                    st.applied_num,
                    gen,
                    st.last_painted_gen,
                    st.server_term.is_alt_screen() as u8,
                ),
            );
        }
        return Vec::new();
    }
    let prev_render_state = st.last_render_state;
    st.last_render_state = model_state;
    st.last_render_overlays = overlays_live;

    // Time the actual render compute (snapshot + prediction/banner overlay +
    // diff), excluding the idle fast-path above so the average reflects real
    // work. enabled() is read and dropped before the borrows below.
    let compose_timer = st.stats.instrument().then(Instant::now);
    // Optimistic echo gate (FDR 0006): when echo is unsafe (password prompt /
    // full-screen app) the optimistic model drops its pending overlay so the
    // authoritative paint stands; other models ignore this.
    st.predict.set_echo_safe(optimistic_echo_on(st));
    let base = Snapshot::from_term(&st.server_term);
    st.predict.cull(&base, now);
    // Metric bus (RFC 0007 §3): while a GP species is active, assemble the
    // client-local terminals from the authoritative screen + transport +
    // predictor feedback. Cheap field copies; skipped for the non-GP models so
    // they pay nothing. Server-forwarded/host terminals stay NaN until wired.
    if is_gp_species(st.predict_model) {
        let mut metrics = predict::gather_client_local(&base);
        metrics.fill_transport(
            st.conn.srtt(),
            st.conn.rto() as f64,
            st.conn.send_interval() as f64,
            st.outbox.pending().len() as f64,
        );
        // Render headroom from the most recent event-loop iteration + frame
        // compute costs (RFC 0007 §2). dump_vt_us is a server-side cost, so it
        // stays NaN client-side (folded into the server-forwarded work).
        let busy = st.stats.last_loop_busy_us();
        let idle = st.stats.last_loop_idle_us();
        let iter_us = busy + idle;
        let (fps, busy_frac) = if iter_us > 0 {
            (1_000_000.0 / iter_us as f64, busy as f64 / iter_us as f64)
        } else {
            (f64::NAN, f64::NAN)
        };
        metrics.fill_render_headroom(
            fps,
            busy_frac,
            st.stats.last_apply_us() as f64,
            st.stats.last_compose_us() as f64,
        );
        metrics.fill_predictor_feedback(&st.predict.stats());
        // Session-gate terminals: already client-side, no forwarding (the
        // alt-screen "indicator" of #97 — the client reconstructs it).
        metrics.fill_session_gate(st.server_term.is_alt_screen(), st.echo_on);
        // Remote terminals decoded from the server's CAP_METRICS (RFC 0007 §3).
        metrics.fill_remote(st.remote_metrics);
        st.last_metrics = metrics;
        // Feed the assembled vector to the predictor (the evolved controller
        // consumes it; other models ignore it) before the next keystroke, so the
        // champion's policy reads the current metrics.
        st.predict.set_metrics(&st.last_metrics);
    }
    let mut next = base;
    st.predict.render(&mut next, &*st.renderer);
    // The palette overlay sits above the session (greyed) but below the banner.
    if palette_open {
        if let Some(rterm) = st.palette.as_ref().and_then(Palette::screen) {
            composite_palette(&mut next, rterm, st.rows, st.cols);
        }
    }
    st.notify.adjust(now);
    st.notify.apply(&mut next, now);

    let wheel = wheel_active(st);
    let bytes = display::new_frame_opt(
        st.initialized,
        &st.last_drawn,
        &next,
        wheel,
        st.last_wheel,
        st.scroll_opt,
    );
    st.initialized = true;
    st.last_wheel = wheel;
    // #wedge (#83): on an empty paint where the generation advanced (the
    // render-skip suspect), check whether the composited grid ACTUALLY differed
    // from what we last drew — `grids_differ=1` is the smoking gun (content-blind
    // new_frame_opt), `=0` is a benign no-op gen bump (cursor move, etc.).
    // Computed BEFORE last_drawn is overwritten, and only in this rare case so
    // the full-grid compare stays off the hot path even with default-on logging.
    let gen_advanced = model_state.1 != prev_render_state.1;
    let grids_differ = util::log_active() && bytes.is_empty() && gen_advanced && next != st.last_drawn;
    st.last_drawn = next;
    if util::log_active() && bytes.is_empty() && gen_advanced {
        util::log_write(
            "render",
            &format!(
                "empty paint: frame={} gen {}->{} grids_differ={} alt={}",
                st.applied_num,
                prev_render_state.1,
                model_state.1,
                grids_differ as u8,
                st.server_term.is_alt_screen() as u8,
            ),
        );
    }
    if let Some(t) = compose_timer {
        st.stats.record_compose_us(t.elapsed().as_micros() as u64);
    }
    bytes
}

/// Builds the scroll-view escape stream (FDR 0005) via the shared
/// [`scrollview::compose_scroll_frame`], threading this client's fields: the
/// scroll offset + memo, the accumulated ring, the server model, the tty size,
/// and the render bookkeeping (`initialized`/`last_drawn`) it advances like any
/// live frame. `scroll_opt` carries the palette scroll-shortcut toggle.
fn compose_scroll_frame(st: &mut ClientState) -> Vec<u8> {
    scrollview::compose_scroll_frame(
        st.scroll_offset,
        &st.scrollback,
        &st.server_term,
        st.rows,
        st.cols,
        &mut st.last_scroll_state,
        &mut st.initialized,
        &mut st.last_drawn,
        st.scroll_opt,
    )
}

/// Snapshots the prediction engine's display gauges for the stats log.
fn predict_sample(stats: &predict::PredictorStats) -> PredictSample {
    let (correct, nocredit, incorrect) = stats.outcomes;
    let (nocredit_unknown, nocredit_blank, nocredit_matched) = stats.nocredit_reasons;
    PredictSample {
        active: stats.active,
        shown: stats.shown_cells,
        epoch_lag: stats.epoch_lag,
        resets: stats.mispredict_resets,
        correct,
        nocredit,
        incorrect,
        nocredit_unknown,
        nocredit_blank,
        nocredit_matched,
    }
}

/// The capability table this client advertises in every message (the
/// protocol is connectionless): protocol version, "I understand exit-status
/// frames", and — unless this is the post-resize message that must cease
/// scrollback (RFC 0002 §4) — "I keep a scrollback ring and understand
/// BODY_SCROLLBACK" with payload 0 requesting the server's default ring
/// depth (RFC 0002 §1). Consumes the one-shot resize suppression.
fn outgoing_caps(st: &mut ClientState) -> Vec<caps::Cap> {
    let mut extra = vec![
        caps::Cap {
            id: caps::CAP_EXIT_STATUS,
            payload: vec![],
        },
        // Base-integrity (RFC 0006): always advertised. It's a pure safety check
        // -- verify the diff base before applying -- at a negligible cost (4
        // bytes per Diff), so there is no opt-in gate like CAP_MORPH.
        caps::Cap {
            id: caps::CAP_BASE_SUM,
            payload: vec![],
        },
    ];
    if st.suppress_scrollback_once {
        st.suppress_scrollback_once = false;
    } else {
        // Scrollback stream v2 (RFC 0009 §1): always advertise SCROLLBACK2 —
        // it doubles as the per-message ack (epoch + cumulative rows). Keep
        // the v1 SCROLLBACK entry only until the server acknowledges v2, so a
        // v1-only server still provides RFC 0002 scrollback.
        extra.push(caps::encode_scrollback2_client(&caps::Scrollback2Client {
            ring_depth: 0,
            epoch: st.sb2_epoch.unwrap_or(0),
            acked_rows: st.sb2_rows,
        }));
        if st.sb2_epoch.is_none() {
            extra.push(caps::Cap {
                id: caps::CAP_SCROLLBACK,
                payload: vec![0],
            });
        }
    }
    // Incremental frame sync (#15): advertise CAP_MORPH only behind the
    // POSH_FRAMESYNC=morph opt-in. A default session never sends it, so the
    // server selects DumpDiff and the byte stream is unchanged.
    if st.framesync.advertises_morph() {
        extra.push(caps::Cap {
            id: caps::CAP_MORPH,
            payload: vec![],
        });
    }
    // Server transport-state piggyback (#6) + agent-endpoint diag (FDR 0004):
    // ask the server to attach its live state in a debug posture OR
    // when agent forwarding is active (so the agent-forwarding palette can show
    // both ends). A default session — neither — leaves this unsent.
    if st.want_server_diag || st.agent.is_some() {
        extra.push(caps::Cap {
            id: caps::CAP_DIAG,
            payload: vec![],
        });
    }
    // Evolved predictor (RFC 0007 §3): request the server's remote-host metric
    // terminals only when a GP species is active, so a default session never
    // negotiates CAP_METRICS and pays no per-frame overhead.
    if is_gp_species(st.predict_model) {
        extra.push(caps::Cap {
            id: caps::CAP_METRICS,
            payload: vec![],
        });
    }
    // Agent forwarding (FDR 0004): advertise AGENT_FORWARD whenever the proxy
    // is active so the server may begin opening channels; emit AGENT_DATA
    // chunks + AGENT_ACK only once the server has advertised back (RFC 0001:
    // not before seeing the peer's AGENT_FORWARD).
    if st.agent.is_some() {
        extra.push(caps::Cap {
            id: caps::CAP_AGENT_FORWARD,
            payload: vec![],
        });
        if st.agent_seen {
            extra.extend(caps::encode_agent_data(
                st.agent_stream.send_base(),
                st.agent_stream.pending(),
            ));
            extra.push(caps::encode_agent_ack(st.agent_stream.recv_ack()));
        }
    }
    caps::own_table(&extra)
}
fn send_message(st: &mut ClientState) {
    let msg = ClientMessage {
        flags: st.flags,
        caps: outgoing_caps(st),
        acked_frame: st.applied_num,
        rows: st.rows,
        cols: st.cols,
        input_base: st.outbox.base(),
        input: st.outbox.pending().to_vec(),
    };
    // CLIENT_FLAG_ESCAPE, the server-logging toggles, and the resync request are
    // one-shot: they rode this message, so clear them now (the server acts
    // idempotently on repeats, and the user can retry if this datagram is lost).
    // SHUTDOWN stays sticky.
    st.flags &= !(sync::CLIENT_FLAG_ESCAPE
        | sync::CLIENT_FLAG_LOG_ON
        | sync::CLIENT_FLAG_LOG_OFF
        | sync::CLIENT_FLAG_RESYNC);
    for frag in st
        .fragmenter
        .make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX)
    {
        let _ = st.conn.send(&frag.to_bytes());
    }
    st.last_send = now_ms();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_families_on_loopback() {
        // Numeric literals resolve to themselves; family filters apply.
        let v4 = resolve("127.0.0.1", 1234, Family::Auto).unwrap();
        assert!(v4.is_ipv4());
        let v4 = resolve("127.0.0.1", 1234, Family::Inet).unwrap();
        assert!(v4.is_ipv4());
        let v6 = resolve("::1", 1234, Family::Inet6).unwrap();
        assert!(v6.is_ipv6());
        assert_eq!(v6.port(), 1234);
        // Family mismatch is an error rather than a silent fallback.
        assert!(resolve("127.0.0.1", 1234, Family::Inet6).is_err());
        assert!(resolve("::1", 1234, Family::Inet).is_err());
    }

    #[test]
    fn grab_mouse_parse() {
        use GrabMouse::*;
        assert_eq!(GrabMouse::parse(None).unwrap(), Off);
        assert_eq!(GrabMouse::parse(Some("")).unwrap(), Off);
        assert_eq!(GrabMouse::parse(Some("off")).unwrap(), Off);
        assert_eq!(GrabMouse::parse(Some("never")).unwrap(), Off);
        assert_eq!(GrabMouse::parse(Some("on")).unwrap(), On);
        assert_eq!(GrabMouse::parse(Some("always")).unwrap(), On);
        assert_eq!(GrabMouse::parse(Some("1")).unwrap(), On);
        assert!(GrabMouse::parse(Some("sometimes")).is_err());
    }

    // The `MouseFilter` unit tests (wheel→arrows, lossless round-trip, split
    // reassembly, grab-flip hand-back, bounded partial, scroll-mode ticks) moved
    // with the filter into `remote::scrollview`.

    #[test]
    fn wheel_active_requires_primary_screen_without_app_mouse_mode() {
        let mut st = test_state(5, 20);
        // Bare prompt, primary screen, no app mouse mode → wheel intercepted.
        assert!(wheel_active(&st));
        // App enables mouse tracking → posh steps back, passes events through.
        st.server_term.process(b"\x1b[?1000h");
        assert!(!wheel_active(&st));
        // No app mouse mode again, but on the alt screen → no scrollback there.
        st.server_term.process(b"\x1b[?1000l\x1b[?1049h");
        assert!(!wheel_active(&st));
    }

    #[test]
    fn echo_safe_gate_requires_echo_on_and_primary_screen() {
        // FDR 0006: echo-safety is computed from the remote PTY echoing AND the
        // primary screen being active. Echo-off (password) or alt-screen
        // (full-screen app) suppress. The model decides what to do with the
        // flag via set_echo_safe — the optimistic model drops its overlay, mosh
        // ignores it — so this gate is now model-independent.
        let mut st = test_state(24, 80);
        st.predict = predict::build(PredictionModel::Optimistic, RenderStyle::Replace, false).0;

        assert!(!optimistic_echo_on(&st), "default echo_on=false => off");

        st.echo_on = true;
        assert!(optimistic_echo_on(&st), "echo on, primary screen => on");

        st.server_term.process(b"\x1b[?1049h"); // enter alt screen
        assert!(st.server_term.is_alt_screen());
        assert!(!optimistic_echo_on(&st), "alt-screen suppresses");
        st.server_term.process(b"\x1b[?1049l");

        st.echo_on = false;
        assert!(!optimistic_echo_on(&st), "echo off (password) suppresses");
    }

    #[test]
    fn resolve_ipv6_literal_with_brackets_in_port_form() {
        let addr = resolve("::1", 60001, Family::Auto).unwrap();
        match addr {
            SocketAddr::V6(a) => assert_eq!(a.ip().to_string(), "::1"),
            SocketAddr::V4(_) => panic!("expected v6"),
        }
    }

    /// A real RawMode over a throwaway pty slave. `dispatch_palette_action`
    /// needs a `&RawMode` (the suspend command restores/re-enters raw mode).
    fn pty_raw_mode() -> RawMode {
        // SAFETY: openpty fills m/s with valid fds; the slave is a tty, which is
        // all RawMode::enable needs. The fds intentionally leak for the test.
        unsafe {
            let (mut m, mut s) = (0, 0);
            assert_eq!(
                libc::openpty(
                    &mut m,
                    &mut s,
                    std::ptr::null_mut(),
                    std::ptr::null::<libc::termios>() as *mut _,
                    std::ptr::null_mut(),
                ),
                0,
                "openpty"
            );
            let _ = m; // keep the pty pair alive for the test
            RawMode::enable(s).unwrap()
        }
    }

    #[test]
    fn dispatch_shell_open_sets_escape_flag() {
        // The palette's "Shell out" command sets CLIENT_FLAG_ESCAPE (FDR 0008)
        // and asks to send promptly.
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "shell.open", &json!({}), 0);
        assert!(send, "shell-out asks to send promptly");
        assert_ne!(st.flags & sync::CLIENT_FLAG_ESCAPE, 0, "escape flag set");
    }

    #[test]
    fn dispatch_quit_requests_shutdown() {
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "app.quit", &json!({}), 0);
        assert!(send, "quit asks to send promptly");
        assert!(st.shutdown_requested, "quit requests shutdown");
    }

    #[test]
    fn dispatch_resync_sets_one_shot_resync_flag() {
        // The palette's "Reset & resync" command sets CLIENT_FLAG_RESYNC and asks
        // to send promptly so the server forces a Full keyframe (#wedge).
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "session.resync", &json!({}), 0);
        assert!(send, "resync asks to send promptly");
        assert_ne!(st.flags & sync::CLIENT_FLAG_RESYNC, 0, "resync flag set");
    }

    #[test]
    fn resync_banner_clears_once_full_applies() {
        // #93: "Reset & resync" sets a sticky "resyncing…" banner; the forced
        // Full keyframe applying must drop it, otherwise it persists forever.
        let raw = pty_raw_mode();
        let mut st = test_state(3, 20);
        let _ = dispatch_palette_action(&mut st, &raw, "session.resync", &json!({}), 0);
        assert_eq!(
            st.notify.message(),
            "resyncing\u{2026}",
            "resync sets the sticky banner"
        );
        let full = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"recovered".to_vec()),
        };
        assert!(apply_frame(&mut st, &full));
        assert_eq!(
            st.notify.message(),
            "",
            "the applied Full clears the resync banner"
        );
    }

    #[test]
    fn resync_banner_survives_an_unrelated_diff() {
        // A Diff advancing the model is not the resync result, so the banner
        // must stay up until the forced Full lands (#93).
        let raw = pty_raw_mode();
        let mut st = test_state(3, 20);
        let base = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"a".to_vec()),
        };
        assert!(apply_frame(&mut st, &base));
        let _ = dispatch_palette_action(&mut st, &raw, "session.resync", &json!({}), 0);
        let diff = ServerFrame {
            frame_num: 2,
            body: FrameBody::Diff {
                base: 1,
                base_sum: None,
                diff: sync::make_diff(b"a", b"ab"),
            },
            ..base
        };
        assert!(apply_frame(&mut st, &diff));
        assert_eq!(
            st.notify.message(),
            "resyncing\u{2026}",
            "a Diff must not clear the resync banner"
        );
    }

    #[test]
    fn dispatch_forensics_without_wedge_reports_nothing_pending() {
        // The "Dump wedge forensics" command is local (no wire send) and, with
        // no apply-stall pending, says so rather than writing a stale bundle.
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "session.forensics", &json!({}), 0);
        assert!(!send, "forensics capture is local, no wire send");
        assert!(
            st.notify.message().contains("nothing pending"),
            "notify: {}",
            st.notify.message(),
        );
    }

    #[test]
    fn wedge_flag_raises_sticky_banner_once_and_resets() {
        // #wedge: the server's FLAG_WEDGE edge raises the sticky banner exactly
        // once per episode; a keystroke dismisses it while the stall continues,
        // and only after the flag clears does a fresh episode re-raise it.
        let mut st = test_state(24, 80);
        let empty = |flags: u8, num: u64| ServerFrame {
            flags,
            caps: vec![],
            frame_num: num,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Empty,
        };
        // Flag rises -> sticky banner, latch set.
        process_frame(&mut st, &empty(sync::FLAG_WEDGE, 1));
        assert!(st.wedge_seen, "latch set on first FLAG_WEDGE");
        assert_eq!(st.notify.message(), WEDGE_BANNER);
        // A keystroke dismisses the banner; the latch stays (still stalled).
        let _ = process_user_input(&mut st, b"x");
        assert_eq!(st.notify.message(), "", "keystroke dismisses the banner");
        assert!(st.wedge_seen, "latch persists across a dismissal mid-episode");
        // Same flag still set on later frames: no re-raise mid-episode.
        process_frame(&mut st, &empty(sync::FLAG_WEDGE, 2));
        assert_eq!(st.notify.message(), "", "no re-raise while the flag stays set");
        // Flag clears -> latch resets.
        process_frame(&mut st, &empty(0, 3));
        assert!(!st.wedge_seen, "latch reset when FLAG_WEDGE clears");
        // A new episode re-raises the banner.
        process_frame(&mut st, &empty(sync::FLAG_WEDGE, 4));
        assert_eq!(st.notify.message(), WEDGE_BANNER, "a new episode re-raises");
    }

    #[test]
    fn wedge_debug_summary_reports_apply_stall_and_server_state() {
        // The on-screen line (#3): with an apply-stall pending and a CAP_DIAG
        // report in hand, it shows reack=yes and the far-side srv state, so the
        // user reads the wedge's cause without a second terminal.
        let mut st = test_state(24, 80);
        st.applied_num = 41;
        st.last_reack = Some((42, 41, FrameKind::Diff, vec![0u8; 8]));
        st.last_server_diag = Some(caps::ServerDiag {
            current_num: 43,
            acked_num: 41,
            term_gen: 90,
            outstanding: 2,
            pty_open: true,
            pid: 1234,
            agent: None,
        });
        let line = wedge_debug_summary(&st, 1000);
        assert!(line.contains("applied=41"), "{line}");
        assert!(line.contains("reack=yes"), "{line}");
        assert!(line.contains("srv=(pid=1234 num=43 acked=41 out=2 pty=1)"), "{line}");
    }

    #[test]
    fn wedge_debug_summary_server_state_off_without_diag_posture() {
        // No CAP_DIAG negotiated (default session, not a debug posture): the
        // far side is unavailable, reported as srv=off, and no apply-stall.
        let st = test_state(24, 80);
        let line = wedge_debug_summary(&st, 0);
        assert!(line.contains("reack=no"), "{line}");
        assert!(line.contains("srv=off"), "{line}");
    }

    #[test]
    fn apply_frame_stashes_reack_on_short_base_diff() {
        // A Diff whose base matches applied_num but whose prefix+suffix exceeds
        // the client's (shorter) applied_data -> apply_diff None -> ReackAndWait.
        // The body must be stashed for forensics. (Auto-capture file write is
        // suppressed here via the one-shot guard so the test does no I/O.)
        let mut st = test_state(24, 80);
        st.applied_num = 5;
        st.applied_data = b"PREFIX".to_vec(); // len 6 < prefix+suffix below
        st.forensic_captured = true; // suppress the auto-capture write
        let diff = sync::make_diff(b"PREFIX_oldmiddle_SUFFIX", b"PREFIX_newmiddle_SUFFIX");
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 6,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Diff { base: 5, base_sum: None, diff },
        };
        assert!(apply_frame(&mut st, &frame), "reack re-acks (returns true)");
        assert_eq!(st.applied_num, 5, "model untouched on reack");
        let (num, base, kind, _) = st.last_reack.as_ref().expect("reack body stashed");
        assert_eq!((*num, *base), (6, 5));
        assert_eq!(*kind, FrameKind::Diff);
        assert_ne!(
            st.flags & sync::CLIENT_FLAG_RESYNC,
            0,
            "an apply-stall auto-escalates: request a Full keyframe (#90)",
        );
    }

    #[test]
    fn apply_frame_advance_clears_reack_latch() {
        let mut st = test_state(24, 80);
        st.last_reack = Some((9, 8, FrameKind::Diff, vec![1, 2, 3]));
        st.forensic_captured = true;
        // A Full keyframe advances and must clear the forensic latch so a later
        // wedge episode captures afresh.
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"\x1b[2J\x1b[Hhi".to_vec()),
        };
        assert!(apply_frame(&mut st, &frame));
        assert!(st.last_reack.is_none(), "advance clears stale reack");
        assert!(!st.forensic_captured, "advance resets the one-shot guard");
    }

    #[test]
    fn apply_frame_base_behind_applied_num_requests_resync() {
        // #95: a scrollback frame can leap applied_num past an unapplied visible
        // frame; the server then retransmits that (now stale) visible frame with
        // base < applied_num. A plain re-ack reports an applied_num whose visible
        // content we don't hold, so the server never falls back to a Full — and
        // this desync self-heals via neither reack nor base_sum. So a base BEHIND
        // applied_num must request a resync; a base AHEAD (we're merely behind and
        // a retransmit fixes it) must keep the passive re-ack.
        let mut st = test_state(24, 80);
        st.applied_num = 5;
        st.flags = 0;

        let behind = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 6,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Diff {
                base: 3,
                base_sum: None,
                diff: vec![0u8; 4],
            },
        };
        assert!(apply_frame(&mut st, &behind), "re-acks (returns true)");
        assert_eq!(st.applied_num, 5, "model untouched on base mismatch");
        assert_ne!(
            st.flags & sync::CLIENT_FLAG_RESYNC,
            0,
            "base behind applied_num (the #95 leap) requests a resync"
        );

        st.flags = 0;
        let ahead = ServerFrame {
            frame_num: 9,
            body: FrameBody::Diff {
                base: 7,
                base_sum: None,
                diff: vec![0u8; 4],
            },
            ..behind
        };
        assert!(apply_frame(&mut st, &ahead), "re-acks (returns true)");
        assert_eq!(
            st.flags & sync::CLIENT_FLAG_RESYNC,
            0,
            "base ahead of applied_num keeps the passive re-ack"
        );
    }

    #[test]
    fn apply_frame_base_sum_mismatch_resyncs_instead_of_applying() {
        // RFC 0006: a Diff whose base NUMBER matches applied_num but whose base
        // CHECKSUM does not (our applied_data diverged from the server's diff
        // base) must NOT be applied -- it would short-base wedge or silently
        // corrupt (#94). The client re-acks and requests a resync.
        let mut st = test_state(24, 80);
        let full = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"\x1b[2J\x1b[Hhello".to_vec()),
        };
        assert!(apply_frame(&mut st, &full));
        assert_eq!(st.applied_num, 1);
        let baseline = st.applied_data.clone();
        let diff = sync::make_diff(&baseline, b"\x1b[2J\x1b[Hhello world");

        // Matching base checksum: applies and advances, no resync.
        let good = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 2,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Diff {
                base: 1,
                base_sum: Some(sync::base_checksum(&baseline)),
                diff: diff.clone(),
            },
        };
        assert!(apply_frame(&mut st, &good));
        assert_eq!(st.applied_num, 2, "matching checksum applies");
        assert_eq!(
            st.flags & sync::CLIENT_FLAG_RESYNC,
            0,
            "no resync on a matching base",
        );

        // Divergent base checksum: must NOT apply; re-ack + resync.
        let wrong = sync::base_checksum(&st.applied_data).wrapping_add(1);
        let before_mismatch = st.stats.apply_snapshot().base_sum_mismatch;
        let bad = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 3,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Diff {
                base: 2,
                base_sum: Some(wrong),
                diff: diff.clone(),
            },
        };
        assert!(apply_frame(&mut st, &bad), "re-acks (returns true)");
        assert_eq!(st.applied_num, 2, "divergent base is not applied");
        assert_ne!(
            st.flags & sync::CLIENT_FLAG_RESYNC,
            0,
            "divergent base requests a resync",
        );
        assert!(
            st.stats.apply_snapshot().base_sum_mismatch > before_mismatch,
            "divergent base records a base-checksum mismatch",
        );
    }

    #[test]
    fn palette_commands_includes_both_logging_scopes() {
        let cmds = palette_commands(false, true);
        let arr = cmds.as_array().expect("commands is an array");
        let names: Vec<&str> = arr.iter().filter_map(|c| c["name"].as_str()).collect();
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("client debug logging")),
            "client logging missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("server debug logging")),
            "server logging missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("resync")),
            "resync command missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("wedge forensics")),
            "forensics command missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("wedge debug info")),
            "debug-info command missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("controller (evolved")),
            "evolved controller command missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("agent-forwarding")),
            "agent-forwarding debug command missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("echo prediction stats")),
            "prediction-stats command missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("connection health")),
            "connection-health command missing: {names:?}"
        );
        // posh#100 scroll-region optimization toggle, with a state-dependent
        // label (Disable when on, Enable when off).
        assert!(
            names.iter().any(|n| n.contains("Disable scroll-region optimization")),
            "scroll-opt disable command missing: {names:?}"
        );
        let off: Vec<String> = palette_commands(false, false)
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str().map(String::from))
            .collect();
        assert!(
            off.iter().any(|n| n == "Enable scroll-region optimization"),
            "scroll-opt enable command missing when off: {off:?}"
        );
        assert_eq!(arr.len(), 18, "expected 18 commands, got {names:?}");
    }

    #[test]
    fn dispatch_linkinfo_is_local_and_reports_liveness_fields() {
        // The "Show connection health" command is client-local (no wire send)
        // and surfaces the transport-liveness fields behind the disconnect
        // banner. Record two frame arrivals with a banner-tripping gap so the
        // summary reflects a real late-gap, not a fresh session's zeros.
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        // Use nonzero arrival timestamps: 0 is the recorder's "no prior frame"
        // sentinel (now_ms() is monotonic-from-a-base and never 0 for a live
        // frame), so the first real arrival must be > 0 to seed the baseline.
        let t0 = 1_000;
        st.stats.record_frame_arrival(t0);
        st.stats
            .record_frame_arrival(t0 + display::SERVER_LATE_AFTER + 1);
        st.stats.record_frame_empty();
        let send = dispatch_palette_action(&mut st, &raw, "session.linkinfo", &json!({}), 0);
        assert!(!send, "connection health is client-local, no wire send");
        let summary = link_debug_summary(&st, t0 + display::SERVER_LATE_AFTER + 1);
        assert!(summary.contains("link:"), "{summary}");
        assert!(summary.contains("heartbeats=1"), "{summary}");
        assert!(summary.contains("late_gaps=1"), "{summary}");
        assert!(
            summary.contains(&format!("banner>{}ms", display::SERVER_LATE_AFTER)),
            "{summary}"
        );
    }

    #[test]
    fn dispatch_predictinfo_is_local_and_reports_inactive_evolution() {
        // The "Show echo prediction stats" command is client-local (no wire
        // send). Under the default adaptive model there is no evolution loop,
        // and the summary must say so rather than showing zeros.
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "session.predictinfo", &json!({}), 0);
        assert!(!send, "prediction stats are client-local, no wire send");
        let summary = predict_debug_summary(&st);
        assert!(summary.contains("outcomes:"), "{summary}");
        assert!(summary.contains("evolution: inactive"), "{summary}");
    }

    #[test]
    fn predict_debug_summary_reports_the_gp_evolution_state() {
        // With the evolved controller selected, the summary carries the
        // evolution gauges (RFC 0007): generations, the §7.1 display decision
        // (a fresh controller sits on the adaptive shadow floor), and the
        // champion hyphence-doc record (none written yet).
        let mut st = test_state(24, 80);
        apply_echo_model(&mut st, PredictionModel::Controller, 0);
        let summary = predict_debug_summary(&st);
        assert!(summary.contains("echo model: Controller"), "{summary}");
        assert!(summary.contains("evolution: gen=0"), "{summary}");
        assert!(summary.contains("shadow (adaptive floor)"), "{summary}");
        assert!(
            summary.contains("champion doc: (none written yet)"),
            "{summary}"
        );
    }

    #[test]
    fn agent_debug_summary_reports_off_then_on_state() {
        let mut st = test_state(24, 80);
        assert!(
            agent_debug_summary(&st).contains("off"),
            "no agent configured => off"
        );
        st.agent = Some(crate::remote::agent::AgentClient::new("/tmp/agent.sock".into()));
        st.agent_seen = true;
        let s = agent_debug_summary(&st);
        assert!(s.contains("agent-fwd: on"), "summary: {s}");
        assert!(s.contains("peer-advertised=yes"), "summary: {s}");
        assert!(s.contains("/tmp/agent.sock"), "summary: {s}");
        // Server line present; with no diag received yet it reads "not yet".
        assert!(s.contains("server: (state not yet received)"), "summary: {s}");

        // Once a server agent-diag arrives, the server line shows that state.
        st.last_server_diag = Some(caps::ServerDiag {
            current_num: 5,
            acked_num: 4,
            term_gen: 10,
            outstanding: 1,
            pty_open: true,
            pid: 555,
            agent: Some(caps::AgentDiag {
                live_channels: 2,
                next_channel_id: 3,
                symlink_ok: true,
            }),
        });
        let s = agent_debug_summary(&st);
        assert!(s.contains("server: endpoint=up"), "summary: {s}");
        assert!(s.contains("channels=2"), "summary: {s}");
        assert!(s.contains("next_chan=3"), "summary: {s}");
        assert!(s.contains("symlink=ok"), "summary: {s}");
    }

    #[test]
    fn dispatch_server_logging_sets_one_shot_wire_flag() {
        // logging.set scope=server requests the toggle over the wire (#3): an
        // idempotent one-shot flag, not a client-local change.
        let raw = pty_raw_mode();

        let mut on = test_state(24, 80);
        let send = dispatch_palette_action(
            &mut on,
            &raw,
            "logging.set",
            &json!({ "scope": "server", "enabled": true }),
            0,
        );
        assert!(send, "server-logging toggle sends promptly");
        assert_ne!(on.flags & sync::CLIENT_FLAG_LOG_ON, 0, "LOG_ON set");
        assert_eq!(on.flags & sync::CLIENT_FLAG_LOG_OFF, 0);

        let mut off = test_state(24, 80);
        dispatch_palette_action(
            &mut off,
            &raw,
            "logging.set",
            &json!({ "scope": "server", "enabled": false }),
            0,
        );
        assert_ne!(off.flags & sync::CLIENT_FLAG_LOG_OFF, 0, "LOG_OFF set");
        assert_eq!(off.flags & sync::CLIENT_FLAG_LOG_ON, 0);
    }

    // Integration of the agent-use notice (#96) with consume_agent_caps: a
    // server frame carrying an OPEN record must surface the rate-limited
    // banner via the NotificationEngine. Exercises the real hook, not just the
    // AgentNotice unit.
    #[test]
    fn consume_agent_caps_shows_notice_on_channel_open() {
        use crate::remote::agent::{AgentClient, AgentNotice};
        use crate::remote::sync::{AgentRecord, AgentStream, RecordKind};
        use std::os::unix::net::UnixListener;
        use std::path::PathBuf;

        // A fake local agent so the proxy's OPEN connect succeeds.
        let sock = PathBuf::from(format!("/tmp/posh-notice-test-{}.sock", std::process::id()));
        std::fs::remove_file(&sock).ok();
        let _listener = UnixListener::bind(&sock).unwrap();

        let mut st = test_state(24, 80);
        st.agent = Some(AgentClient::new(sock.clone()));
        st.agent_notice = Some(AgentNotice::new(false, "box"));

        // Build the server's agent caps the way the server would: frame an OPEN
        // record onto an AgentStream and encode its pending bytes as AGENT_DATA.
        let mut server_stream = AgentStream::new();
        server_stream.send(&AgentRecord {
            channel: 1,
            kind: RecordKind::Open,
            payload: Vec::new(),
        });
        let mut server_caps = vec![caps::Cap {
            id: caps::CAP_AGENT_FORWARD,
            payload: vec![],
        }];
        server_caps.extend(caps::encode_agent_data(
            server_stream.send_base(),
            server_stream.pending(),
        ));

        let frame = ServerFrame {
            flags: 0,
            caps: caps::own_table(&server_caps),
            frame_num: 0,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Empty,
        };
        consume_agent_caps(&mut st, &frame);

        let msg = st.notify.message();
        assert!(
            msg.contains("box") && msg.contains("agent"),
            "expected an agent-use notice naming the host, got {msg:?}"
        );
        std::fs::remove_file(&sock).ok();
    }

    /// ClientState over a throwaway loopback connection, for unit tests
    /// of frame application and composition.
    fn test_state(rows: u16, cols: u16) -> ClientState {
        let key = Key::random();
        let conn = Connection::client("127.0.0.1:9".parse().unwrap(), &key).unwrap();
        ClientState {
            conn,
            fragmenter: Fragmenter::new(),
            outbox: InputOutbox::new(),
            rows,
            cols,
            flags: 0,
            last_send: 0,
            last_heard: 0,
            applied_num: 0,
            applied_data: Vec::new(),
            server_term: Terminal::with_scrollback(rows, cols, 0),
            scrollback: ScrollbackRing::new(SCROLLBACK_RING_DEPTH),
            suppress_scrollback_once: false,
            sb2_epoch: None,
            sb2_rows: 0,
            last_drawn: Snapshot::blank(rows, cols),
            initialized: false,
            last_wheel: false,
            predict: predict::build(PredictionModel::Never, RenderStyle::Replace, false).0,
            renderer: predict::build(PredictionModel::Never, RenderStyle::Replace, false).1,
            predict_model: PredictionModel::Never,
            predict_render: RenderStyle::Replace,
            last_metrics: predict::MetricVector::unavailable(),
            remote_metrics: [f64::NAN; caps::METRICS_FIELDS],
            predict_overwrite: false,
            notify: NotificationEngine::new(0),
            grab_mouse: GrabMouse::Off,
            mouse_filter: MouseFilter::default(),
            palette_keys: PaletteKeyNormalizer::default(),
            quit_pending: false,
            shutdown_requested: false,
            shutdown_requested_at: 0,
            shutdown_seen: false,
            exit_status: 0,
            last_render_state: (u64::MAX, u64::MAX),
            last_render_overlays: false,
            last_painted_gen: 0,
            last_skip_log_gen: u64::MAX,
            scroll_offset: 0,
            last_scroll_state: None,
            echo_on: false,
            server_log_on: false,
            scroll_opt: true,
            input_sent: VecDeque::new(),
            framesync: framesync::FrameSync::DumpDiff,
            applier: Box::new(framesync::DumpDiff),
            stats: Stats::new(),
            palette: None,
            last_reack: None,
            forensic_captured: false,
            want_server_diag: false,
            last_server_diag: None,
            agent: None,
            agent_stream: sync::AgentStream::new(),
            agent_seen: false,
            agent_notice: None,
            wedge_seen: false,
        }
    }

    // Faithful reproduction harness for the apply-stall wedge (#2/#90). Drives a
    // REAL server_loop over loopback UDP with a shell that floods content +
    // frequent OSC 2 title changes (the suspected trigger), applies frames
    // through the REAL apply_frame, and induces packet loss at the client app
    // layer so the server's retransmit + diff-base management is exercised.
    //
    // The invariant: in a correct system the server always diffs against the
    // frame the client acked, so base == applied_num and apply_diff never
    // returns None. A nonzero `reack` (the SHORT_BASE apply-stall) means the
    // wedge reproduced -- we panic with the captured forensic fields.
    //
    // #[ignore] so it stays OUT of the merge gate (real PTY + threads + UDP +
    // an 8s run); invoke explicitly: `cargo test -p posh -- --ignored wedge_repro`.
    #[test]
    #[ignore = "apply-stall reproduction harness; run with --ignored"]
    fn wedge_repro_server_loop_with_loss_and_titles() {
        let key = Key::random();
        let (server_conn, port) = Connection::server((62300, 62399), &key, Family::Inet).unwrap();
        // Pace output with a busy inner loop so the screen keeps changing across
        // the whole run (a raw flood finishes in <1s on loopback and the server
        // then goes quiet, starving the harness). Frequent OSC 2 title changes
        // are the suspected trigger.
        let script = "i=0; while [ $i -lt 100000 ]; do \
                      printf '\\033]2;ttl-%d\\007line %d: the quick brown fox jumps over\\r\\n' $i $i; \
                      j=0; while [ $j -lt 250 ]; do j=$((j+1)); done; \
                      i=$((i+1)); done; sleep 30";
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), script.into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || {
            crate::remote::server::server_loop(server_conn, child, 24, 80, None)
        });

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();
        let mut st = test_state(24, 80);

        // Deterministic loss PRNG so a reproduction is replayable.
        let mut seed = 0xdead_beef_cafe_f00du64;
        let mut roll = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 40) % 100
        };
        const DROP_PCT: u64 = 35;

        let deadline = now_ms() + 8_000;
        let mut frames_applied = 0u64;
        while now_ms() < deadline {
            // Advertise the real caps so the server interleaves scrollback with
            // visible frames — historically (#95) the wedge ingredient: a v1
            // scrollback frame advanced applied_num past an unapplied visible
            // frame -> stale visible baseline -> apply-stall. Under RFC 0009
            // the client advertises SCROLLBACK2, the server delivers rows as a
            // cumulative offset stream outside the frame sequence, and the
            // leap is impossible by construction — this harness now validates
            // exactly that (§4), plus the v2 accumulation under 35% loss.
            let caps = outgoing_caps(&mut st);
            let msg = ClientMessage {
                flags: st.flags,
                caps,
                acked_frame: st.applied_num,
                rows: 24,
                cols: 80,
                input_base: 0,
                input: Vec::new(),
            };
            st.flags &= !sync::CLIENT_FLAG_RESYNC; // one-shot, like the real client
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                conn.send(&frag.to_bytes()).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            loop {
                match conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                            continue;
                        };
                        let Some(assembled) = assembly.add(frag) else {
                            continue;
                        };
                        let Ok(frame) = ServerFrame::decode(&assembled) else {
                            continue;
                        };
                        if roll() < DROP_PCT {
                            continue; // induced packet loss: never reaches apply
                        }
                        // Capture pre-apply state so the reacking frame is fully
                        // described (the drain would otherwise clear last_reack).
                        let before = st.stats.apply_snapshot().reack;
                        let pre_num = st.applied_num;
                        let pre_len = st.applied_data.len();
                        let desc = match &frame.body {
                            FrameBody::Full(b) => format!("Full(len={})", b.len()),
                            FrameBody::Diff { base, diff, .. } => {
                                format!("Diff(base={base}, difflen={})", diff.len())
                            }
                            FrameBody::Morph { base, escapes, .. } => {
                                format!("Morph(base={base}, len={})", escapes.len())
                            }
                            FrameBody::Scrollback { base, rows } => {
                                format!("Scrollback(base={base}, rows={})", rows.len())
                            }
                            FrameBody::Scrollback2 {
                                epoch,
                                row_offset,
                                rows,
                            } => format!(
                                "Scrollback2(epoch={epoch}, off={row_offset}, rows={})",
                                rows.len()
                            ),
                            FrameBody::Empty => "Empty".to_string(),
                        };
                        // The full frame path (not bare apply_frame): v2 epoch
                        // adoption rides the frame's SCROLLBACK2 ack cap.
                        process_frame(&mut st, &frame);
                        frames_applied += 1;
                        if st.stats.apply_snapshot().reack > before {
                            let ps = if let FrameBody::Diff { diff, .. } = &frame.body {
                                if diff.len() >= 8 {
                                    let p = u32::from_le_bytes(diff[0..4].try_into().unwrap());
                                    let s = u32::from_le_bytes(diff[4..8].try_into().unwrap());
                                    format!(" prefix={p} suffix={s} (sum={})", p as u64 + s as u64)
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            };
                            panic!(
                                "WEDGE REPRODUCED at frame#{} body={desc}: \
                                 pre-apply applied_num={pre_num} applied_len={pre_len}{ps} \
                                 (after {frames_applied} applies)",
                                frame.frame_num,
                            );
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // Wind the server down (bounded; detach rather than hang if it resists).
        let shutdown_deadline = now_ms() + 5_000;
        let mut server_done = false;
        while now_ms() < shutdown_deadline && !server_done {
            let q = ClientMessage {
                flags: sync::CLIENT_FLAG_SHUTDOWN,
                caps: vec![],
                acked_frame: st.applied_num,
                rows: 24,
                cols: 80,
                input_base: 0,
                input: Vec::new(),
            };
            for frag in fragmenter.make_fragments(&q.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                let _ = conn.send(&frag.to_bytes());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            while let Ok(Some(payload)) = conn.recv() {
                if let Ok(frag) = sync::Fragment::from_bytes(&payload) {
                    if let Some(asm) = assembly.add(frag) {
                        if let Ok(frame) = ServerFrame::decode(&asm) {
                            if frame.flags & sync::FLAG_SHUTDOWN != 0 {
                                server_done = true;
                            }
                        }
                    }
                }
            }
        }
        if server_done {
            let _ = server.join();
        } else {
            eprintln!("harness: server did not wind down in time; detaching its thread");
        }

        assert!(frames_applied > 0, "transport never connected (0 frames applied)");
        // RFC 0006 corruption detector (#94): with #95 + #7 the diff base never
        // diverges, so the base checksum never mismatches. A nonzero count means a
        // base divergence was caught -- the exact condition that, unchecked,
        // short-base wedges (#90) or silently corrupts the screen (#94). This
        // catches a divergence even when apply_diff would have returned Some
        // (garbage), which the reack assertion alone cannot.
        let bsm = st.stats.apply_snapshot().base_sum_mismatch;
        assert_eq!(
            bsm, 0,
            "base divergence detected ({bsm}x): the client's diff base diverged \
             from the server's (would corrupt or wedge without the RFC 0006 checksum)"
        );
        // RFC 0009: the v2 stream must actually have engaged (epoch adopted from
        // the server's SCROLLBACK2 ack) and delivered the flood's history rows —
        // guarding against the harness silently degrading to visible-only and
        // vacuously passing the wedge assertions.
        assert!(
            st.sb2_epoch.is_some(),
            "SCROLLBACK2 never negotiated: the harness is not exercising v2"
        );
        assert!(
            st.sb2_rows > 0,
            "no v2 scrollback rows accepted despite a scrolling flood"
        );
        eprintln!(
            "harness CLEAN: {frames_applied} frames applied, reack=0, \
             base_sum_mismatch=0, sb2_rows={}",
            st.sb2_rows
        );
    }

    #[test]
    fn dispatch_echo_set_swaps_predictor_and_banners() {
        let raw = pty_raw_mode();
        let mut st = test_state(3, 30);
        st.predict_model = PredictionModel::Never;
        st.initialized = true;

        let send = dispatch_palette_action(&mut st, &raw, "echo.set", &json!({ "model": "optimistic" }), 0);
        assert!(!send, "echo.set is client-local, no prompt send");
        assert_eq!(st.predict_model, PredictionModel::Optimistic);
        assert!(st.notify.message().contains("Optimistic"), "banner names the new model");
        assert!(!st.initialized, "swapping the predictor forces a clean repaint");
    }

    #[test]
    fn compose_skips_idle_ticks_but_not_time_driven_banners() {
        // github #35: idle ticks must not rebuild the full-grid snapshot —
        // but the skip may never eat time-driven output: the lateness
        // banner appears (and counts up) without any model change.
        let mut st = test_state(3, 30);
        assert!(
            !compose_frame(&mut st, 0).is_empty(),
            "first compose paints from scratch"
        );
        assert!(
            compose_frame(&mut st, 100).is_empty(),
            "idle tick composes nothing"
        );
        let late = compose_frame(&mut st, 10_000);
        assert!(
            String::from_utf8_lossy(&late).contains("Last contact"),
            "lateness banner must survive the idle fast path: {late:?}"
        );
        assert!(
            !compose_frame(&mut st, 11_000).is_empty(),
            "banner count-up keeps rendering"
        );
    }

    #[test]
    fn compose_renders_on_applied_frames() {
        let mut st = test_state(3, 20);
        let _ = compose_frame(&mut st, 0);
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"hello".to_vec()),
        };
        assert!(apply_frame(&mut st, &frame));
        let bytes = compose_frame(&mut st, 10);
        assert!(
            String::from_utf8_lossy(&bytes).contains("hello"),
            "applied frame must compose: {bytes:?}"
        );
        assert!(
            compose_frame(&mut st, 20).is_empty(),
            "and the tick after it is idle again"
        );
    }

    /// A lossy in-memory paint destination for driving the real `render_to`.
    /// Feeds an outer `Terminal` (the physical tty the user sees), but DROPS the
    /// whole write on the frame index in `drop_on` — modeling write_all_retry
    /// spending its budget on a degraded link. The dropped frame reports 0 bytes
    /// written, exactly the short-write signal render_to reacts to.
    struct LossySink {
        tty: Terminal,
        frame: usize,
        drop_on: Option<usize>,
    }

    impl TtySink for LossySink {
        fn write_budget(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let dropped = self.drop_on == Some(self.frame);
            self.frame += 1;
            if dropped {
                Ok(0) // budget spent before any byte drained
            } else {
                self.tty.process(bytes);
                Ok(bytes.len())
            }
        }
    }

    // Regression guard for the "top line permanently unechoed" bug, driving the
    // REAL render_to through a lossy sink (#127 — no hand-mirrored copy).
    //
    // The renderer is DIFFERENTIAL: compose_frame diffs the model against
    // st.last_drawn (its belief about the physical tty) and emits only changed
    // cells, then commits `st.last_drawn = next` unconditionally. render_to
    // writes those bytes through the sink; a dropped write (budget spent on a
    // degraded link) leaves last_drawn ahead of the real screen, and — because
    // we only ever emit diffs — nothing would repaint those cells again. The
    // fix: on a short/failed write render_to forces a full repaint next tick
    // (initialized = false), resyncing the whole screen.
    //
    // Models the git-commit editor case: while the banner is up (link was late),
    // the app owns row 0; the paint that would clear the banner is DROPPED; the
    // user then types and the server echoes. Without the fix the stale banner
    // permanently corrupts the top line; with it, the forced repaint restores it.
    #[test]
    fn dropped_paint_resyncs_top_line_via_forced_repaint() {
        let (rows, cols) = (6u16, 40u16);
        let mut st = test_state(rows, cols);

        // The alt-screen editor: a commit buffer. Row 0 is the message first
        // line, where the cursor sits.
        let editor = |line1: &str| {
            let mut s = String::from("\x1b[?1049h\x1b[2J\x1b[H");
            s.push_str(line1); // row 0: the message first line
            s.push_str("\r\n\r\n# Please enter the commit message");
            s.push_str("\r\n# On branch master\r\n#\r\n# Changes:");
            // Park the cursor back on row 0, col after line1.
            s.push_str(&format!("\x1b[1;{}H", line1.chars().count() + 1));
            s.into_bytes()
        };

        let push_full = |st: &mut ClientState, num: u64, screen: Vec<u8>| {
            let frame = ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: num,
                input_ack: 0,
                echo_ack: 0,
                body: FrameBody::Full(screen),
            };
            assert!(apply_frame(st, &frame));
        };

        // Frame indices seen by the sink: 0 = initial paint, 1 = banner-appears,
        // 2 = banner-clear (DROPPED), 3.. = the typed "hello" echoes.
        let mut sink = LossySink {
            tty: Terminal::with_scrollback(rows, cols, 0),
            frame: 0,
            drop_on: Some(2),
        };

        // 1. Editor opens; fresh contact. Paint lands.
        push_full(&mut st, 1, editor(""));
        render_to(&mut st, 0, &mut sink);

        // 2. Link goes late (>6.5s): the banner appears on row 0. Paint lands.
        render_to(&mut st, 7000, &mut sink);
        assert!(
            row_text(&Snapshot::from_term(&sink.tty), 0).starts_with("posh:"),
            "banner should cover the tty row 0 while late"
        );

        // 3. Contact resumes: the banner-clear paint is DROPPED (the degraded→
        //    recovered transition where the write budget is spent). last_drawn
        //    advances anyway, so the tty is now stale — render_to must latch
        //    initialized = false for the next tick.
        st.notify.server_heard(9000);
        render_to(&mut st, 9000, &mut sink);
        assert!(
            !st.initialized,
            "render_to must force a repaint after a dropped write"
        );
        // Confirm the drop really did desync the tty (else the scenario is moot).
        assert!(
            row_text(&Snapshot::from_term(&sink.tty), 0).contains("Last contact"),
            "the dropped paint left the stale banner on the physical tty"
        );

        // 4. The user types "hello"; the server echoes each keystroke. The very
        //    next render_to composes with initialized = false, so it full-repaints
        //    and resyncs the whole screen onto the tty.
        for (i, prefix) in ["h", "he", "hel", "hell", "hello"].iter().enumerate() {
            push_full(&mut st, 2 + i as u64, editor(prefix));
            render_to(&mut st, 9100 + i as u64 * 10, &mut sink);
        }

        // With the fix, the physical tty converges on the model: every row
        // matches, with the top line showing exactly the typed text and no
        // banner residue.
        let tty_snap = Snapshot::from_term(&sink.tty);
        let model_snap = Snapshot::from_term(&st.server_term);
        for r in 0..rows as usize {
            assert_eq!(
                row_text(&tty_snap, r).trim_end(),
                row_text(&model_snap, r).trim_end(),
                "row {r} on the physical tty diverged from the model after recovery"
            );
        }
        assert_eq!(
            row_text(&tty_snap, 0).trim_end(),
            "hello",
            "the typed text echoed onto the physical top line after the repaint"
        );
    }

    /// The printable text of one snapshot row, spacer halves elided.
    fn row_text(s: &Snapshot, row: usize) -> String {
        s.cells[row]
            .iter()
            .filter(|c| c.width > 0)
            .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
            .collect()
    }

    /// RFC 0002 §3: a `BODY_SCROLLBACK` advances the accumulated ring in row
    /// order without touching the visible model, and only when the client is
    /// at the body's base.
    #[test]
    fn scrollback_frames_accumulate_in_ring_order() {
        let mut st = test_state(3, 20);
        // A visible frame first so applied_num advances to a real base.
        let visible = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"prompt$ ".to_vec()),
        };
        assert!(apply_frame(&mut st, &visible));
        assert_eq!(st.applied_num, 1);
        assert!(st.scrollback.is_empty());

        // Two scrollback frames in sequence, each anchored to the prior.
        let sb1 = ServerFrame {
            frame_num: 2,
            body: FrameBody::Scrollback {
                base: 1,
                rows: vec![b"line one\r\n".to_vec(), b"line two\r\n".to_vec()],
            },
            ..visible.clone()
        };
        assert!(apply_frame(&mut st, &sb1));
        assert_eq!(st.applied_num, 2);
        assert_eq!(st.scrollback.len(), 2);
        let sb2 = ServerFrame {
            frame_num: 3,
            body: FrameBody::Scrollback {
                base: 2,
                rows: vec![b"line three\r\n".to_vec()],
            },
            ..visible.clone()
        };
        assert!(apply_frame(&mut st, &sb2));
        assert_eq!(st.applied_num, 3);
        assert_eq!(st.scrollback.len(), 3);
        assert_eq!(st.scrollback.row(0), Some(&b"line one\r\n"[..]));
        assert_eq!(st.scrollback.row(2), Some(&b"line three\r\n"[..]));

        // A body whose base does not match the client's state is not applied
        // (no double-append), and re-acks our newer state.
        let stale = ServerFrame {
            frame_num: 4,
            body: FrameBody::Scrollback {
                base: 2, // we are at 3
                rows: vec![b"dup\r\n".to_vec()],
            },
            ..visible.clone()
        };
        assert!(apply_frame(&mut st, &stale));
        assert_eq!(st.scrollback.len(), 3, "base mismatch must not append");
        assert_eq!(st.applied_num, 3);
    }

    /// RFC 0002 §1/§4: the client advertises `SCROLLBACK` in steady state,
    /// but ceases for exactly the post-resize message so the server restarts
    /// appended-row counting at the new width, then resumes.
    #[test]
    fn resize_ceases_scrollback_advertisement_for_one_message() {
        let mut st = test_state(5, 20);
        // Steady state: advertised every message.
        let caps = outgoing_caps(&mut st);
        assert!(caps::find(&caps, caps::CAP_SCROLLBACK).is_some());

        // Simulate the SIGWINCH bookkeeping: ring dropped, advertisement
        // suppressed once.
        st.scrollback.append(&[b"row\r\n".to_vec()]);
        st.scrollback.clear();
        st.suppress_scrollback_once = true;
        assert!(st.scrollback.is_empty());

        // The resize message must NOT advertise scrollback (still carries
        // the rest of the table).
        let caps = outgoing_caps(&mut st);
        assert!(
            caps::find(&caps, caps::CAP_SCROLLBACK).is_none(),
            "resize message must cease scrollback"
        );
        assert!(caps::find(&caps, caps::CAP_EXIT_STATUS).is_some());

        // And the very next message re-advertises to resume accumulation.
        let caps = outgoing_caps(&mut st);
        assert!(
            caps::find(&caps, caps::CAP_SCROLLBACK).is_some(),
            "scrollback resumes after the resize message"
        );
    }

    #[test]
    fn scrollback2_apply_rules_never_touch_applied_num() {
        // RFC 0009 §3/§4: dup discard, in-order append, forward-jump accept,
        // partial-overlap discard, epoch gating — and applied_num is inert
        // throughout (the class-killing invariant).
        let mut st = test_state(5, 20);
        st.applied_num = 7;
        st.sb2_epoch = Some(3);
        let body = |epoch: u8, off: u64, n: usize| ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 99,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Scrollback2 {
                epoch,
                row_offset: off,
                rows: (0..n).map(|i| format!("r{i}\r\n").into_bytes()).collect(),
            },
        };
        // Wrong epoch: a stale in-flight body from a superseded row space.
        assert!(apply_frame(&mut st, &body(2, 0, 2)));
        assert_eq!(st.sb2_rows, 0);
        assert!(st.scrollback.is_empty());
        // In-order append.
        assert!(apply_frame(&mut st, &body(3, 0, 2)));
        assert_eq!(st.sb2_rows, 2);
        assert_eq!(st.scrollback.len(), 2);
        // Fully-covered dup: discarded (idempotent under retransmit).
        assert!(apply_frame(&mut st, &body(3, 0, 2)));
        assert_eq!(st.sb2_rows, 2);
        assert_eq!(st.scrollback.len(), 2);
        // Partial overlap: discarded (a conforming server re-anchors at our ack).
        assert!(apply_frame(&mut st, &body(3, 1, 3)));
        assert_eq!(st.sb2_rows, 2);
        // Forward jump (server-ring eviction): accepted; the gap is lost
        // history and the partial view is first-class (FDR 0005).
        assert!(apply_frame(&mut st, &body(3, 10, 1)));
        assert_eq!(st.sb2_rows, 11);
        assert_eq!(st.scrollback.len(), 3);
        // The invariant that kills the #95/#117 class:
        assert_eq!(st.applied_num, 7, "scrollback v2 must never advance applied_num");
    }

    #[test]
    fn scrollback2_epoch_adoption_resets_ring_and_count() {
        // RFC 0009 §1: the server's ack cap names the epoch; a change clears
        // the ring and zeroes the cumulative count; a repeat does not.
        let mut st = test_state(5, 20);
        st.scrollback.append(&[b"old\r\n".to_vec()]);
        st.sb2_rows = 5;
        st.sb2_epoch = Some(1);
        let frame = ServerFrame {
            flags: 0,
            caps: vec![caps::encode_scrollback2_ack(2)],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Empty,
        };
        process_frame(&mut st, &frame);
        assert_eq!(st.sb2_epoch, Some(2));
        assert_eq!(st.sb2_rows, 0);
        assert!(st.scrollback.is_empty(), "a fresh epoch clears the ring");
        st.sb2_rows = 4;
        st.scrollback.append(&[b"kept\r\n".to_vec()]);
        process_frame(&mut st, &frame);
        assert_eq!(st.sb2_rows, 4, "the same epoch again must not reset");
        assert_eq!(st.scrollback.len(), 1);
    }

    #[test]
    fn outgoing_caps_advertises_v2_and_drops_v1_once_acked() {
        // RFC 0009 §1: v2 always advertised (it carries the per-message ack);
        // the v1 entry rides only until the server acknowledges v2.
        let mut st = test_state(5, 20);
        let caps = outgoing_caps(&mut st);
        assert!(caps::find(&caps, caps::CAP_SCROLLBACK2).is_some());
        assert!(caps::find(&caps, caps::CAP_SCROLLBACK).is_some());

        st.sb2_epoch = Some(4);
        st.sb2_rows = 123;
        let caps = outgoing_caps(&mut st);
        assert!(
            caps::find(&caps, caps::CAP_SCROLLBACK).is_none(),
            "v1 dropped once v2 is acked"
        );
        let entry = caps::find(&caps, caps::CAP_SCROLLBACK2).expect("v2 advertised");
        let c = caps::decode_scrollback2_client(&entry.payload).unwrap();
        assert_eq!((c.epoch, c.acked_rows), (4, 123));
    }

    /// RFC 0002 §3: a `Full` visible reset re-establishes the visible screen
    /// but MUST NOT clear the durable accumulated scrollback ring.
    #[test]
    fn full_body_preserves_accumulated_scrollback() {
        let mut st = test_state(3, 20);
        let base = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"a".to_vec()),
        };
        assert!(apply_frame(&mut st, &base));
        let sb = ServerFrame {
            frame_num: 2,
            body: FrameBody::Scrollback {
                base: 1,
                rows: vec![b"kept\r\n".to_vec()],
            },
            ..base.clone()
        };
        assert!(apply_frame(&mut st, &sb));
        assert_eq!(st.scrollback.len(), 1);

        // A later Full (e.g. after loss) resets the visible model only.
        let full = ServerFrame {
            frame_num: 3,
            body: FrameBody::Full(b"recovered".to_vec()),
            ..base
        };
        assert!(apply_frame(&mut st, &full));
        assert_eq!(st.scrollback.len(), 1, "Full must not clear the ring");
        assert_eq!(st.scrollback.row(0), Some(&b"kept\r\n"[..]));
    }

    // --- scrollback scroll-view (FDR 0005) -----------------------------------
    // (the pure `MouseFilter` scroll-tick test moved to `remote::scrollview`;
    // the tests below exercise the offset/compose wiring against `ClientState`.)

    #[test]
    fn scroll_offset_clamps_to_ring_and_returns_to_live_at_bottom() {
        let mut st = test_state(5, 20);
        for _ in 0..2 {
            st.scrollback.append(&[b"x\r\n".to_vec()]); // ring depth 2
        }
        scroll_by(&mut st, 1); // +WHEEL_STEP lines, clamped to the ring depth
        assert_eq!(st.scroll_offset, 2);
        scroll_by(&mut st, -1); // back down past the bottom → live view
        assert_eq!(st.scroll_offset, 0);
    }

    #[test]
    fn append_while_scrolled_bumps_offset_to_freeze_viewport() {
        let mut st = test_state(3, 20);
        for i in 0..5 {
            st.scrollback.append(&[format!("r{i}\r\n").into_bytes()]);
        }
        st.applied_num = 1;
        set_scroll(&mut st, 5); // scrolled to the top of history
        // Two more rows scroll off while we are scrolled up.
        let sb = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 2,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Scrollback {
                base: 1,
                rows: vec![b"r5\r\n".to_vec(), b"r6\r\n".to_vec()],
            },
        };
        assert!(apply_frame(&mut st, &sb));
        assert_eq!(st.scrollback.len(), 7);
        assert_eq!(
            st.scroll_offset, 7,
            "offset bumped by the appended rows so the viewport stays anchored"
        );
    }

    #[test]
    fn scroll_frame_renders_history_window_with_indicator() {
        let mut st = test_state(5, 20);
        for s in ["line0", "line1", "line2", "line3"] {
            st.scrollback.append(&[format!("{s}\r\n").into_bytes()]);
        }
        set_scroll(&mut st, 4); // scroll to the top of history
        let bytes = compose_scroll_frame(&mut st);
        assert!(!bytes.is_empty(), "scroll frame must render");

        // Replay onto a fresh terminal of the same size and read it back.
        let mut v = Terminal::with_scrollback(5, 20, 0);
        v.process(&bytes);
        let snap = Snapshot::from_term(&v);
        let row = |r: u16| -> String {
            (0..20)
                .filter_map(|c| snap.cell(r, c))
                .map(|cell| if cell.ch == '\0' { ' ' } else { cell.ch })
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert!(row(0).contains("SCROLLBACK"), "indicator on the top row: {:?}", row(0));
        assert!(row(1).contains("line1"), "history rendered below the bar: {:?}", row(1));
    }

    #[test]
    fn scroll_frame_is_memoized_until_state_changes() {
        let mut st = test_state(5, 20);
        for _ in 0..4 {
            st.scrollback.append(&[b"h\r\n".to_vec()]);
        }
        set_scroll(&mut st, 3);
        assert!(!compose_scroll_frame(&mut st).is_empty(), "first scroll frame paints");
        assert!(
            compose_scroll_frame(&mut st).is_empty(),
            "unchanged scroll state re-renders nothing"
        );
        set_scroll(&mut st, 2); // offset changed → repaint
        assert!(!compose_scroll_frame(&mut st).is_empty());
    }

    #[test]
    fn deccolm_frame_does_not_resize_local_model_past_tty_width() {
        let mut st = test_state(24, 80);
        let (rows, cols) = (24u16, 80u16);

        // Server dump replaying 132-column mode (DECSET 40 allows DECCOLM,
        // DECSET 3 switches): the local model must stay at the tty size or
        // every subsequent render paints a 132-col image onto 80 cols.
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"\x1b[?40h\x1b[?3h132-col mode".to_vec()),
        };
        assert!(apply_frame(&mut st, &frame));
        assert_eq!(st.server_term.rows(), rows);
        assert_eq!(
            st.server_term.cols(),
            cols,
            "DECCOLM resized the client model away from the tty width"
        );
    }
}
