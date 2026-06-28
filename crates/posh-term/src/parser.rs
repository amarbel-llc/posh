//! VT500-series escape sequence parser (Paul Williams DEC parser states)
//! with incremental UTF-8 decoding.
//!
//! Strings (OSC/DCS/APC) treat bytes 0x80-0xFF as content so that UTF-8
//! payloads survive; they terminate on BEL (OSC only) or ESC (the following
//! `\` then dispatches as a no-op ST). C1 controls are recognized as raw
//! 8-bit bytes outside strings and as decoded U+0080..U+009F codepoints.

const MAX_PARAMS: usize = 32;
const MAX_SUBPARAMS: usize = 16;
const MAX_INTERMEDIATES: usize = 2;
const MAX_OSC: usize = 4 * 1024 * 1024;
const MAX_DCS: usize = 64 * 1024;
// Large enough for a sizeable single-escape kitty graphics transmission.
const MAX_APC: usize = 96 * 1024 * 1024;

/// A parsed terminal action ready for dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Print(char),
    /// C0 or C1 control.
    Execute(u8),
    Csi {
        /// Each parameter is a list of colon-separated subparameters.
        params: Vec<Vec<u16>>,
        intermediates: Vec<u8>,
        /// Private marker byte (`?`, `<`, `=`, `>`) or 0.
        private: u8,
        final_byte: u8,
    },
    Esc {
        intermediates: Vec<u8>,
        final_byte: u8,
    },
    Osc {
        data: Vec<u8>,
        /// Terminated by BEL rather than ST (replies should mirror this).
        bel: bool,
    },
    Dcs {
        params: Vec<Vec<u16>>,
        intermediates: Vec<u8>,
        final_byte: u8,
        data: Vec<u8>,
    },
    Apc {
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsPassthrough,
    DcsIgnore,
    SosPmString,
    ApcString,
}

#[derive(Debug, Default)]
pub struct Parser {
    state: State,
    // UTF-8 accumulator (ground state only).
    utf8: [u8; 4],
    utf8_len: u8,
    utf8_need: u8,
    // CSI/DCS parameter accumulation.
    params: Vec<Vec<u16>>,
    cur_parts: Vec<u16>,
    cur_val: u32,
    part_seen: bool,
    intermediates: Vec<u8>,
    private: u8,
    // DCS header captured at hook time.
    dcs_final: u8,
    // String buffers.
    string_buf: Vec<u8>,
    string_overflow: bool,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    /// True in the ESC and CSI accumulation states — the only states from
    /// which a screen-switch action (DECSET/DECRST 47/1047/1049, RIS) can
    /// still dispatch. String states are excluded so their (potentially
    /// huge) payloads never need to be withheld by a streaming consumer.
    pub(crate) fn mid_escape(&self) -> bool {
        matches!(
            self.state,
            State::Escape
                | State::EscapeIntermediate
                | State::CsiEntry
                | State::CsiParam
                | State::CsiIntermediate
        )
    }

    pub fn advance(&mut self, b: u8, out: &mut Vec<Action>) {
        match self.state {
            State::Ground => self.ground(b, out),
            State::Escape => self.escape(b, out),
            State::EscapeIntermediate => self.escape_intermediate(b, out),
            State::CsiEntry => self.csi_entry(b, out),
            State::CsiParam => self.csi_param(b, out),
            State::CsiIntermediate => self.csi_intermediate(b, out),
            State::CsiIgnore => self.csi_ignore(b, out),
            State::OscString => self.osc_string(b, out),
            State::DcsEntry => self.dcs_entry(b, out),
            State::DcsParam => self.dcs_param(b, out),
            State::DcsIntermediate => self.dcs_intermediate(b, out),
            State::DcsPassthrough => self.dcs_passthrough(b, out),
            State::DcsIgnore => self.dcs_ignore(b),
            State::SosPmString => self.sos_pm(b, out),
            State::ApcString => self.apc_string(b, out),
        }
    }

    fn clear(&mut self) {
        self.params.clear();
        self.cur_parts.clear();
        self.cur_val = 0;
        self.part_seen = false;
        self.intermediates.clear();
        self.private = 0;
    }

