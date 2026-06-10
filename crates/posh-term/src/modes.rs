//! Terminal mode state (DEC private and ANSI modes).

/// Mouse tracking mode (which events are reported).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    None,
    /// DECSET 9: X10 compatibility (press only).
    X10,
    /// DECSET 1000: press + release.
    Normal,
    /// DECSET 1002: press + release + drag.
    ButtonEvent,
    /// DECSET 1003: all motion.
    AnyEvent,
}

impl MouseMode {
    /// The DECSET parameter that enables this mode (`None` for no tracking).
    pub fn decset(self) -> Option<u16> {
        match self {
            MouseMode::None => None,
            MouseMode::X10 => Some(9),
            MouseMode::Normal => Some(1000),
            MouseMode::ButtonEvent => Some(1002),
            MouseMode::AnyEvent => Some(1003),
        }
    }
}

/// Mouse coordinate encoding protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseProtocol {
    /// Legacy single-byte coordinates.
    #[default]
    Normal,
    /// DECSET 1005: UTF-8 coordinates.
    Utf8,
    /// DECSET 1006: SGR (`CSI <` form).
    Sgr,
    /// DECSET 1016: SGR with pixel coordinates.
    SgrPixel,
}

impl MouseProtocol {
    /// The DECSET parameter that selects this protocol (`None` for legacy).
    pub fn decset(self) -> Option<u16> {
        match self {
            MouseProtocol::Normal => None,
            MouseProtocol::Utf8 => Some(1005),
            MouseProtocol::Sgr => Some(1006),
            MouseProtocol::SgrPixel => Some(1016),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Modes {
    /// DECCKM: application cursor keys.
    pub cursor_keys: bool,
    /// DECOM: origin mode (cursor addressing relative to scroll region).
    pub origin: bool,
    /// DECAWM: autowrap.
    pub autowrap: bool,
    /// DECTCEM: cursor visible.
    pub cursor_visible: bool,
    /// DECSET 2004: bracketed paste.
    pub bracketed_paste: bool,
    /// DECSET 1004: focus in/out reporting.
    pub focus_reporting: bool,
    /// DECSET 2026: synchronized output.
    pub synchronized: bool,
    /// IRM (SM 4): insert mode.
    pub insert: bool,
    /// LNM (SM 20): linefeed also performs carriage return.
    pub lnm: bool,
    /// DECARM: autorepeat.
    pub autorepeat: bool,
    /// DECKPAM / DECKPNM (ESC = / ESC >): application keypad.
    pub keypad_app: bool,
    /// DECSCNM: reverse video.
    pub reverse_video: bool,
    /// DECSET 12: cursor blink override.
    pub cursor_blink: bool,
    /// DECCOLM (DECSET 3): 132-column mode.
    pub deccolm: bool,
    /// DECSET 40: allow DECCOLM (xterm gates mode 3 behind this).
    pub allow_deccolm: bool,
    /// DECNCSM (DECSET 95): DECCOLM does not clear the screen.
    pub no_clear_on_deccolm: bool,
    pub mouse_mode: MouseMode,
    pub mouse_protocol: MouseProtocol,
}

impl Default for Modes {
    fn default() -> Modes {
        Modes {
            cursor_keys: false,
            origin: false,
            autowrap: true,
            cursor_visible: true,
            bracketed_paste: false,
            focus_reporting: false,
            synchronized: false,
            insert: false,
            lnm: false,
            autorepeat: true,
            keypad_app: false,
            reverse_video: false,
            cursor_blink: false,
            deccolm: false,
            allow_deccolm: false,
            no_clear_on_deccolm: false,
            mouse_mode: MouseMode::None,
            mouse_protocol: MouseProtocol::Normal,
        }
    }
}
