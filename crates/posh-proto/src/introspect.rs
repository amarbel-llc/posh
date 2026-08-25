//! RFC 0014: client introspection — the one struct every reporting surface
//! renders from (§6), its `CAP_CLIENT_STATE` codec (§2), the `CAP_CLIENT_IDENT`
//! codec (§1, the `SERVER_IDENT` layout under the client id), and the status
//! line a serving side prints per attached client (§4.2).
//!
//! The double-end visibility rule is structural here, not a review reminder:
//! [`ClientIntrospection`] is the ONLY place an axis is declared, every field
//! has a registered key in [`CLIENT_FIELDS`], and [`render_client_line`] must
//! mention every key — the `client_line_covers_every_field` test fails the
//! moment a field is added without a renderer, and the append-only payload
//! rule (§2.1) means the wire grows with the struct instead of drifting.

use crate::caps::{
    Cap, ServerIdent, CAP_CLIENT_IDENT, CAP_CLIENT_STATE, decode_server_ident, encode_server_ident,
};
use crate::error::{Error, Result};

/// The client's identity (RFC 0014 §1) is the same shape as the server's: the
/// alias keeps the two decoders one function.
pub type Ident = ServerIdent;

/// The echo prediction model in effect (RFC 0014 §2.2). Values are wire
/// ordinals and MUST NOT be reused; `None` is a client with no predictor at all
/// (today's local-attach client), distinct from "nothing reported".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EchoModel {
    #[default]
    None = 0,
    Adaptive = 1,
    Always = 2,
    Never = 3,
    Experimental = 4,
    Optimistic = 5,
    Controller = 6,
    Scratch = 7,
}

impl EchoModel {
    /// The §2.2 ordinal → model; an unknown ordinal (a newer peer's model)
    /// decodes as `None` rather than failing the whole payload, so the rest of
    /// the state stays readable.
    pub fn from_wire(v: u8) -> EchoModel {
        match v {
            1 => EchoModel::Adaptive,
            2 => EchoModel::Always,
            3 => EchoModel::Never,
            4 => EchoModel::Experimental,
            5 => EchoModel::Optimistic,
            6 => EchoModel::Controller,
            7 => EchoModel::Scratch,
            _ => EchoModel::None,
        }
    }

    /// The §4.2 rendering: the model's user-facing spelling, `none` for a
    /// predictor-less client.
    pub fn name(self) -> &'static str {
        match self {
            EchoModel::None => "none",
            EchoModel::Adaptive => "adaptive",
            EchoModel::Always => "always",
            EchoModel::Never => "never",
            EchoModel::Experimental => "experimental",
            EchoModel::Optimistic => "optimistic",
            EchoModel::Controller => "controller",
            EchoModel::Scratch => "scratch",
        }
    }
}

/// Who decides the model (RFC 0014 §2.3). `governing` and either pin are
/// mutually exclusive; `escalated` requires `governing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EchoControl {
    /// The escalation machine decides the model (default model, gate on).
    pub governing: bool,
    /// The auto switch to `optimistic` is currently applied.
    pub escalated: bool,
    /// `POSH_ECHO_ESCALATE` is off.
    pub gate_off: bool,
    /// Pinned by `POSH_PREDICTION_MODEL` / `POSH_PREDICTION`.
    pub pinned_env: bool,
    /// Pinned by a palette `Echo:` command.
    pub pinned_palette: bool,
}

impl EchoControl {
    const GOVERNING: u8 = 1 << 0;
    const ESCALATED: u8 = 1 << 1;
    const GATE_OFF: u8 = 1 << 2;
    const PINNED_ENV: u8 = 1 << 3;
    const PINNED_PALETTE: u8 = 1 << 4;
    const KNOWN: u8 = 0x1f;

    fn to_bits(self) -> u8 {
        [
            (self.governing, Self::GOVERNING),
            (self.escalated, Self::ESCALATED),
            (self.gate_off, Self::GATE_OFF),
            (self.pinned_env, Self::PINNED_ENV),
            (self.pinned_palette, Self::PINNED_PALETTE),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |acc, (_, bit)| acc | bit)
    }