    fn start_string(&mut self, state: State) {
        self.string_buf = Vec::new();
        self.string_overflow = false;
        self.state = state;
    }

    fn utf8_reset(&mut self) {
        self.utf8_len = 0;
        self.utf8_need = 0;
    }

    // --- ground / UTF-8 ---------------------------------------------------

    fn ground(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.utf8_need > 0 {
            if (0x80..=0xBF).contains(&b) {
                self.utf8[self.utf8_len as usize] = b;
                self.utf8_len += 1;
                self.utf8_need -= 1;
                if self.utf8_need == 0 {
                    let len = self.utf8_len as usize;
                    match std::str::from_utf8(&self.utf8[..len]) {
                        Ok(s) => out.push(Action::Print(s.chars().next().unwrap())),
                        Err(_) => out.push(Action::Print('\u{fffd}')),
                    }
                    self.utf8_reset();
                }
                return;
            }
            // Malformed: emit replacement and reprocess this byte.
            self.utf8_reset();
            out.push(Action::Print('\u{fffd}'));
            self.ground(b, out);
            return;
        }
        match b {
            0x1B => self.state = State::Escape,
            0x00..=0x1F => out.push(Action::Execute(b)),
            0x20..=0x7E => out.push(Action::Print(b as char)),
            0x7F => {} // DEL ignored
            0x80..=0x9F => self.c1(b, out),
            0xA0..=0xBF => out.push(Action::Print('\u{fffd}')),
            0xC2..=0xDF => {
                self.utf8[0] = b;
                self.utf8_len = 1;
                self.utf8_need = 1;
            }
            0xE0..=0xEF => {
                self.utf8[0] = b;
                self.utf8_len = 1;
                self.utf8_need = 2;
            }
            0xF0..=0xF4 => {
                self.utf8[0] = b;
                self.utf8_len = 1;
                self.utf8_need = 3;
            }
            // 0xC0/0xC1 (overlong) and 0xF5..=0xFF are never valid leads.
            _ => out.push(Action::Print('\u{fffd}')),
        }
    }

    fn c1(&mut self, b: u8, out: &mut Vec<Action>) {
        match b {
            0x90 => {
                self.clear();
                self.state = State::DcsEntry;
            }
            0x98 | 0x9E => self.start_string(State::SosPmString),
            0x9B => {
                self.clear();
                self.state = State::CsiEntry;
            }
            0x9C => self.state = State::Ground,
            0x9D => self.start_string(State::OscString),
            0x9F => self.start_string(State::ApcString),
            _ => out.push(Action::Execute(b)),
        }
    }

    /// Handles bytes with the "anywhere" semantics shared by all
    /// non-string, non-ground states. Returns true if consumed.
    fn anywhere(&mut self, b: u8, out: &mut Vec<Action>) -> bool {
        match b {
            0x1B => {
                self.state = State::Escape;
                true
            }
            0x18 | 0x1A => {
                out.push(Action::Execute(b));
                self.state = State::Ground;
                true
            }
            0x80..=0x9F => {
                self.state = State::Ground;
                self.c1(b, out);
                true
            }
            _ => false,
        }
    }

    // --- escape -----------------------------------------------------------

