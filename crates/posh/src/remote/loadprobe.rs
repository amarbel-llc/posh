//! Loaded-mux measurement probe (debug-only, `#[ignore]`d): the experiment
//! posh#143 (RFC 0011 §9.2, congestion control) and posh#144 (§9.3, flow
//! control) demand before M2 puts N sessions plus agent bulk on one
//! connection. Everything transport-level is REAL — `Connection` (AEAD, RTT
//! estimator, send pacing), `channel::seal_on`/`open_any_instruction`,
//! `Fragmenter`/`FragmentAssembly`, `AgentChannelMux` with its RTO-paced
//! cumulative outboxes — driven by bench loops copied from the
//! `mux_loop`/`agent_only_loop` send/recv skeletons over loopback UDP,
//! through an in-process impairment relay ([`LossyLink`]: one-way delay,
//! seeded random loss, and a token-bucket bottleneck with a bounded queue
//! and tail-drop, the shape that turns "bandwidth limit" into real
//! congestion signals). Only the two ends beyond the wire are synthetic:
//! the local agent is a canned bulk responder and the remote consumers are
//! request generators, because §9.2/§9.3 are questions about the transport
//! between them, not about unix-socket plumbing.
//!
//! Modeling notes, deliberate and worth knowing when reading numbers:
//!
//!   * N sessions ride the single `SESSION_CHANNEL` with a session index in
//!     the payload — M2's per-session channel ids don't exist yet, and the
//!     §4.1 discipline under measurement (session instructions precede bulk
//!     agent data in each drain) is per-SENDER, not per-channel.
//!   * Session frames and agent bulk both flow client→remote, sharing the
//!     constrained uplink — the contended-drain configuration §9.3's
//!     starvation question is about. Session frames are fire-and-forget
//!     (mosh latest-state-wins), so their delivery ratio under loss is
//!     itself a measurement, not a bug.
//!   * The relay is wall-clock, not virtual-time: runs are seeded (loss
//!     schedule) but timing-sensitive, so scenarios print aggregates and
//!     assert only harness-health floors — the `perf_probe` posture, not
//!     golden numbers.
//!
//! Not run in CI (`cargo test` skips `#[ignore]`). Run via
//! `just debug-mux-load` (release; --test-threads=1 — scenarios bind fixed
//! loopback UDP port ranges and measure timing).

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::remote::agent::AgentChannelMux;
use crate::remote::channel::{self, AgentPayload, KIND_AGENT, SESSION_CHANNEL};
use crate::remote::datagram::{Connection, Family};
use crate::remote::sync::{self, AgentRecord, RecordKind};
use crate::util;

// ---------------------------------------------------------------------------
// Seeded PRNG: xorshift64* — 3 lines, no rand dependency, reproducible loss.

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ---------------------------------------------------------------------------
// The impairment shapes.

/// One direction's link model: fixed one-way delay, independent random loss,
/// and a bottleneck of `bandwidth_bytes_per_s` with `queue_bytes` of buffer
/// (tail-drop beyond it). `bandwidth_bytes_per_s == 0` means unlimited (no
/// queue, no bandwidth loss — delay and random loss still apply).
#[derive(Clone, Copy, Debug)]
pub struct LinkShape {
    pub delay_ms: u64,
    pub loss_pct: f64,
    pub bandwidth_bytes_per_s: u64,
    pub queue_bytes: usize,
    pub seed: u64,
}

impl LinkShape {
    /// Loopback-clean: no delay, no loss, no bottleneck. The harness
    /// self-proof preset.
    pub fn lan() -> LinkShape {
        LinkShape { delay_ms: 0, loss_pct: 0.0, bandwidth_bytes_per_s: 0, queue_bytes: 0, seed: 1 }
    }
    /// 50 ms one-way, clean, unlimited: the ack-clock ceiling preset.
    pub fn wan_clean() -> LinkShape {
        LinkShape { delay_ms: 50, loss_pct: 0.0, bandwidth_bytes_per_s: 0, queue_bytes: 0, seed: 2 }
    }
    /// The bufferbloat shape: 1 Mbit/s behind a DEEP (512 KiB) queue, no
    /// loss — nothing bounds queue occupancy but the ack clock, so the srtt
    /// series shows the standing queue agent bulk builds.
    pub fn bufferbloat() -> LinkShape {
        LinkShape {
            delay_ms: 50,
            loss_pct: 0.0,
            bandwidth_bytes_per_s: 125_000,
            queue_bytes: 512 * 1024,
            seed: 5,
        }
    }
    /// The §9.2 decider: 150 ms one-way, 1% loss, 1 Mbit/s with a 64 KiB
    /// queue — the ~256 KB per-drain agent burst does not fit.
    pub fn constrained() -> LinkShape {
        LinkShape {
            delay_ms: 150,
            loss_pct: 1.0,
            bandwidth_bytes_per_s: 125_000,
            queue_bytes: 64 * 1024,
            seed: 4,
        }
    }
}