    fn from_bits(b: u8) -> Result<EchoControl> {
        let c = EchoControl {
            governing: b & Self::GOVERNING != 0,
            escalated: b & Self::ESCALATED != 0,
            gate_off: b & Self::GATE_OFF != 0,
            pinned_env: b & Self::PINNED_ENV != 0,
            pinned_palette: b & Self::PINNED_PALETTE != 0,
        };
        if b & !Self::KNOWN != 0 {
            return Err(Error::from("CLIENT_STATE: reserved echo_control bit set"));
        }
        if c.governing && (c.pinned_env || c.pinned_palette) {
            return Err(Error::from("CLIENT_STATE: governing and pinned both set"));
        }
        if c.escalated && !c.governing {
            return Err(Error::from("CLIENT_STATE: escalated without governing"));
        }
        Ok(c)
    }

    /// The §4.2 `control=` value.
    pub fn name(self) -> &'static str {
        if self.gate_off {
            "gate-off"
        } else if self.pinned_env {
            "pinned-env"
        } else if self.pinned_palette {
            "pinned-palette"
        } else if self.escalated {
            "auto-escalated"
        } else {
            "auto"
        }
    }
}

/// The client's view of the FDR 0006 gates (RFC 0014 §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gates {
    /// The remote PTY's `ECHO` bit as last read from `FLAG_ECHO`.
    pub echo_on: bool,
    /// The reconstructed server terminal is on the alternate screen.
    pub alt_screen: bool,
    /// The predictor currently has showable predictions.
    pub predict_active: bool,
}

impl Gates {
    fn to_bits(self) -> u8 {
        [(self.echo_on, 1), (self.alt_screen, 2), (self.predict_active, 4)]
            .into_iter()
            .filter(|(on, _)| *on)
            .fold(0, |acc, (_, bit)| acc | bit)
    }
    fn from_bits(b: u8) -> Result<Gates> {
        if b & !0x07 != 0 {
            return Err(Error::from("CLIENT_STATE: reserved gates bit set"));
        }
        Ok(Gates {
            echo_on: b & 1 != 0,
            alt_screen: b & 2 != 0,
            predict_active: b & 4 != 0,
        })
    }
}

/// The negotiated frame-sync codec (RFC 0014 §2.1 byte 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    #[default]
    Unknown = 0,
    DumpDiff = 1,
    Morph = 2,
}

impl Codec {
    pub fn from_wire(v: u8) -> Codec {
        match v {
            1 => Codec::DumpDiff,
            2 => Codec::Morph,
            _ => Codec::Unknown,
        }
    }
    /// From the client's `framesync.label()` spelling.
    pub fn from_label(label: &str) -> Codec {
        match label {
            "dumpdiff" => Codec::DumpDiff,
            "morph" => Codec::Morph,
            _ => Codec::Unknown,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Codec::Unknown => "unknown",
            Codec::DumpDiff => "dumpdiff",
            Codec::Morph => "morph",
        }
    }
}

/// The escalation machine's thresholds as the SENDER runs them (RFC 0014
/// §2.1 bytes 13–20), so a reader never guesses which build's constants apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Thresholds {
    pub escalate_srtt_ms: u16,
    pub escalate_hold_ms: u16,
    pub deescalate_srtt_ms: u16,
    pub deescalate_hold_ms: u16,
}

/// Prediction outcome counters (RFC 0014 §2.1 bytes 21–36), cumulative for
/// the life of the current predictor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcomes {
    pub correct: u32,
    pub nocredit: u32,
    pub incorrect: u32,
    pub mispredict_resets: u32,
}

/// `srtt_ms` sentinel: no measured sample yet.
pub const SRTT_UNMEASURED: u32 = u32::MAX;

/// Everything a client reports about itself (RFC 0014 §2) — THE introspection
/// struct (§6). Add an axis here, and only here; the codec, the §4.2 line, the
/// diag dump, and the palette all follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientIntrospection {
    pub echo_model: EchoModel,
    pub control: EchoControl,
    pub gates: Gates,
    pub codec: Codec,
    /// `None` = no measured sample yet (`SRTT_UNMEASURED` on the wire).
    pub srtt_ms: Option<u32>,
    pub rto_ms: u32,
    pub thresholds: Thresholds,
    pub outcomes: Outcomes,
}

