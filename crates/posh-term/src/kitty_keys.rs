//! Kitty keyboard protocol: progressive enhancement flag stack (terminal
//! side) and a key-event encoder (client side) covering both legacy and
//! kitty CSI u encodings.

use std::fmt::Write;

/// Kitty keyboard protocol progressive enhancement flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KittyFlags(pub u8);

impl KittyFlags {
    pub const DISAMBIGUATE: KittyFlags = KittyFlags(1);
    pub const REPORT_EVENTS: KittyFlags = KittyFlags(2);
    pub const REPORT_ALTERNATE: KittyFlags = KittyFlags(4);
    pub const REPORT_ALL: KittyFlags = KittyFlags(8);
    pub const REPORT_TEXT: KittyFlags = KittyFlags(16);

    pub fn contains(self, other: KittyFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for KittyFlags {
    type Output = KittyFlags;
    fn bitor(self, rhs: KittyFlags) -> KittyFlags {
        KittyFlags(self.0 | rhs.0)
    }
}

/// Per-screen flag stack (the kitty spec gives the main and alternate
/// screens independent stacks).
#[derive(Debug, Default)]
pub(crate) struct KittyKeyStack {
    stack: Vec<u8>,
}

const MAX_STACK: usize = 128;

impl KittyKeyStack {
    pub fn flags(&self) -> KittyFlags {
        KittyFlags(self.stack.last().copied().unwrap_or(0))
    }

    /// Pushed entries, oldest first (for dump_vt replay).
    pub fn entries(&self) -> &[u8] {
        &self.stack
    }

    pub fn push(&mut self, flags: u8) {
        if self.stack.len() >= MAX_STACK {
            self.stack.remove(0);
        }
        self.stack.push(flags & 0x1f);
    }

    pub fn pop(&mut self, n: u16) {
        for _ in 0..n {
            if self.stack.pop().is_none() {
                break;
            }
        }
    }

    /// `CSI = flags ; mode u`: mode 1 = set, 2 = or, 3 = and-not.
    pub fn set(&mut self, flags: u8, mode: u16) {
        let cur = self.stack.last().copied().unwrap_or(0);
        let new = match mode {
            2 => cur | (flags & 0x1f),
            3 => cur & !(flags & 0x1f),
            _ => flags & 0x1f,
        };
        match self.stack.last_mut() {
            Some(top) => *top = new,
            None => self.stack.push(new),
        }
    }

    pub fn reset(&mut self) {
        self.stack.clear();
    }
}

/// Keyboard modifiers, kitty bit layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const SHIFT: Modifiers = Modifiers(1);
    pub const ALT: Modifiers = Modifiers(2);
    pub const CTRL: Modifiers = Modifiers(4);
    pub const SUPER: Modifiers = Modifiers(8);
    pub const HYPER: Modifiers = Modifiers(16);
    pub const META: Modifiers = Modifiers(32);
    pub const CAPS_LOCK: Modifiers = Modifiers(64);
    pub const NUM_LOCK: Modifiers = Modifiers(128);

    pub fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    fn any_of(self, mask: u8) -> bool {
        self.0 & mask != 0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    fn bitor(self, rhs: Modifiers) -> Modifiers {
        Modifiers(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    /// A text-producing key, given as its unshifted (base layout) character.
    Char(char),
    Escape,
    Enter,
    Tab,
    Backspace,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    /// Function key F1..=F12.
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyEventType {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: KeyCode,
    pub mods: Modifiers,
    pub event_type: KeyEventType,
}

impl KeyEvent {
    pub fn new(key: KeyCode, mods: Modifiers) -> KeyEvent {
        KeyEvent {
            key,
            mods,
            event_type: KeyEventType::Press,
        }
    }
}

/// Encodes a key event as the byte sequence a terminal would send to the
/// application, honoring the active kitty protocol `flags`, and (in legacy
/// mode) DECCKM `app_cursor`.
pub fn encode_key(key: KeyEvent, flags: KittyFlags, app_cursor: bool) -> Vec<u8> {
    if flags.is_empty() {
        encode_legacy(key, app_cursor)
    } else {
        encode_kitty(key, flags, app_cursor)
    }
}

/// Modifier field value: 1 + bits (kitty / xterm convention).
fn mod_value(mods: Modifiers) -> u16 {
    u16::from(mods.0) + 1
}

fn event_num(t: KeyEventType) -> u8 {
    match t {
        KeyEventType::Press => 1,
        KeyEventType::Repeat => 2,
        KeyEventType::Release => 3,
    }
}

/// The legacy control byte for ctrl+key, if any.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        ' ' | '@' | '2' => Some(0),
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' | '3' => Some(27),
        '\\' | '4' => Some(28),
        ']' | '5' => Some(29),
        '^' | '6' => Some(30),
        '_' | '7' | '/' => Some(31),
        '?' | '8' => Some(127),
        _ => None,
    }
}

/// CSI-letter functional keys: (final letter, usable with SS3 when
/// unmodified in application cursor mode).
fn letter_key(key: KeyCode) -> Option<(char, bool)> {
    match key {
        KeyCode::Up => Some(('A', true)),
        KeyCode::Down => Some(('B', true)),
        KeyCode::Right => Some(('C', true)),
        KeyCode::Left => Some(('D', true)),
        KeyCode::Home => Some(('H', true)),
        KeyCode::End => Some(('F', true)),
        KeyCode::F(1) => Some(('P', false)),
        KeyCode::F(2) => Some(('Q', false)),
        KeyCode::F(3) => Some(('R', false)),
        KeyCode::F(4) => Some(('S', false)),
        _ => None,
    }
}

/// Tilde-form functional keys: `CSI {n} ~`.
fn tilde_key(key: KeyCode) -> Option<u16> {
    match key {
        KeyCode::Insert => Some(2),
        KeyCode::Delete => Some(3),
        KeyCode::PageUp => Some(5),
        KeyCode::PageDown => Some(6),
        KeyCode::F(5) => Some(15),
        KeyCode::F(6) => Some(17),
        KeyCode::F(7) => Some(18),
        KeyCode::F(8) => Some(19),
        KeyCode::F(9) => Some(20),
        KeyCode::F(10) => Some(21),
        KeyCode::F(11) => Some(23),
        KeyCode::F(12) => Some(24),
        _ => None,
    }
}

fn encode_legacy(ev: KeyEvent, app_cursor: bool) -> Vec<u8> {
    if ev.event_type == KeyEventType::Release {
        return Vec::new();
    }
    let mods = ev.mods;
    let alt = mods.contains(Modifiers::ALT);
    let ctrl = mods.contains(Modifiers::CTRL);

    if let Some((letter, ss3_ok)) = letter_key(ev.key) {
        // Only shift/alt/ctrl participate in legacy modifier encoding.
        let m = Modifiers(mods.0 & 0x07);
        return if m.0 == 0 {
            // SS3 (\x1bO) when app-cursor mode wants it on an SS3-capable key,
            // or for F1-F4 which always default to SS3; otherwise CSI (\x1b[).
            if !ss3_ok || app_cursor {
                format!("\x1bO{letter}").into_bytes()
            } else {
                format!("\x1b[{letter}").into_bytes()
            }
        } else {
            format!("\x1b[1;{}{letter}", mod_value(m)).into_bytes()
        };
    }
    if let Some(n) = tilde_key(ev.key) {
        let m = Modifiers(mods.0 & 0x07);
        return if m.0 == 0 {
            format!("\x1b[{n}~").into_bytes()
        } else {
            format!("\x1b[{n};{}~", mod_value(m)).into_bytes()
        };
    }

    let mut out = Vec::new();
    match ev.key {
        KeyCode::Escape => {
            if alt {
                out.push(0x1b);
            }
            out.push(0x1b);
        }
        KeyCode::Enter => {
            if alt {
                out.push(0x1b);
            }
            out.push(b'\r');
        }
        KeyCode::Tab => {
            if mods.contains(Modifiers::SHIFT) {
                out.extend_from_slice(b"\x1b[Z");
                return out;
            }
            if alt {
                out.push(0x1b);
            }
            out.push(b'\t');
        }
        KeyCode::Backspace => {
            if alt {
                out.push(0x1b);
            }
            out.push(if ctrl { 0x08 } else { 0x7f });
        }
        KeyCode::Char(c) => {
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                if let Some(b) = ctrl_byte(c) {
                    out.push(b);
                    return out;
                }
            }
            let c = if mods.contains(Modifiers::SHIFT) {
                shifted_char(c)
            } else {
                c
            };
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        _ => {}
    }
    out
}

/// Best-effort shifted form of a base key (letters only; other keys keep
/// their character since the layout is unknown).
fn shifted_char(c: char) -> char {
    c.to_ascii_uppercase()
}

fn encode_kitty(ev: KeyEvent, flags: KittyFlags, app_cursor: bool) -> Vec<u8> {
    let report_events = flags.contains(KittyFlags::REPORT_EVENTS);
    let report_all = flags.contains(KittyFlags::REPORT_ALL);
    let mods = ev.mods;

    // Functional keys with dedicated CSI forms keep them; modifiers and
    // event types ride in the second parameter position.
    if let Some((letter, ss3_ok)) = letter_key(ev.key) {
        if ev.event_type == KeyEventType::Release && !report_events {
            return Vec::new();
        }
        // While the kitty protocol is active DECCKM is ignored; F1-F4 keep
        // their SS3 legacy form when unmodified.
        return csi_form("1", mods, ev.event_type, flags, letter).unwrap_or_else(|| {
            if ss3_ok {
                format!("\x1b[{letter}").into_bytes()
            } else {
                format!("\x1bO{letter}").into_bytes()
            }
        });
    }
    if let Some(n) = tilde_key(ev.key) {
        if ev.event_type == KeyEventType::Release && !report_events {
            return Vec::new();
        }
        return csi_form(&n.to_string(), mods, ev.event_type, flags, '~')
            .unwrap_or_else(|| format!("\x1b[{n}~").into_bytes());
    }

    // CSI u candidates: Escape always (with disambiguate); Enter, Tab,
    // Backspace only with modifiers or report-all; text keys with
    // ctrl/alt/super-class modifiers or report-all.
    let code: u32 = match ev.key {
        KeyCode::Escape => 27,
        KeyCode::Enter => 13,
        KeyCode::Tab => 9,
        KeyCode::Backspace => 127,
        KeyCode::Char(c) => c.to_lowercase().next().unwrap_or(c) as u32,
        _ => return Vec::new(),
    };
    let non_shift_mods = mods.any_of(!1 & 0x3f); // anything besides shift/locks
    let csi_u_needed = report_all
        || matches!(ev.key, KeyCode::Escape)
        || non_shift_mods
        || (report_events && ev.event_type != KeyEventType::Press);
    if !csi_u_needed {
        return encode_legacy(ev, app_cursor);
    }
    if ev.event_type == KeyEventType::Release && !report_events {
        return Vec::new();
    }

    // Alternate (shifted) key reporting: `code:shifted`.
    let alternate =
        if flags.contains(KittyFlags::REPORT_ALTERNATE) && mods.contains(Modifiers::SHIFT) {
            match ev.key {
                KeyCode::Char(c) => {
                    let s = shifted_char(c);
                    if s as u32 != code {
                        Some(s as u32)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            None
        };

    let mut s = format!("\x1b[{code}");
    if let Some(alt_code) = alternate {
        let _ = write!(s, ":{alt_code}");
    }
    let ev_suffix = report_events && ev.event_type != KeyEventType::Press;
    let text = if flags.contains(KittyFlags::REPORT_TEXT) && ev.event_type != KeyEventType::Release
    {
        match ev.key {
            KeyCode::Char(c)
                if !mods.contains(Modifiers::CTRL) && !mods.contains(Modifiers::ALT) =>
            {
                let ch = if mods.contains(Modifiers::SHIFT) {
                    shifted_char(c)
                } else {
                    c
                };
                Some(ch as u32)
            }
            _ => None,
        }
    } else {
        None
    };
    if mods.0 != 0 || ev_suffix || text.is_some() {
        let _ = write!(s, ";{}", mod_value(mods));
        if ev_suffix {
            let _ = write!(s, ":{}", event_num(ev.event_type));
        }
    }
    if let Some(t) = text {
        let _ = write!(s, ";{t}");
    }
    s.push('u');
    s.into_bytes()
}

/// Builds `CSI {num};{mods}:{event}{final}` when modifiers or an event type
/// must be encoded; returns `None` when the bare legacy form suffices.
fn csi_form(
    num: &str,
    mods: Modifiers,
    event_type: KeyEventType,
    flags: KittyFlags,
    final_ch: char,
) -> Option<Vec<u8>> {
    let ev_suffix = flags.contains(KittyFlags::REPORT_EVENTS) && event_type != KeyEventType::Press;
    if mods.0 == 0 && !ev_suffix {
        return None;
    }
    let mut s = format!("\x1b[{num};{}", mod_value(mods));
    if ev_suffix {
        let _ = write!(s, ":{}", event_num(event_type));
    }
    s.push(final_ch);
    Some(s.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: KeyCode, mods: Modifiers) -> KeyEvent {
        KeyEvent::new(key, mods)
    }

    const NO: KittyFlags = KittyFlags(0);
    const DIS: KittyFlags = KittyFlags::DISAMBIGUATE;

    #[test]
    fn legacy_plain_char() {
        assert_eq!(
            encode_key(ev(KeyCode::Char('a'), Modifiers::NONE), NO, false),
            b"a"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Char('a'), Modifiers::SHIFT), NO, false),
            b"A"
        );
    }

    #[test]
    fn legacy_ctrl_and_alt() {
        assert_eq!(
            encode_key(ev(KeyCode::Char('a'), Modifiers::CTRL), NO, false),
            b"\x01"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Char('a'), Modifiers::ALT), NO, false),
            b"\x1ba"
        );
        assert_eq!(
            encode_key(
                ev(KeyCode::Char('a'), Modifiers::CTRL | Modifiers::ALT),
                NO,
                false
            ),
            b"\x1b\x01"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Char(' '), Modifiers::CTRL), NO, false),
            b"\x00"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Char('['), Modifiers::CTRL), NO, false),
            b"\x1b"
        );
    }

    #[test]
    fn legacy_arrows_and_app_cursor() {
        assert_eq!(
            encode_key(ev(KeyCode::Up, Modifiers::NONE), NO, false),
            b"\x1b[A"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Up, Modifiers::NONE), NO, true),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Up, Modifiers::SHIFT), NO, true),
            b"\x1b[1;2A"
        );
        assert_eq!(
            encode_key(
                ev(KeyCode::Right, Modifiers::CTRL | Modifiers::SHIFT),
                NO,
                false
            ),
            b"\x1b[1;6C"
        );
    }

    #[test]
    fn legacy_functional() {
        assert_eq!(
            encode_key(ev(KeyCode::F(1), Modifiers::NONE), NO, false),
            b"\x1bOP"
        );
        assert_eq!(
            encode_key(ev(KeyCode::F(5), Modifiers::NONE), NO, false),
            b"\x1b[15~"
        );
        assert_eq!(
            encode_key(ev(KeyCode::F(5), Modifiers::CTRL), NO, false),
            b"\x1b[15;5~"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Delete, Modifiers::NONE), NO, false),
            b"\x1b[3~"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Tab, Modifiers::SHIFT), NO, false),
            b"\x1b[Z"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Enter, Modifiers::NONE), NO, false),
            b"\r"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Backspace, Modifiers::NONE), NO, false),
            b"\x7f"
        );
    }

    #[test]
    fn legacy_release_ignored() {
        let mut e = ev(KeyCode::Char('a'), Modifiers::NONE);
        e.event_type = KeyEventType::Release;
        assert_eq!(encode_key(e, NO, false), b"");
    }

    // Kitty spec example: ESC with disambiguate is CSI 27 u.
    #[test]
    fn kitty_escape_disambiguated() {
        assert_eq!(
            encode_key(ev(KeyCode::Escape, Modifiers::NONE), DIS, false),
            b"\x1b[27u"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Escape, Modifiers::CTRL), DIS, false),
            b"\x1b[27;5u"
        );
    }

    // Kitty spec example: ctrl+shift+a reports the lowercase codepoint with
    // modifier value 1 + (shift=1 + ctrl=4) = 6.
    #[test]
    fn kitty_ctrl_shift_a() {
        assert_eq!(
            encode_key(
                ev(KeyCode::Char('a'), Modifiers::CTRL | Modifiers::SHIFT),
                DIS,
                false
            ),
            b"\x1b[97;6u"
        );
    }

    #[test]
    fn kitty_alternate_keys() {
        let flags = DIS | KittyFlags::REPORT_ALTERNATE;
        assert_eq!(
            encode_key(
                ev(KeyCode::Char('a'), Modifiers::CTRL | Modifiers::SHIFT),
                flags,
                false
            ),
            b"\x1b[97:65;6u"
        );
    }

    #[test]
    fn kitty_shift_only_text_stays_legacy() {
        assert_eq!(
            encode_key(ev(KeyCode::Char('a'), Modifiers::SHIFT), DIS, false),
            b"A"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Char('a'), Modifiers::NONE), DIS, false),
            b"a"
        );
    }

    #[test]
    fn kitty_enter_tab_backspace() {
        // Unmodified: legacy bytes even with disambiguate.
        assert_eq!(
            encode_key(ev(KeyCode::Enter, Modifiers::NONE), DIS, false),
            b"\r"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Tab, Modifiers::NONE), DIS, false),
            b"\t"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Backspace, Modifiers::NONE), DIS, false),
            b"\x7f"
        );
        // Modified: CSI u.
        assert_eq!(
            encode_key(ev(KeyCode::Enter, Modifiers::CTRL), DIS, false),
            b"\x1b[13;5u"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Backspace, Modifiers::ALT), DIS, false),
            b"\x1b[127;3u"
        );
    }

    #[test]
    fn kitty_event_types() {
        let flags = DIS | KittyFlags::REPORT_EVENTS;
        let mut e = ev(KeyCode::Char('a'), Modifiers::CTRL);
        assert_eq!(encode_key(e, flags, false), b"\x1b[97;5u");
        e.event_type = KeyEventType::Repeat;
        assert_eq!(encode_key(e, flags, false), b"\x1b[97;5:2u");
        e.event_type = KeyEventType::Release;
        assert_eq!(encode_key(e, flags, false), b"\x1b[97;5:3u");
    }

    #[test]
    fn kitty_arrow_event_types() {
        let flags = DIS | KittyFlags::REPORT_EVENTS;
        let mut e = ev(KeyCode::Up, Modifiers::NONE);
        assert_eq!(encode_key(e, flags, false), b"\x1b[A");
        e.event_type = KeyEventType::Release;
        assert_eq!(encode_key(e, flags, false), b"\x1b[1;1:3A");
    }

    #[test]
    fn kitty_report_all() {
        let flags = KittyFlags::REPORT_ALL;
        assert_eq!(
            encode_key(ev(KeyCode::Char('a'), Modifiers::NONE), flags, false),
            b"\x1b[97u"
        );
        assert_eq!(
            encode_key(ev(KeyCode::Enter, Modifiers::NONE), flags, false),
            b"\x1b[13u"
        );
    }

    #[test]
    fn kitty_report_text() {
        let flags = KittyFlags::REPORT_ALL | KittyFlags::REPORT_TEXT;
        assert_eq!(
            encode_key(ev(KeyCode::Char('a'), Modifiers::SHIFT), flags, false),
            b"\x1b[97;2;65u"
        );
    }

    #[test]
    fn stack_push_pop_set() {
        let mut s = KittyKeyStack::default();
        assert_eq!(s.flags(), KittyFlags(0));
        s.push(1);
        s.push(15);
        assert_eq!(s.flags(), KittyFlags(15));
        s.pop(1);
        assert_eq!(s.flags(), KittyFlags(1));
        s.set(2, 2); // or
        assert_eq!(s.flags(), KittyFlags(3));
        s.set(1, 3); // and-not
        assert_eq!(s.flags(), KittyFlags(2));
        s.set(31, 1);
        assert_eq!(s.flags(), KittyFlags(31));
        s.pop(10);
        assert_eq!(s.flags(), KittyFlags(0));
    }
}