    fn escape(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => out.push(Action::Execute(b)),
            0x20..=0x2F => {
                self.clear();
                self.intermediates.push(b);
                self.state = State::EscapeIntermediate;
            }
            b'[' => {
                self.clear();
                self.state = State::CsiEntry;
            }
            b']' => self.start_string(State::OscString),
            b'P' => {
                self.clear();
                self.state = State::DcsEntry;
            }
            b'_' => self.start_string(State::ApcString),
            b'X' | b'^' => self.start_string(State::SosPmString),
            0x30..=0x7E => {
                out.push(Action::Esc {
                    intermediates: Vec::new(),
                    final_byte: b,
                });
                self.state = State::Ground;
            }
            _ => {} // 0x7F and high bytes ignored
        }
    }

    fn escape_intermediate(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => out.push(Action::Execute(b)),
            0x20..=0x2F
                if self.intermediates.len() < MAX_INTERMEDIATES => {
                    self.intermediates.push(b);
                }
            0x30..=0x7E => {
                out.push(Action::Esc {
                    intermediates: std::mem::take(&mut self.intermediates),
                    final_byte: b,
                });
                self.state = State::Ground;
            }
            _ => {}
        }
    }

    // --- CSI ----------------------------------------------------------------

    fn param_digit(&mut self, b: u8) {
        self.cur_val = (self.cur_val * 10 + u32::from(b - b'0')).min(u32::from(u16::MAX));
        self.part_seen = true;
    }

    fn param_colon(&mut self) {
        if self.cur_parts.len() < MAX_SUBPARAMS {
            self.cur_parts.push(self.cur_val as u16);
        }
        self.cur_val = 0;
        self.part_seen = true;
    }

    fn param_semicolon(&mut self) {
        self.cur_parts.push(self.cur_val as u16);
        if self.params.len() < MAX_PARAMS {
            self.params.push(std::mem::take(&mut self.cur_parts));
        } else {
            self.cur_parts.clear();
        }
        self.cur_val = 0;
        self.part_seen = true;
    }

    fn take_params(&mut self) -> Vec<Vec<u16>> {
        if self.part_seen || !self.cur_parts.is_empty() {
            self.cur_parts.push(self.cur_val as u16);
            if self.params.len() < MAX_PARAMS {
                self.params.push(std::mem::take(&mut self.cur_parts));
            }
        }
        self.cur_parts.clear();
        self.cur_val = 0;
        self.part_seen = false;
        std::mem::take(&mut self.params)
    }

    fn csi_dispatch(&mut self, b: u8, out: &mut Vec<Action>) {
        out.push(Action::Csi {
            params: self.take_params(),
            intermediates: std::mem::take(&mut self.intermediates),
            private: self.private,
            final_byte: b,
        });
        self.state = State::Ground;
    }

    fn csi_entry(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => out.push(Action::Execute(b)),
            0x20..=0x2F => {
                self.intermediates.push(b);
                self.state = State::CsiIntermediate;
            }
            b'0'..=b'9' => {
                self.param_digit(b);
                self.state = State::CsiParam;
            }
            b':' => {
                self.param_colon();
                self.state = State::CsiParam;
            }
            b';' => {
                self.param_semicolon();
                self.state = State::CsiParam;
            }
            0x3C..=0x3F => {
                self.private = b;
                self.state = State::CsiParam;
            }
            0x40..=0x7E => self.csi_dispatch(b, out),
            _ => {}
        }
    }

    fn csi_param(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => out.push(Action::Execute(b)),
            b'0'..=b'9' => self.param_digit(b),
            b':' => self.param_colon(),
            b';' => self.param_semicolon(),
            0x3C..=0x3F => self.state = State::CsiIgnore,
            0x20..=0x2F => {
                if self.intermediates.len() < MAX_INTERMEDIATES {
                    self.intermediates.push(b);
                }
                self.state = State::CsiIntermediate;
            }
            0x40..=0x7E => self.csi_dispatch(b, out),
            _ => {}
        }
    }

    fn csi_intermediate(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => out.push(Action::Execute(b)),
            0x20..=0x2F
                if self.intermediates.len() < MAX_INTERMEDIATES => {
                    self.intermediates.push(b);
                }
            0x30..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.csi_dispatch(b, out),
            _ => {}
        }
    }

    fn csi_ignore(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => out.push(Action::Execute(b)),
            0x40..=0x7E => {
                self.clear();
                self.state = State::Ground;
            }
            _ => {}
        }
    }

    // --- OSC / APC / SOS-PM -------------------------------------------------

    fn push_string_byte(&mut self, b: u8, cap: usize) {
        if self.string_buf.len() < cap {
            self.string_buf.push(b);
        } else {
            self.string_overflow = true;
        }
    }

    fn osc_string(&mut self, b: u8, out: &mut Vec<Action>) {
        match b {
            0x07 => self.finish_osc(true, out),
            0x1B => {
                self.finish_osc(false, out);
                self.state = State::Escape;
            }
            0x18 | 0x1A => self.state = State::Ground,
            0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1C..=0x1F => {}
            _ => self.push_string_byte(b, MAX_OSC),
        }
    }

    fn finish_osc(&mut self, bel: bool, out: &mut Vec<Action>) {
        let data = std::mem::take(&mut self.string_buf);
        if !self.string_overflow {
            out.push(Action::Osc { data, bel });
        }
        self.string_overflow = false;
        self.state = State::Ground;
    }

    fn apc_string(&mut self, b: u8, out: &mut Vec<Action>) {
        match b {
            0x1B => {
                let data = std::mem::take(&mut self.string_buf);
                if !self.string_overflow {
                    out.push(Action::Apc { data });
                }
                self.string_overflow = false;
                self.state = State::Escape;
            }
            0x18 | 0x1A => self.state = State::Ground,
            _ => self.push_string_byte(b, MAX_APC),
        }
    }

    fn sos_pm(&mut self, b: u8, _out: &mut Vec<Action>) {
        match b {
            0x1B => self.state = State::Escape,
            0x18 | 0x1A | 0x9C => self.state = State::Ground,
            _ => {}
        }
    }

    // --- DCS ------------------------------------------------------------------

    fn dcs_entry(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            b'0'..=b'9' => {
                self.param_digit(b);
                self.state = State::DcsParam;
            }
            b':' | b';' => {
                if b == b':' {
                    self.param_colon()
                } else {
                    self.param_semicolon()
                }
                self.state = State::DcsParam;
            }
            0x3C..=0x3F => {
                self.private = b;
                self.state = State::DcsParam;
            }
            0x20..=0x2F => {
                self.intermediates.push(b);
                self.state = State::DcsIntermediate;
            }
            0x40..=0x7E => self.dcs_hook(b),
            _ => {}
        }
    }

    fn dcs_param(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            b'0'..=b'9' => self.param_digit(b),
            b':' => self.param_colon(),
            b';' => self.param_semicolon(),
            0x3C..=0x3F => self.state = State::DcsIgnore,
            0x20..=0x2F => {
                if self.intermediates.len() < MAX_INTERMEDIATES {
                    self.intermediates.push(b);
                }
                self.state = State::DcsIntermediate;
            }
            0x40..=0x7E => self.dcs_hook(b),
            _ => {}
        }
    }

    fn dcs_intermediate(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.anywhere(b, out) {
            return;
        }
        match b {
            0x20..=0x2F
                if self.intermediates.len() < MAX_INTERMEDIATES => {
                    self.intermediates.push(b);
                }
            0x30..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.dcs_hook(b),
            _ => {}
        }
    }

    fn dcs_hook(&mut self, b: u8) {
        self.dcs_final = b;
        self.string_buf = Vec::new();
        self.string_overflow = false;
        self.state = State::DcsPassthrough;
    }

    fn dcs_passthrough(&mut self, b: u8, out: &mut Vec<Action>) {
        match b {
            0x1B => {
                self.dcs_unhook(out);
                self.state = State::Escape;
            }
            0x9C => {
                self.dcs_unhook(out);
                self.state = State::Ground;
            }
            0x18 | 0x1A => {
                self.clear();
                self.string_buf.clear();
                self.state = State::Ground;
            }
            _ => self.push_string_byte(b, MAX_DCS),
        }
    }

    fn dcs_unhook(&mut self, out: &mut Vec<Action>) {
        let data = std::mem::take(&mut self.string_buf);
        if !self.string_overflow {
            out.push(Action::Dcs {
                params: self.take_params(),
                intermediates: std::mem::take(&mut self.intermediates),
                final_byte: self.dcs_final,
                data,
            });
        }
        self.string_overflow = false;
        self.clear();
    }

    fn dcs_ignore(&mut self, b: u8) {
        match b {
            0x1B => self.state = State::Escape,
            0x18 | 0x1A | 0x9C => self.state = State::Ground,
            _ => {}
        }
    }
}