/// Format version of the [`ClientIntrospection`] payload. Unlike
/// `SERVER_IDENT`, the SAME version tolerates trailing bytes (§2.1): a later
/// build appends fields and an older reader keeps this prefix.
pub const CLIENT_STATE_FMT: u8 = 1;
/// The `0x01` prefix length every decoder requires.
pub const CLIENT_STATE_LEN: usize = 37;

/// The registered `key=` names of the §4.2 client line, one per axis. The
/// coverage test asserts every renderer mentions every key; a surface that
/// hand-lists fields cannot pass it.
pub const CLIENT_FIELDS: &[&str] = &[
    "echo", "control", "srtt", "rto", "codec", "gates", "thresholds", "predict", "resets",
];

/// Encode as the `CAP_CLIENT_STATE` entry (§2.1).
pub fn encode_client_state(s: &ClientIntrospection) -> Cap {
    let mut p = Vec::with_capacity(CLIENT_STATE_LEN);
    p.push(CLIENT_STATE_FMT);
    p.push(s.echo_model as u8);
    p.push(s.control.to_bits());
    p.push(s.gates.to_bits());
    p.push(s.codec as u8);
    p.extend_from_slice(&s.srtt_ms.unwrap_or(SRTT_UNMEASURED).to_le_bytes());
    p.extend_from_slice(&s.rto_ms.to_le_bytes());
    for v in [
        s.thresholds.escalate_srtt_ms,
        s.thresholds.escalate_hold_ms,
        s.thresholds.deescalate_srtt_ms,
        s.thresholds.deescalate_hold_ms,
    ] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    for v in [
        s.outcomes.correct,
        s.outcomes.nocredit,
        s.outcomes.incorrect,
        s.outcomes.mispredict_resets,
    ] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    debug_assert_eq!(p.len(), CLIENT_STATE_LEN);
    Cap {
        id: CAP_CLIENT_STATE,
        payload: p,
    }
}

/// Decode a `CAP_CLIENT_STATE` payload: rejects an unknown version or a short
/// payload, keeps the known prefix of a longer one (§2.1), and rejects the
/// §2.3 exclusivity violations so a reader never renders a contradiction.
pub fn decode_client_state(p: &[u8]) -> Result<ClientIntrospection> {
    if p.len() < CLIENT_STATE_LEN {
        return Err(Error::from("CLIENT_STATE payload too short"));
    }
    if p[0] != CLIENT_STATE_FMT {
        return Err(Error::from("CLIENT_STATE unknown format version"));
    }
    let u16_at = |o: usize| u16::from_le_bytes(p[o..o + 2].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
    let srtt = u32_at(5);
    Ok(ClientIntrospection {
        echo_model: EchoModel::from_wire(p[1]),
        control: EchoControl::from_bits(p[2])?,
        gates: Gates::from_bits(p[3])?,
        codec: Codec::from_wire(p[4]),
        srtt_ms: (srtt != SRTT_UNMEASURED).then_some(srtt),
        rto_ms: u32_at(9),
        thresholds: Thresholds {
            escalate_srtt_ms: u16_at(13),
            escalate_hold_ms: u16_at(15),
            deescalate_srtt_ms: u16_at(17),
            deescalate_hold_ms: u16_at(19),
        },
        outcomes: Outcomes {
            correct: u32_at(21),
            nocredit: u32_at(25),
            incorrect: u32_at(29),
            mispredict_resets: u32_at(33),
        },
    })
}

/// Encode an identity under the CLIENT id (§1.1: the `SERVER_IDENT` layout).
pub fn encode_client_ident(ident: &Ident) -> Cap {
    Cap {
        id: CAP_CLIENT_IDENT,
        ..encode_server_ident(ident)
    }
}

/// Decode a `CAP_CLIENT_IDENT` payload (same rules as `SERVER_IDENT`).
pub fn decode_client_ident(payload: &[u8]) -> Result<Ident> {
    decode_server_ident(payload)
}

/// What a serving side retains per attached client (§3) and what the client
/// itself holds about itself: identity, latest state, and how old it is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientRecord {
    pub ident: Option<Ident>,
    pub state: Option<ClientIntrospection>,
    /// ms since `state` was decoded (or built, on the client itself).
    pub age_ms: Option<u64>,
    /// The relay's pid when this record was forwarded (§3), else `None`.
    pub via_relay_pid: Option<u32>,
}