// ---------------------------------------------------------------------------
// The relay.

#[derive(Default)]
struct DirCounters {
    forwarded_dgrams: AtomicU64,
    forwarded_bytes: AtomicU64,
    dropped_loss: AtomicU64,
    dropped_queue: AtomicU64,
    queue_high_water: AtomicU64,
}

/// Per-direction shaper state: the virtual-clock bottleneck (`next_free` is
/// when the link finishes transmitting everything accepted so far) plus the
/// in-flight delay line, FIFO because departure times are monotonic.
struct Shaper {
    shape: LinkShape,
    rng: Rng,
    next_free: f64,
    queued_bytes: usize,
    line: VecDeque<(f64, Vec<u8>)>,
}

impl Shaper {
    fn new(shape: LinkShape, seed_salt: u64) -> Shaper {
        Shaper {
            shape,
            rng: Rng::new(shape.seed.wrapping_mul(0x9E3779B97F4A7C15) ^ seed_salt),
            next_free: 0.0,
            queued_bytes: 0,
            line: VecDeque::new(),
        }
    }

    /// Admits one arriving datagram at `now` (ms), or drops it.
    fn admit(&mut self, now: f64, data: Vec<u8>, ctr: &DirCounters) {
        if self.shape.loss_pct > 0.0 && self.rng.next_f64() * 100.0 < self.shape.loss_pct {
            ctr.dropped_loss.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let departure = if self.shape.bandwidth_bytes_per_s == 0 {
            now
        } else {
            if self.queued_bytes + data.len() > self.shape.queue_bytes.max(data.len()) {
                ctr.dropped_queue.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let service_ms = data.len() as f64 * 1000.0 / self.shape.bandwidth_bytes_per_s as f64;
            let start = self.next_free.max(now);
            self.next_free = start + service_ms;
            self.queued_bytes += data.len();
            ctr.queue_high_water.fetch_max(self.queued_bytes as u64, Ordering::Relaxed);
            self.next_free
        };
        self.line.push_back((departure + self.shape.delay_ms as f64, data));
    }

    /// Releases every datagram whose departure time has passed.
    fn release(&mut self, now: f64, mut deliver: impl FnMut(&[u8])) {
        while let Some((due, _)) = self.line.front() {
            if *due > now {
                break;
            }
            let (_, data) = self.line.pop_front().unwrap();
            if self.shape.bandwidth_bytes_per_s != 0 {
                self.queued_bytes -= data.len();
            }
            deliver(&data);
        }
    }
}

/// The loopback UDP impairment relay: the client aims `Connection::client`
/// at [`client_addr`](Self::client_addr); the relay forwards to the real
/// server from its server-facing socket (whose address the server adopts as
/// its roamed peer), and forwards replies back to the client address —
/// re-pinned on every client datagram, which is harmless here because the
/// bench client never roams, so it never changes within a run. AEAD passes
/// through untouched — the
/// relay never decrypts, so it cannot corrupt, only delay/drop/queue.
struct LossyLink {
    client_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    up: Arc<DirCounters>,
    down: Arc<DirCounters>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LossyLink {
    fn start(shape: LinkShape, server_addr: SocketAddr) -> LossyLink {
        let sock_c = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let sock_s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        sock_c.set_nonblocking(true).unwrap();
        sock_s.set_nonblocking(true).unwrap();
        let client_addr = sock_c.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let up = Arc::new(DirCounters::default());
        let down = Arc::new(DirCounters::default());
        let (stop2, up2, down2) = (stop.clone(), up.clone(), down.clone());
        let thread = std::thread::spawn(move || {
            let mut up_shaper = Shaper::new(shape, 0xA5);
            let mut down_shaper = Shaper::new(shape, 0x5A);
            let mut client: Option<SocketAddr> = None;
            let mut buf = [0u8; 2048];
            while !stop2.load(Ordering::Relaxed) {
                let mut fds = vec![
                    util::pollfd(std::os::fd::AsRawFd::as_raw_fd(&sock_c), libc::POLLIN),
                    util::pollfd(std::os::fd::AsRawFd::as_raw_fd(&sock_s), libc::POLLIN),
                ];
                let _ = util::poll(&mut fds, 1);
                let now = util::now_ms() as f64;
                while let Ok((n, from)) = sock_c.recv_from(&mut buf) {
                    client = Some(from);
                    up_shaper.admit(now, buf[..n].to_vec(), &up2);
                }
                while let Ok((n, _)) = sock_s.recv_from(&mut buf) {
                    down_shaper.admit(now, buf[..n].to_vec(), &down2);
                }
                let now = util::now_ms() as f64;
                up_shaper.release(now, |d| {
                    up2.forwarded_dgrams.fetch_add(1, Ordering::Relaxed);
                    up2.forwarded_bytes.fetch_add(d.len() as u64, Ordering::Relaxed);
                    let _ = sock_s.send_to(d, server_addr);
                });
                if let Some(client) = client {
                    down_shaper.release(now, |d| {
                        down2.forwarded_dgrams.fetch_add(1, Ordering::Relaxed);
                        down2.forwarded_bytes.fetch_add(d.len() as u64, Ordering::Relaxed);
                        let _ = sock_c.send_to(d, client);
                    });
                }
            }
        });
        LossyLink { client_addr, stop, up, down, thread: Some(thread) }
    }

    fn client_addr(&self) -> SocketAddr {
        self.client_addr
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for LossyLink {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// The scenario driver.

/// Everything one loaded run measured. Durations in ms, rates in bytes/s.
#[derive(Debug)]
pub struct LoadReport {
    pub secs: u64,
    pub sessions: usize,
    pub agent_channels: usize,
    // Client (sender under test).
    pub frames_sent: u64,
    pub agent_payload_bytes_offered: u64,
    pub drains: u64,
    pub burst_high_water: usize,
    pub srtt_samples: Vec<f64>,
    pub rto_samples: Vec<u64>,
    pub wire_tx: u64,
    pub wire_rx: u64,
    // Remote (receiver).
    pub frames_delivered: u64,
    pub agent_bytes_unique: u64,
    pub latency_ms: Vec<u64>,
    pub worst_frame_gap_ms: u64,
    // Relay.
    pub up_forwarded: (u64, u64),
    pub up_dropped_loss: u64,
    pub up_dropped_queue: u64,
    pub up_queue_high_water: u64,
    pub down_forwarded: (u64, u64),
    pub down_dropped_loss: u64,
}

impl LoadReport {
    fn pct(sorted: &[u64], p: f64) -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        sorted[((sorted.len() - 1) as f64 * p) as usize]
    }

    /// One aligned block per run, `--nocapture`'s consumption format. The
    /// RFC 0011 §9.2/§9.3 resolutions quote these rows verbatim.
    pub fn print(&self, label: &str) {
        let mut lat = self.latency_ms.clone();
        lat.sort_unstable();
        let mut srtt = self.srtt_samples.clone();
        srtt.sort_by(|a, b| a.total_cmp(b));
        let mut rto = self.rto_samples.clone();
        rto.sort_unstable();
        let retx = self.agent_payload_bytes_offered as f64 / self.agent_bytes_unique.max(1) as f64;
        eprintln!("[load] === {label} ({}s, {} sessions, {} agent ch) ===", self.secs, self.sessions, self.agent_channels);
        eprintln!(
            "[load] agent: offered={}KB unique={}KB retx_ratio={retx:.3} goodput={}KB/s",
            self.agent_payload_bytes_offered / 1024,
            self.agent_bytes_unique / 1024,
            self.agent_bytes_unique / 1024 / self.secs.max(1),
        );
        eprintln!(
            "[load] frames: sent={} delivered={} ({:.1}%) latency p50={}ms p95={}ms max={}ms worst_gap={}ms",
            self.frames_sent,
            self.frames_delivered,
            self.frames_delivered as f64 * 100.0 / self.frames_sent.max(1) as f64,
            Self::pct(&lat, 0.50),
            Self::pct(&lat, 0.95),
            lat.last().copied().unwrap_or(0),
            self.worst_frame_gap_ms,
        );
        eprintln!(
            "[load] pacing: srtt min={:.0} med={:.0} max={:.0}ms  rto min={} med={} max={}ms  drains={} burst_max={}B",
            srtt.first().copied().unwrap_or(0.0),
            srtt.get(srtt.len() / 2).copied().unwrap_or(0.0),
            srtt.last().copied().unwrap_or(0.0),
            rto.first().copied().unwrap_or(0),
            rto.get(rto.len() / 2).copied().unwrap_or(0),
            rto.last().copied().unwrap_or(0),
            self.drains,
            self.burst_high_water,
        );
        eprintln!(
            "[load] relay: up fwd={}dg/{}KB loss_drop={} queue_drop={} qmax={}B | down fwd={}dg/{}KB loss_drop={} | wire tx={}KB rx={}KB",
            self.up_forwarded.0,
            self.up_forwarded.1 / 1024,
            self.up_dropped_loss,
            self.up_dropped_queue,
            self.up_queue_high_water,
            self.down_forwarded.0,
            self.down_forwarded.1 / 1024,
            self.down_dropped_loss,
            self.wire_tx / 1024,
            self.wire_rx / 1024,
        );
    }
}

/// The remote thread's collected half of the report.
struct RemoteStats {
    frames_delivered: u64,
    agent_bytes_unique: u64,
    latency_ms: Vec<u64>,
    worst_frame_gap_ms: u64,
}

const SESSION_FRAME_HEADER: usize = 18; // u16 idx + u64 seq + u64 sent_at

fn session_frame(idx: u16, seq: u64, size: usize) -> Vec<u8> {
    let mut p = vec![0u8; size.max(SESSION_FRAME_HEADER)];
    p[0..2].copy_from_slice(&idx.to_le_bytes());
    p[2..10].copy_from_slice(&seq.to_le_bytes());
    p[10..18].copy_from_slice(&util::now_ms().to_le_bytes());
    p
}

/// Runs one loaded-connection scenario: `sessions` synthetic frame producers
/// (2 KiB frames, `send_interval()`-paced, latest-state-wins) plus
/// `agent_channels` remote consumers each demanding continuous `bulk_bytes`
/// replies from a canned local agent, for `secs` wall seconds through
/// `shape`. Ports: `port_range` must be unique per scenario
/// (`--test-threads=1` keeps runs serial; uniqueness keeps a wedged run from
/// poisoning the next).
pub fn run_loaded_mux(
    shape: LinkShape,
    sessions: usize,
    agent_channels: usize,
    bulk_bytes: usize,
    secs: u64,
    port_range: (u16, u16),
) -> LoadReport {
    const FRAME_SIZE: usize = 2048 - 512; // one datagram after envelope+seal
    const REQUEST_SIZE: usize = 8;

    let key = crate::remote::crypto::Key::random();
    let (server_conn, port) = Connection::server(port_range, &key, Family::Inet).unwrap();
    let server_addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();
    let mut relay = LossyLink::start(shape, server_addr);
    let client_conn = Connection::client(relay.client_addr(), &key).unwrap();

    let stop = Arc::new(AtomicBool::new(false));

    // --- Remote thread: agent_only_loop's skeleton + consumers + a 1 s
    // heartbeat so the client's RTT estimator keeps sampling.
    let stop_r = stop.clone();
    let remote = std::thread::spawn(move || {
        let mut conn = server_conn;
        let mut fragmenter = sync::Fragmenter::new();
        let mut assembly = sync::FragmentAssembly::new();
        let mut mux = AgentChannelMux::new_server();
        // Consumer state per rec id (1-based): requests sent, reply bytes seen.
        let mut consumers: Vec<(u64, u64)> = vec![(0, 0); agent_channels];
        let mut opened = false;
        let mut stats = RemoteStats {
            frames_delivered: 0,
            agent_bytes_unique: 0,
            latency_ms: Vec::new(),
            worst_frame_gap_ms: 0,
        };
        let mut last_frame_at: Option<u64> = None;
        let mut last_beat = 0u64;

        loop {
            let now = util::now_ms();
            if stop_r.load(Ordering::Relaxed) {
                break;
            }
            let mut deadline = last_beat + 1000;
            if let Some(d) = mux.next_deadline(conn.rto()) {
                deadline = deadline.min(d.max(now));
            }
            let mut fds = vec![util::pollfd(conn.raw_fd(), libc::POLLIN)];
            let _ = util::poll(&mut fds, deadline.saturating_sub(now).min(50) as i32);

            if fds[0].revents & libc::POLLIN != 0 {
                loop {
                    match conn.recv() {
                        Ok(Some(payload)) => {
                            let Ok(frag) = sync::Fragment::from_bytes(&payload) else { continue };
                            let Some(assembled) = assembly.add(frag) else { continue };
                            let Some((chan, message)) = channel::open_any_instruction(true, &assembled)
                            else {
                                continue;
                            };
                            if chan.kind() == KIND_AGENT {
                                for rec in mux.on_instruction(chan, message) {
                                    if rec.kind == RecordKind::Data {
                                        let idx = rec.channel as usize - 1;
                                        if let Some(c) = consumers.get_mut(idx) {
                                            c.1 += rec.payload.len() as u64;
                                        }
                                        stats.agent_bytes_unique += rec.payload.len() as u64;
                                    }
                                }
                            } else if message.len() >= SESSION_FRAME_HEADER {
                                let sent = u64::from_le_bytes(message[10..18].try_into().unwrap());
                                let now = util::now_ms();
                                stats.frames_delivered += 1;
                                stats.latency_ms.push(now.saturating_sub(sent));
                                if let Some(prev) = last_frame_at {
                                    stats.worst_frame_gap_ms =
                                        stats.worst_frame_gap_ms.max(now.saturating_sub(prev));
                                }
                                last_frame_at = Some(now);
                            }
                        }
                        Ok(None) => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
            }

            // Consumers: open once the peer is known, then keep one request
            // outstanding per channel (a fresh request the moment the
            // previous bulk reply completes — continuous demand).
            if conn.has_remote() && !opened {
                opened = true;
                let mut recs = Vec::new();
                for i in 0..agent_channels {
                    recs.push(AgentRecord {
                        channel: (i + 1) as u32,
                        kind: RecordKind::Open,
                        payload: Vec::new(),
                    });
                }
                mux.queue_records(&recs);
            }
            if opened && !stop_r.load(Ordering::Relaxed) {
                let mut recs = Vec::new();
                for (i, c) in consumers.iter_mut().enumerate() {
                    if c.1 >= c.0 * bulk_bytes as u64 {
                        c.0 += 1;
                        recs.push(AgentRecord {
                            channel: (i + 1) as u32,
                            kind: RecordKind::Data,
                            payload: vec![0x5A; REQUEST_SIZE],
                        });
                    }
                }
                if !recs.is_empty() {
                    mux.queue_records(&recs);
                }
            }

            // Sends: heartbeat (the remote's only session traffic) + agent.
            let now = util::now_ms();
            let beat = (now.saturating_sub(last_beat) >= 1000).then(|| {
                last_beat = now;
                b"beat".to_vec()
            });
            for (chan, payload) in
                crate::remote::agent::iteration_sends(beat, Some(&mut mux), now, conn.rto())
            {
                crate::remote::server::send_on_channel(&mut conn, &mut fragmenter, chan, &payload, true);
            }
        }
        stats
    });

    // --- Client thread work, run inline: mux_loop's skeleton + N session
    // producers + the canned bulk agent.
    let mut conn = client_conn;
    let mut fragmenter = sync::Fragmenter::new();
    let mut assembly = sync::FragmentAssembly::new();
    let mut mux = AgentChannelMux::new_client();
    // Canned agent: per rec id, request bytes seen (every REQUEST_SIZE
    // triggers one bulk reply).
    let mut agent_seen: Vec<(u32, u64, u64)> = Vec::new(); // (rec_id, req_bytes, replies)
    let mut frames_sent = 0u64;
    let mut seqs = vec![0u64; sessions];
    let mut next_frame_at = vec![0u64; sessions];
    let mut offered = 0u64;
    let mut drains = 0u64;
    let mut burst_high_water = 0usize;
    let mut srtt_samples = Vec::new();
    let mut rto_samples = Vec::new();
    let mut last_sample = 0u64;
    // The client heartbeat (mux_loop's role): with zero sessions nothing
    // else ever transmits, and the server only learns its peer address from
    // the first authentic client datagram (roaming adoption) — without this
    // an agent-only run deadlocks silently on both sides. `None` = never
    // sent, so the first beat goes immediately (the mux_loop trap note).
    let mut last_beat: Option<u64> = None;

    let t0 = util::now_ms();
    let deadline_ms = t0 + secs * 1000;
    while util::now_ms() < deadline_ms {
        let now = util::now_ms();
        let mut deadline = next_frame_at.iter().copied().min().unwrap_or(now + 50);
        if let Some(d) = mux.next_deadline(conn.rto()) {
            deadline = deadline.min(d.max(now));
        }
        let mut fds = vec![util::pollfd(conn.raw_fd(), libc::POLLIN)];
        let _ = util::poll(&mut fds, deadline.saturating_sub(now).clamp(0, 50) as i32);

        if fds[0].revents & libc::POLLIN != 0 {
            loop {
                match conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else { continue };
                        let Some(assembled) = assembly.add(frag) else { continue };
                        let Some((chan, message)) = channel::open_any_instruction(true, &assembled)
                        else {
                            continue;
                        };
                        if chan.kind() != KIND_AGENT {
                            continue; // remote heartbeats: RTT rides the datagram layer
                        }
                        let mut replies = Vec::new();
                        for rec in mux.on_instruction(chan, message) {
                            match rec.kind {
                                RecordKind::Open => agent_seen.push((rec.channel, 0, 0)),
                                RecordKind::Data => {
                                    if let Some(c) =
                                        agent_seen.iter_mut().find(|c| c.0 == rec.channel)
                                    {
                                        c.1 += rec.payload.len() as u64;
                                        while c.1 / REQUEST_SIZE as u64 > c.2 {
                                            c.2 += 1;
                                            replies.push(AgentRecord {
                                                channel: rec.channel,
                                                kind: RecordKind::Data,
                                                payload: vec![0xAB; bulk_bytes],
                                            });
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !replies.is_empty() {
                            mux.queue_records(&replies);
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // Producers: one frame per session per send_interval (latest state
        // wins — a skipped tick is a coalesced frame, mosh-style).
        let now = util::now_ms();
        let mut session_payloads = Vec::new();
        for (i, due) in next_frame_at.iter_mut().enumerate() {
            if now >= *due {
                seqs[i] += 1;
                session_payloads.push(session_frame(i as u16, seqs[i], FRAME_SIZE));
                *due = now + conn.send_interval();
                frames_sent += 1;
            }
        }
        // Heartbeat: peer discovery + RTT sampling when no frame is due
        // (the sole client traffic in an agent-only run). Too short for
        // SESSION_FRAME_HEADER, so the remote never counts it as a frame.
        if session_payloads.is_empty()
            && last_beat.is_none_or(|t| now.saturating_sub(t) >= 1000)
        {
            last_beat = Some(now);
            session_payloads.push(b"beat".to_vec());
        }

        // The §4.1 ordered drain: session instructions first, then the agent
        // mux's due instructions (iteration_sends generalized to N session
        // payloads — the discipline, not the single-payload signature).
        let agent_out = mux.outgoing(now, conn.rto());
        let mut drain_bytes = 0usize;
        let had_sends = !session_payloads.is_empty() || !agent_out.is_empty();
        for (chan, payload) in session_payloads
            .into_iter()
            .map(|p| (SESSION_CHANNEL, p))
            .chain(agent_out)
        {
            if chan.kind() == KIND_AGENT {
                if let Ok(p) = AgentPayload::decode(&payload) {
                    offered += p.data.len() as u64;
                }
            }
            drain_bytes += payload.len();
            crate::remote::server::send_on_channel(&mut conn, &mut fragmenter, chan, &payload, true);
        }
        if had_sends {
            drains += 1;
            burst_high_water = burst_high_water.max(drain_bytes);
        }

        if now.saturating_sub(last_sample) >= 1000 {
            last_sample = now;
            srtt_samples.push(conn.srtt());
            rto_samples.push(conn.rto());
        }
    }

    stop.store(true, Ordering::Relaxed);
    let remote_stats = remote.join().unwrap();
    relay.stop();

    LoadReport {
        secs,
        sessions,
        agent_channels,
        frames_sent,
        agent_payload_bytes_offered: offered,
        drains,
        burst_high_water,
        srtt_samples,
        rto_samples,
        wire_tx: conn.bytes_tx(),
        wire_rx: conn.bytes_rx(),
        frames_delivered: remote_stats.frames_delivered,
        agent_bytes_unique: remote_stats.agent_bytes_unique,
        latency_ms: remote_stats.latency_ms,
        worst_frame_gap_ms: remote_stats.worst_frame_gap_ms,
        up_forwarded: (
            relay.up.forwarded_dgrams.load(Ordering::Relaxed),
            relay.up.forwarded_bytes.load(Ordering::Relaxed),
        ),
        up_dropped_loss: relay.up.dropped_loss.load(Ordering::Relaxed),
        up_dropped_queue: relay.up.dropped_queue.load(Ordering::Relaxed),
        up_queue_high_water: relay.up.queue_high_water.load(Ordering::Relaxed),
        down_forwarded: (
            relay.down.forwarded_dgrams.load(Ordering::Relaxed),
            relay.down.forwarded_bytes.load(Ordering::Relaxed),
        ),
        down_dropped_loss: relay.down.dropped_loss.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// The scenario matrix. Each test answers one §9.2/§9.3 question by PRINTING
// its LoadReport; assertions are harness-health floors only (did traffic
// flow, did the run measure anything) — the decision reads the numbers, the
// test only proves the experiment ran. Port ranges are unique per scenario
// so a wedged run cannot poison the next (63500+; the mux/agent suites own
// 63400-63499).

const BULK: usize = 256 * 1024; // one OpenSSH-max agent reply per request

/// Hermetic-gate smoke (NOT ignored): 1 s on an unimpaired loopback link,
/// 1 session + 1 agent channel, small bulk. Proves the relay pins both
/// peers, frames arrive, and agent bytes round-trip — so a refactor that
/// breaks the harness fails in CI, not on the next measurement day.
#[test]
fn load_smoke_lan_roundtrip() {
    let r = run_loaded_mux(LinkShape::lan(), 1, 1, 8 * 1024, 1, (63610, 63619));
    assert!(r.frames_sent > 0, "producer never produced");
    assert!(r.frames_delivered > 0, "no session frame crossed the relay");
    assert!(r.agent_bytes_unique > 0, "no agent bytes crossed the relay");
    // The agent-only shape: with zero sessions the client heartbeat is the
    // only session traffic, and it alone must bootstrap peer discovery
    // (the deadlock the first measurement run hit — every 0-session
    // scenario sat at zero wire bytes because nothing ever transmitted).
    let r = run_loaded_mux(LinkShape::lan(), 0, 1, 8 * 1024, 1, (63620, 63629));
    assert!(
        r.agent_bytes_unique > 0,
        "agent-only run deadlocked: heartbeat never bootstrapped the peer"
    );
}

/// Harness self-proof on a clean 50 ms link: the ack-clocked ceiling.
/// Analysis: each channel's unacked window is one §4.1 instruction
/// (32 KiB from the cumulative base) per RTT (~100 ms), so 8 channels ≈
/// 2.5 MB/s. A harness or bench-loop bug (stalled acks, missing send_due
/// pumping) lands far below; the floor asserts a conservative half of it.
#[test]
#[ignore = "load probe; run via `just debug-mux-load` (--ignored --nocapture)"]
fn load_baseline_clean_wan_agent_bulk_saturates() {
    let r = run_loaded_mux(LinkShape::wan_clean(), 0, 8, BULK, 10, (63500, 63509));
    r.print("baseline: clean 50ms WAN, 8 agent channels, no sessions");
    assert!(r.agent_bytes_unique > 0, "no agent bytes crossed the relay");
    let goodput = r.agent_bytes_unique / r.secs.max(1);
    assert!(
        goodput > 1_250_000 / 2,
        "clean-link goodput {goodput} B/s is under half the analytic ack-clock ceiling — \
         the harness (not the transport) is suspect"
    );
    assert!(
        r.up_dropped_queue == 0 && r.up_dropped_loss == 0,
        "clean link must drop nothing"
    );
}

/// The §9.2 decider: the ~256 KB per-drain aggregate burst against a
/// 1 Mbit/s bottleneck with a 64 KiB queue. The report shows whether the
/// RTO-paced re-offer spirals (rto pinned at max, queue drops climbing,
/// goodput collapsing) or the ack-clock self-limits.
#[test]
#[ignore = "load probe; run via `just debug-mux-load` (--ignored --nocapture)"]
fn load_constrained_link_burst_behavior() {
    let r = run_loaded_mux(LinkShape::constrained(), 0, 8, BULK, 15, (63510, 63519));
    r.print("constrained: 150ms/1%loss/1Mbit/64KiB queue, 8 agent channels");
    assert!(r.agent_bytes_unique > 0, "no agent bytes crossed the relay");
    // The §9.2 response's regression ceiling, generous per this file's
    // floors-not-golden posture: pre-response this measured 10x (the
    // recorded collapse), post-response 3.75x. A return above 6x means the
    // backoff/AIMD sender stopped bounding its re-offers.
    let retx = r.agent_payload_bytes_offered as f64 / r.agent_bytes_unique.max(1) as f64;
    assert!(
        retx < 6.0,
        "constrained-link retransmit ratio {retx:.2} regressed past the §9.2 ceiling"
    );
}

/// The §9.3 decider: does the §4.1 session-first drain ordering survive a
/// shared bottleneck queue, or does agent bulk's queue occupancy starve
/// frame latency? Control run (sessions only) first, then rising session
/// counts under full agent bulk.
#[test]
#[ignore = "load probe; run via `just debug-mux-load` (--ignored --nocapture)"]
fn load_session_frames_vs_agent_bulk_starvation() {
    let control = run_loaded_mux(LinkShape::constrained(), 4, 0, BULK, 15, (63520, 63529));
    control.print("starvation control: 4 sessions, NO agent bulk");
    assert!(control.frames_delivered > 0, "control frames never arrived");
    for (i, sessions) in [1usize, 4, 8].into_iter().enumerate() {
        let base = 63530 + (i as u16) * 10;
        let r = run_loaded_mux(
            LinkShape::constrained(),
            sessions,
            8,
            BULK,
            15,
            (base, base + 9),
        );
        r.print(&format!("starvation: {sessions} sessions + 8 agent channels"));
        assert!(r.frames_delivered > 0, "frames starved to zero at {sessions} sessions");
    }
}

/// §9.2's loss-response probe: identical load at 0%, 2%, and 5% random
/// loss on an unlimited link. With no backoff, the re-offer rate is fixed
/// — the retransmit ratio across the three runs shows whether offered
/// load responds to loss at all.
#[test]
#[ignore = "load probe; run via `just debug-mux-load` (--ignored --nocapture)"]
fn load_loss_step_response() {
    for (i, loss) in [0.0f64, 2.0, 5.0].into_iter().enumerate() {
        let shape = LinkShape { loss_pct: loss, seed: 7 + i as u64, ..LinkShape::wan_clean() };
        let base = 63560 + (i as u16) * 10;
        let r = run_loaded_mux(shape, 0, 8, BULK, 10, (base, base + 9));
        r.print(&format!("loss response: {loss}% loss, clean 50ms WAN"));
        assert!(r.agent_bytes_unique > 0, "no agent bytes at {loss}% loss");
    }
}

/// §9.2/§9.3 coupling through the shared estimator: a big queue (512 KiB)
/// on the same 1 Mbit/s bottleneck — no loss, so nothing bounds queue
/// occupancy but the ack clock. The srtt series shows how much standing
/// queue agent bulk builds (bufferbloat), which also inflates
/// `send_interval` and so slows every session's frame cadence.
#[test]
#[ignore = "load probe; run via `just debug-mux-load` (--ignored --nocapture)"]
fn load_bufferbloat_srtt_inflation() {
    let quiet = run_loaded_mux(
        LinkShape { seed: 11, ..LinkShape::bufferbloat() },
        2,
        0,
        BULK,
        10,
        (63590, 63599),
    );
    quiet.print("bufferbloat control: 2 sessions, no bulk, 1Mbit/512KiB queue");
    let loaded = run_loaded_mux(
        LinkShape { seed: 12, ..LinkShape::bufferbloat() },
        2,
        8,
        BULK,
        10,
        (63600, 63609),
    );
    loaded.print("bufferbloat: 2 sessions + 8 agent channels, 1Mbit/512KiB queue");
    assert!(loaded.frames_delivered > 0 && quiet.frames_delivered > 0);
    assert!(!loaded.srtt_samples.is_empty(), "no srtt samples collected");
}