/// First subparameter of params[i], or `default` if absent.
pub(crate) fn param(params: &[Vec<u16>], i: usize, default: u16) -> u16 {
    params
        .get(i)
        .and_then(|p| p.first())
        .copied()
        .unwrap_or(default)
}

/// Like [`param`] but maps 0 to `default` (cursor-movement convention).
pub(crate) fn param_or(params: &[Vec<u16>], i: usize, default: u16) -> u16 {
    match param(params, i, 0) {
        0 => default,
        v => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Action> {
        let mut p = Parser::new();
        let mut out = Vec::new();
        for &b in bytes {
            p.advance(b, &mut out);
        }
        out
    }

    #[test]
    fn plain_text() {
        assert_eq!(parse(b"hi"), vec![Action::Print('h'), Action::Print('i')]);
    }

    #[test]
    fn csi_basic_params() {
        let acts = parse(b"\x1b[1;2H");
        assert_eq!(
            acts,
            vec![Action::Csi {
                params: vec![vec![1], vec![2]],
                intermediates: vec![],
                private: 0,
                final_byte: b'H',
            }]
        );
    }

    #[test]
    fn csi_no_params() {
        let acts = parse(b"\x1b[m");
        assert_eq!(
            acts,
            vec![Action::Csi {
                params: vec![],
                intermediates: vec![],
                private: 0,
                final_byte: b'm'
            }]
        );
    }

    #[test]
    fn csi_colon_subparams() {
        let acts = parse(b"\x1b[4:3m");
        assert_eq!(
            acts,
            vec![Action::Csi {
                params: vec![vec![4, 3]],
                intermediates: vec![],
                private: 0,
                final_byte: b'm',
            }]
        );
        let acts = parse(b"\x1b[38:2::10:20:30m");
        assert_eq!(
            acts,
            vec![Action::Csi {
                params: vec![vec![38, 2, 0, 10, 20, 30]],
                intermediates: vec![],
                private: 0,
                final_byte: b'm',
            }]
        );
    }

    #[test]
    fn csi_private_marker() {
        let acts = parse(b"\x1b[?2004h");
        assert_eq!(
            acts,
            vec![Action::Csi {
                params: vec![vec![2004]],
                intermediates: vec![],
                private: b'?',
                final_byte: b'h',
            }]
        );
    }

    #[test]
    fn csi_intermediate_byte() {
        let acts = parse(b"\x1b[2 q");
        assert_eq!(
            acts,
            vec![Action::Csi {
                params: vec![vec![2]],
                intermediates: vec![b' '],
                private: 0,
                final_byte: b'q',
            }]
        );
    }

    #[test]
    fn c0_within_csi_executes() {
        let acts = parse(b"\x1b[1\x072H");
        assert_eq!(acts[0], Action::Execute(0x07));
        assert!(matches!(&acts[1], Action::Csi { params, .. } if params == &vec![vec![12]]));
    }

    #[test]
    fn cancel_aborts_csi() {
        let acts = parse(b"\x1b[12\x18Hx");
        assert_eq!(
            acts,
            vec![
                Action::Execute(0x18),
                Action::Print('H'),
                Action::Print('x')
            ]
        );
    }

    #[test]
    fn esc_dispatch() {
        assert_eq!(
            parse(b"\x1bM"),
            vec![Action::Esc {
                intermediates: vec![],
                final_byte: b'M'
            }]
        );
        assert_eq!(
            parse(b"\x1b#8"),
            vec![Action::Esc {
                intermediates: vec![b'#'],
                final_byte: b'8'
            }]
        );
        assert_eq!(
            parse(b"\x1b(0"),
            vec![Action::Esc {
                intermediates: vec![b'('],
                final_byte: b'0'
            }]
        );
    }

    #[test]
    fn osc_bel_and_st() {
        assert_eq!(
            parse(b"\x1b]2;title\x07"),
            vec![Action::Osc {
                data: b"2;title".to_vec(),
                bel: true
            }]
        );
        let acts = parse(b"\x1b]2;title\x1b\\");
        assert_eq!(
            acts[0],
            Action::Osc {
                data: b"2;title".to_vec(),
                bel: false
            }
        );
        // The trailing `\` dispatches as ESC \ (ST), a no-op.
        assert_eq!(
            acts[1],
            Action::Esc {
                intermediates: vec![],
                final_byte: b'\\'
            }
        );
    }

    #[test]
    fn osc_utf8_payload_with_c1_range_bytes() {
        // "Ā" is C4 80: 0x80 must be treated as string content, not C1 PAD.
        let acts = parse("\x1b]2;Ā\u{7}".as_bytes());
        assert_eq!(
            acts,
            vec![Action::Osc {
                data: "2;Ā".as_bytes().to_vec(),
                bel: true
            }]
        );
    }

    #[test]
    fn dcs_dispatch() {
        let acts = parse(b"\x1bP$qm\x1b\\");
        assert_eq!(
            acts[0],
            Action::Dcs {
                params: vec![],
                intermediates: vec![b'$'],
                final_byte: b'q',
                data: b"m".to_vec(),
            }
        );
    }

    #[test]
    fn apc_dispatch() {
        let acts = parse(b"\x1b_Ga=q,i=1;\x1b\\");
        assert_eq!(
            acts[0],
            Action::Apc {
                data: b"Ga=q,i=1;".to_vec()
            }
        );
    }

    #[test]
    fn sos_pm_ignored() {
        let acts = parse(b"\x1bXjunk\x1b\\x");
        assert_eq!(
            acts,
            vec![
                Action::Esc {
                    intermediates: vec![],
                    final_byte: b'\\'
                },
                Action::Print('x')
            ]
        );
    }

    #[test]
    fn utf8_two_three_four_byte() {
        assert_eq!(parse("é".as_bytes()), vec![Action::Print('é')]);
        assert_eq!(parse("中".as_bytes()), vec![Action::Print('中')]);
        assert_eq!(parse("🙂".as_bytes()), vec![Action::Print('🙂')]);
    }

    #[test]
    fn utf8_malformed() {
        // Lone continuation byte.
        assert_eq!(parse(&[0xA9]), vec![Action::Print('\u{fffd}')]);
        // Truncated sequence followed by ASCII: replacement + the ASCII char.
        assert_eq!(
            parse(&[0xC3, b'x']),
            vec![Action::Print('\u{fffd}'), Action::Print('x')]
        );
        // Overlong encoding (C0 80): invalid lead, then 0x80 is C1 PAD.
        assert_eq!(
            parse(&[0xC0, 0x80]),
            vec![Action::Print('\u{fffd}'), Action::Execute(0x80)]
        );
        // Surrogate half (ED A0 80) rejected by str validation.
        let acts = parse(&[0xED, 0xA0, 0x80]);
        assert!(acts.iter().all(|a| *a == Action::Print('\u{fffd}')));
        // Invalid lead byte.
        assert_eq!(parse(&[0xFF]), vec![Action::Print('\u{fffd}')]);
    }

    #[test]
    fn utf8_interrupted_by_escape() {
        let acts = parse(&[0xC3, 0x1B, b'M']);
        assert_eq!(
            acts,
            vec![
                Action::Print('\u{fffd}'),
                Action::Esc {
                    intermediates: vec![],
                    final_byte: b'M'
                }
            ]
        );
    }

    #[test]
    fn eight_bit_c1_csi() {
        let acts = parse(&[0x9B, b'5', b'A']);
        assert_eq!(
            acts,
            vec![Action::Csi {
                params: vec![vec![5]],
                intermediates: vec![],
                private: 0,
                final_byte: b'A',
            }]
        );
    }

    #[test]
    fn decoded_c1_nel() {
        // C2 85 decodes to U+0085 NEL: executed as a C1 control.
        let acts = parse(&[0xC2, 0x85]);
        assert_eq!(acts, vec![Action::Print('\u{85}')]);
        // Note: decoded C1 codepoints are emitted as Print here; the
        // terminal layer maps prints of U+0080..U+009F to controls.
    }

    #[test]
    fn empty_params_default() {
        let acts = parse(b"\x1b[;5H");
        assert_eq!(
            acts,
            vec![Action::Csi {
                params: vec![vec![0], vec![5]],
                intermediates: vec![],
                private: 0,
                final_byte: b'H',
            }]
        );
    }
}