/// Render the §4.2 client line. Every key in [`CLIENT_FIELDS`] appears
/// whenever `state` is present; `echo=unknown` marks a client that reported no
/// state (an old client), distinct from `echo=none` (reported: no predictor).
pub fn render_client_line(r: &ClientRecord) -> String {
    let mut out = String::from("client");
    match &r.ident {
        Some(id) => out.push_str(&format!(" pid={} build={}({})", id.pid, id.version, id.git_sha)),
        None => out.push_str(" build=unknown"),
    }
    if let Some(pid) = r.via_relay_pid {
        out.push_str(&format!(" via=relay pid={pid}"));
    }
    let Some(s) = &r.state else {
        out.push_str(" echo=unknown");
        return out;
    };
    let srtt = match s.srtt_ms {
        Some(ms) => ms.to_string(),
        None => "none".to_string(),
    };
    out.push_str(&format!(
        " echo={} control={} srtt={srtt} rto={} codec={} gates=echo:{},alt:{},active:{} \
         thresholds={}/{}/{}/{} predict={}/{}/{} resets={}",
        s.echo_model.name(),
        s.control.name(),
        s.rto_ms,
        s.codec.name(),
        s.gates.echo_on as u8,
        s.gates.alt_screen as u8,
        s.gates.predict_active as u8,
        s.thresholds.escalate_srtt_ms,
        s.thresholds.escalate_hold_ms,
        s.thresholds.deescalate_srtt_ms,
        s.thresholds.deescalate_hold_ms,
        s.outcomes.correct,
        s.outcomes.nocredit,
        s.outcomes.incorrect,
        s.outcomes.mispredict_resets,
    ));
    if let Some(age) = r.age_ms {
        out.push_str(&format!(" age={age}"));
    }
    out
}

