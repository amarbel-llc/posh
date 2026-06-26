//! RFC 0007 §3 server-side metric gathering: the remote-host signals the server
//! samples and forwards to the client (via `CAP_METRICS`, #7) to populate the
//! metric vector's `remote_*` terminals.
//!
//! Host stats are read from Linux `/proc`; on other platforms they report
//! `None` (→ `NaN` terminals client-side, which the evolved program tolerates).
//! Categorical identities (frontmost app, foreground process) are reduced to a
//! stable numeric id via [`category_id`] before they leave the server, so no
//! string crosses the wire or reaches the GP.

// Scaffold surface (RFC 0007 §3): consumed once the server samples these into
// each frame and CAP_METRICS (#7) forwards them. Allow until then.
#![allow(dead_code)]

/// The server-sampled remote-host signals (RFC 0007 §2 `remote_*` terminals).
/// A `None` field becomes a `NaN` terminal on the client.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RemoteMetrics {
    /// 1-minute load average divided by CPU count (so ~1.0 == fully loaded).
    pub load1: Option<f64>,
    /// `MemAvailable / MemTotal`.
    pub mem_avail_frac: Option<f64>,
    /// Hashed frontmost-app identity (terminal title / foreground app).
    pub frontmost_app: Option<u32>,
    /// Process count in the session.
    pub proc_count: Option<u32>,
    /// Hashed foreground-process command name.
    pub fg_proc_id: Option<u32>,
}

impl RemoteMetrics {
    /// The remote terminals in `CAP_METRICS` order (RFC 0007 §3): load1,
    /// mem_avail_frac, frontmost_app, proc_count, fg_proc_id. Absent values are
    /// `NaN`; categorical ids widen to `f64`.
    pub fn to_terminals(&self) -> [f64; 5] {
        [
            self.load1.unwrap_or(f64::NAN),
            self.mem_avail_frac.unwrap_or(f64::NAN),
            self.frontmost_app.map(f64::from).unwrap_or(f64::NAN),
            self.proc_count.map(f64::from).unwrap_or(f64::NAN),
            self.fg_proc_id.map(f64::from).unwrap_or(f64::NAN),
        ]
    }
}

/// Stable FNV-1a 32-bit hash of a categorical string into the GP id space
/// (RFC 0007 §2). Stable across sessions and posh versions so a persisted
/// genome's equality/branch tests stay valid. Empty input maps to `0`
/// ("unknown"), distinct from any real hash in practice.
pub fn category_id(s: &str) -> u32 {
    if s.is_empty() {
        return 0;
    }
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Parse the normalized 1-minute load from `/proc/loadavg` contents given the
/// CPU count. Split out for testing; the live reader passes the file contents.
fn parse_load1_normalized(loadavg: &str, ncpu: usize) -> Option<f64> {
    let load1: f64 = loadavg.split_whitespace().next()?.parse().ok()?;
    let ncpu = ncpu.max(1) as f64;
    Some(load1 / ncpu)
}

/// Parse `MemAvailable / MemTotal` from `/proc/meminfo` contents. Split out for
/// testing; the live reader passes the file contents.
fn parse_mem_avail_frac(meminfo: &str) -> Option<f64> {
    let mut total: Option<f64> = None;
    let mut avail: Option<f64> = None;
    for line in meminfo.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_kb(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb(v);
        }
    }
    let (t, a) = (total?, avail?);
    if t <= 0.0 {
        return None;
    }
    Some(a / t)
}

/// Parse a `/proc/meminfo` value line ("   16384256 kB") to its kB number.
fn parse_kb(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse::<f64>().ok()
}

/// Sample the host-stat terminals (`load1`, `mem_avail_frac`). The app/process
/// terminals are filled separately by the server (they need the session's pty
/// foreground process group); see [`RemoteMetrics`].
#[cfg(target_os = "linux")]
pub fn sample_host_stats() -> RemoteMetrics {
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    RemoteMetrics {
        load1: std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| parse_load1_normalized(&s, ncpu)),
        mem_avail_frac: std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| parse_mem_avail_frac(&s)),
        ..RemoteMetrics::default()
    }
}