/// A fully populated value with every field non-default — the §6 coverage
/// fixture, shared with the posh crate's renderer tests so every surface is
/// checked against the same instance.
pub fn coverage_fixture() -> ClientIntrospection {
    ClientIntrospection {
        echo_model: EchoModel::Optimistic,
        control: EchoControl {
            governing: true,
            escalated: true,
            ..EchoControl::default()
        },
        gates: Gates {
            echo_on: true,
            alt_screen: true,
            predict_active: true,
        },
        codec: Codec::Morph,
        srtt_ms: Some(412),
        rto_ms: 900,
        thresholds: Thresholds {
            escalate_srtt_ms: 150,
            escalate_hold_ms: 3000,
            deescalate_srtt_ms: 80,
            deescalate_hold_ms: 15000,
        },
        outcomes: Outcomes {
            correct: 11,
            nocredit: 22,
            incorrect: 33,
            mispredict_resets: 44,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{CAP_CLIENT_UPSTREAM, decode_table, encode_table, find};

    #[test]
    fn client_state_roundtrips_every_field() {
        let s = coverage_fixture();
        let cap = encode_client_state(&s);
        assert_eq!(cap.id, CAP_CLIENT_STATE);
        assert_eq!(cap.payload.len(), CLIENT_STATE_LEN);
        assert_eq!(decode_client_state(&cap.payload).unwrap(), s);
        // A predictor-less client (RFC 0014 §2.5) roundtrips too, with the
        // unmeasured-SRTT sentinel.
        let none = ClientIntrospection::default();
        assert_eq!(decode_client_state(&encode_client_state(&none).payload).unwrap(), none);
        assert_eq!(&encode_client_state(&none).payload[5..9], &SRTT_UNMEASURED.to_le_bytes());
    }

    #[test]
    fn client_state_keeps_prefix_of_longer_and_rejects_short_or_unknown_version() {
        let cap = encode_client_state(&coverage_fixture());
        // §2.1 append rule: trailing bytes are a later build's fields.
        let mut longer = cap.payload.clone();
        longer.extend_from_slice(&[9, 9, 9]);
        assert_eq!(decode_client_state(&longer).unwrap(), coverage_fixture());
        for cut in 0..CLIENT_STATE_LEN {
            assert!(decode_client_state(&cap.payload[..cut]).is_err(), "cut={cut} decoded");
        }
        let mut future = cap.payload.clone();
        future[0] = 2;
        assert!(decode_client_state(&future).is_err());
    }

    #[test]
    fn client_state_rejects_control_contradictions_and_reserved_bits() {
        let base = encode_client_state(&ClientIntrospection::default()).payload;
        let with = |ctl: u8, gates: u8| {
            let mut p = base.clone();
            p[2] = ctl;
            p[3] = gates;
            decode_client_state(&p)
        };
        // governing + pinned_env
        assert!(with(0x01 | 0x08, 0).is_err());
        // escalated without governing
        assert!(with(0x02, 0).is_err());
        // reserved bits
        assert!(with(0x20, 0).is_err());
        assert!(with(0, 0x08).is_err());
        // a plain pin decodes and renders as such
        assert_eq!(with(0x10, 0).unwrap().control.name(), "pinned-palette");
        assert_eq!(with(0x04 | 0x08, 0).unwrap().control.name(), "gate-off");
    }

    #[test]
    fn unknown_echo_model_ordinal_degrades_to_none_not_error() {
        let mut p = encode_client_state(&coverage_fixture()).payload;
        p[1] = 200;
        assert_eq!(decode_client_state(&p).unwrap().echo_model, EchoModel::None);
    }

    #[test]
    fn client_ident_rides_id_16_with_the_server_layout() {
        let id = Ident {
            version: "0.3.2".into(),
            git_sha: "e76fb8f".into(),
            pid: 4242,
            start_unix_ms: 1_755_000_000_000,
        };
        let cap = encode_client_ident(&id);
        assert_eq!(cap.id, CAP_CLIENT_IDENT);
        assert_eq!(cap.payload, encode_server_ident(&id).payload);
        assert_eq!(decode_client_ident(&cap.payload).unwrap(), id);
    }

    #[test]
    fn client_line_covers_every_field() {
        // RFC 0014 §6: the coverage test. Every registered key appears in the
        // rendered line for a fully populated record.
        let r = ClientRecord {
            ident: Some(Ident {
                version: "9.9.9".into(),
                git_sha: "cafef00".into(),
                pid: 7,
                start_unix_ms: 1,
            }),
            state: Some(coverage_fixture()),
            age_ms: Some(1234),
            via_relay_pid: Some(8),
        };
        let line = render_client_line(&r);
        for key in CLIENT_FIELDS {
            assert!(line.contains(&format!(" {key}=")), "missing {key}= in {line:?}");
        }
        assert_eq!(
            line,
            "client pid=7 build=9.9.9(cafef00) via=relay pid=8 echo=optimistic \
             control=auto-escalated srtt=412 rto=900 codec=morph gates=echo:1,alt:1,active:1 \
             thresholds=150/3000/80/15000 predict=11/22/33 resets=44 age=1234"
        );
        // The two absence verdicts stay distinguishable (§4.2).
        let old = ClientRecord::default();
        assert_eq!(render_client_line(&old), "client build=unknown echo=unknown");
        let none = ClientRecord {
            state: Some(ClientIntrospection::default()),
            ..ClientRecord::default()
        };
        assert!(render_client_line(&none).contains(" echo=none control=auto srtt=none "));
    }

    #[test]
    fn client_caps_survive_a_table_roundtrip_and_sit_in_the_released_band() {
        assert_eq!(CAP_CLIENT_IDENT, 16);
        assert_eq!(CAP_CLIENT_STATE, 17);
        assert_eq!(CAP_CLIENT_UPSTREAM, 18);
        let table = vec![
            encode_client_ident(&Ident {
                version: "1".into(),
                git_sha: "2".into(),
                pid: 3,
                start_unix_ms: 4,
            }),
            encode_client_state(&coverage_fixture()),
        ];
        let (decoded, used) = decode_table(&encode_table(&table)).unwrap();
        assert_eq!(used, encode_table(&table).len());
        let st = find(&decoded, CAP_CLIENT_STATE).unwrap();
        assert_eq!(decode_client_state(&st.payload).unwrap(), coverage_fixture());
    }
}