/// Non-Linux: host stats unavailable (no `/proc`); reports all `None`.
#[cfg(not(target_os = "linux"))]
pub fn sample_host_stats() -> RemoteMetrics {
    RemoteMetrics::default()
}

/// Parse the session id (field 6) from `/proc/<pid>/stat`. The comm field (2nd)
/// is parenthesized and may itself contain spaces and parens, so we split on the
/// LAST `)`: the fields after it are `state ppid pgrp session …`. Split out for
/// testing.
fn parse_session_field(stat: &str) -> Option<i32> {
    let after = &stat[stat.rfind(')')? + 1..];
    let mut fields = after.split_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    let _pgrp = fields.next()?;
    fields.next()?.parse().ok()
}

/// The command name of a process (`/proc/<pid>/comm`), trimmed. `None` off Linux
/// or when the process is gone.
#[cfg(target_os = "linux")]
fn read_comm(pid: i32) -> Option<String> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[cfg(not(target_os = "linux"))]
fn read_comm(_pid: i32) -> Option<String> {
    None
}

/// Count processes in the shell's session by walking `/proc` and matching each
/// process's session id (field 6 of its stat) against `session_id`.
#[cfg(target_os = "linux")]
fn session_proc_count(session_id: i32) -> Option<u32> {
    let mut count = 0u32;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{name}/stat")) {
            if parse_session_field(&stat) == Some(session_id) {
                count += 1;
            }
        }
    }
    Some(count)
}

#[cfg(not(target_os = "linux"))]
fn session_proc_count(_session_id: i32) -> Option<u32> {
    None
}

/// Sample the full RFC 0007 §3 remote signal set for one session: host stats
/// (load/mem), the foreground app, and the session process count.
///
/// - `fg_pgid`: the pty's foreground process group ([`crate::pty::foreground_pgid`]).
/// - `title`: the session terminal's title (the strongest "frontmost app" hint;
///   shells/apps set it to the running command). Falls back to the foreground
///   process's comm when empty.
/// - `session_id`: the shell's session id (its pid — the pty session leader).
pub fn sample(fg_pgid: Option<i32>, title: &str, session_id: i32) -> RemoteMetrics {
    let mut m = sample_host_stats();
    let fg_comm = fg_pgid.and_then(read_comm);
    m.fg_proc_id = fg_comm.as_deref().map(category_id);
    m.frontmost_app = if title.is_empty() {
        m.fg_proc_id
    } else {
        Some(category_id(title))
    };
    m.proc_count = session_proc_count(session_id);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load1_normalizes_by_cpu_count() {
        let got = parse_load1_normalized("2.50 1.20 0.80 1/234 5678\n", 4).unwrap();
        assert!((got - 0.625).abs() < 1e-9);
    }

    #[test]
    fn load1_treats_zero_cpu_as_one() {
        let got = parse_load1_normalized("1.00 0 0 1/1 2\n", 0).unwrap();
        assert!((got - 1.0).abs() < 1e-9);
    }

    #[test]
    fn mem_avail_frac_from_meminfo() {
        let meminfo = "MemTotal:       16000 kB\nMemFree: 1000 kB\nMemAvailable:    8000 kB\n";
        let got = parse_mem_avail_frac(meminfo).unwrap();
        assert!((got - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mem_avail_frac_missing_field_is_none() {
        assert!(parse_mem_avail_frac("MemTotal: 16000 kB\n").is_none());
    }

    #[test]
    fn category_id_is_stable_and_distinct() {
        assert_eq!(category_id(""), 0);
        assert_eq!(category_id("vim"), category_id("vim"));
        assert_ne!(category_id("vim"), category_id("less"));
        assert_ne!(category_id("vim"), 0);
    }

    #[test]
    fn parse_session_field_simple() {
        // pid (comm) state ppid pgrp session ...
        assert_eq!(parse_session_field("42 (bash) S 1 42 99 0 -1 0"), Some(99));
    }

    #[test]
    fn parse_session_field_handles_comm_with_spaces_and_parens() {
        let stat = "1234 (weird (name) x) S 1000 1234 5678 34816 1234";
        assert_eq!(parse_session_field(stat), Some(5678));
    }
}
