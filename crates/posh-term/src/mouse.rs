//! Mouse event encoding (client side, like `encode_key`): produces the
//! byte sequence a terminal sends to the application for a mouse event
//! under the active tracking mode and coordinate protocol.

use crate::kitty_keys::Modifiers;
use crate::modes::{MouseMode, MouseProtocol};

/// Placeholder cell size in pixels for SGR-pixel coordinates when the
/// event carries none, matching the XTWINOPS report.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Motion with no button held.
    None,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseEventKind {
    #[default]
    Press,
    Release,
    Motion,
}

/// A mouse event in 0-based cell coordinates. `pixel` is the 0-based pixel
/// position used by the SGR-pixel protocol (1016); when `None` it is
/// derived from the cell with the 10x20 placeholder cell size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: MouseButton,
    pub kind: MouseEventKind,
    pub row: u16,
    pub col: u16,
    pub mods: Modifiers,
    pub pixel: Option<(u16, u16)>,
}

impl MouseEvent {
    pub fn new(button: MouseButton, kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        MouseEvent {
            button,
            kind,
            row,
            col,
            mods: Modifiers::NONE,
            pixel: None,
        }
    }
}

/// Encodes a mouse event, or `None` when the tracking mode does not report
/// it (e.g. motion without a button under 1002, anything under X10 but a
/// press, wheel releases).
pub fn encode_mouse(
    event: MouseEvent,
    mode: MouseMode,
    protocol: MouseProtocol,
) -> Option<Vec<u8>> {
    let wheel = matches!(
        event.button,
        MouseButton::WheelUp
            | MouseButton::WheelDown
            | MouseButton::WheelLeft
            | MouseButton::WheelRight
    );
    match mode {
        MouseMode::None => return None,
        MouseMode::X10 => {
            if event.kind != MouseEventKind::Press {
                return None;
            }
        }
        MouseMode::Normal => {
            if event.kind == MouseEventKind::Motion {
                return None;
            }
        }
        MouseMode::ButtonEvent => {
            // Drag only: motion requires a held button.
            if event.kind == MouseEventKind::Motion && event.button == MouseButton::None {
                return None;
            }
        }
        MouseMode::AnyEvent => {}
    }
    if wheel && event.kind == MouseEventKind::Release {
        return None; // wheel "buttons" have no release
    }

    let mut cb: u8 = match event.button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::None => 3,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
        MouseButton::WheelLeft => 66,
        MouseButton::WheelRight => 67,
    };
    if event.kind == MouseEventKind::Motion {
        cb += 32;
    }
    // X10 compatibility mode reports no modifiers.
    if mode != MouseMode::X10 {
        if event.mods.contains(Modifiers::SHIFT) {
            cb += 4;
        }
        if event.mods.contains(Modifiers::ALT) {
            cb += 8;
        }
        if event.mods.contains(Modifiers::CTRL) {
            cb += 16;
        }
    }

    match protocol {
        MouseProtocol::Sgr | MouseProtocol::SgrPixel => {
            let (x, y) = if protocol == MouseProtocol::SgrPixel {
                let (px, py) = event
                    .pixel
                    .map(|(x, y)| (u32::from(x), u32::from(y)))
                    .unwrap_or((u32::from(event.col) * CELL_W, u32::from(event.row) * CELL_H));
                (px + 1, py + 1)
            } else {
                (u32::from(event.col) + 1, u32::from(event.row) + 1)
            };
            let final_ch = if event.kind == MouseEventKind::Release {
                'm'
            } else {
                'M'
            };
            Some(format!("\x1b[<{cb};{x};{y}{final_ch}").into_bytes())
        }
        MouseProtocol::Normal | MouseProtocol::Utf8 => {
            // Legacy encoding loses button identity on release.
            if event.kind == MouseEventKind::Release {
                cb = (cb & !0b11) | 3;
            }
            let mut out = vec![0x1b, b'[', b'M'];
            out.push(32 + cb);
            let max = if protocol == MouseProtocol::Utf8 {
                2014 // 0x7FF - 33, the largest UTF-8 (1005) coordinate
            } else {
                222 // 255 - 33, the largest single-byte coordinate
            };
            for v in [event.col, event.row] {
                let coord = 32 + 1 + u32::from(v).min(max);
                if protocol == MouseProtocol::Utf8 && coord > 127 {
                    let mut buf = [0u8; 4];
                    let c = char::from_u32(coord).unwrap_or(' ');
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                } else {
                    out.push(coord as u8);
                }
            }
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(button: MouseButton, kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        MouseEvent::new(button, kind, row, col)
    }

    #[test]
    fn no_tracking_reports_nothing() {
        let e = ev(MouseButton::Left, MouseEventKind::Press, 0, 0);
        assert_eq!(encode_mouse(e, MouseMode::None, MouseProtocol::Sgr), None);
    }

    #[test]
    fn legacy_press_release() {
        let press = ev(MouseButton::Left, MouseEventKind::Press, 4, 9);
        assert_eq!(
            encode_mouse(press, MouseMode::Normal, MouseProtocol::Normal).unwrap(),
            // button 0, col 10, row 5 -> 32+0, 32+10, 32+5
            vec![0x1b, b'[', b'M', 32, 42, 37]
        );
        let release = ev(MouseButton::Left, MouseEventKind::Release, 4, 9);
        assert_eq!(
            encode_mouse(release, MouseMode::Normal, MouseProtocol::Normal).unwrap(),
            vec![0x1b, b'[', b'M', 35, 42, 37] // release reports button 3
        );
    }

    #[test]
    fn legacy_clamps_large_coordinates() {
        let e = ev(MouseButton::Left, MouseEventKind::Press, 500, 500);
        let bytes = encode_mouse(e, MouseMode::Normal, MouseProtocol::Normal).unwrap();
        assert_eq!(&bytes[4..], &[255, 255]);
    }

    #[test]
    fn x10_only_press_no_modifiers() {
        let mut e = ev(MouseButton::Right, MouseEventKind::Press, 0, 0);
        e.mods = Modifiers::CTRL;
        assert_eq!(
            encode_mouse(e, MouseMode::X10, MouseProtocol::Normal).unwrap(),
            vec![0x1b, b'[', b'M', 34, 33, 33] // ctrl not encoded
        );
        e.kind = MouseEventKind::Release;
        assert_eq!(encode_mouse(e, MouseMode::X10, MouseProtocol::Normal), None);
    }

    #[test]
    fn modifiers_add_button_bits() {
        let mut e = ev(MouseButton::Left, MouseEventKind::Press, 0, 0);
        e.mods = Modifiers::SHIFT | Modifiers::CTRL;
        assert_eq!(
            encode_mouse(e, MouseMode::Normal, MouseProtocol::Sgr).unwrap(),
            b"\x1b[<20;1;1M"
        );
    }

    #[test]
    fn sgr_press_release_keep_button() {
        let press = ev(MouseButton::Right, MouseEventKind::Press, 2, 5);
        assert_eq!(
            encode_mouse(press, MouseMode::Normal, MouseProtocol::Sgr).unwrap(),
            b"\x1b[<2;6;3M"
        );
        let release = ev(MouseButton::Right, MouseEventKind::Release, 2, 5);
        assert_eq!(
            encode_mouse(release, MouseMode::Normal, MouseProtocol::Sgr).unwrap(),
            b"\x1b[<2;6;3m"
        );
    }

    #[test]
    fn motion_filtering_by_mode() {
        let drag = {
            let mut e = ev(MouseButton::Left, MouseEventKind::Motion, 0, 0);
            e.mods = Modifiers::NONE;
            e
        };
        let hover = ev(MouseButton::None, MouseEventKind::Motion, 0, 0);
        assert_eq!(
            encode_mouse(drag, MouseMode::Normal, MouseProtocol::Sgr),
            None
        );
        assert_eq!(
            encode_mouse(drag, MouseMode::ButtonEvent, MouseProtocol::Sgr).unwrap(),
            b"\x1b[<32;1;1M"
        );
        assert_eq!(
            encode_mouse(hover, MouseMode::ButtonEvent, MouseProtocol::Sgr),
            None
        );
        assert_eq!(
            encode_mouse(hover, MouseMode::AnyEvent, MouseProtocol::Sgr).unwrap(),
            b"\x1b[<35;1;1M"
        );
    }

    #[test]
    fn wheel_events() {
        let up = ev(MouseButton::WheelUp, MouseEventKind::Press, 0, 0);
        assert_eq!(
            encode_mouse(up, MouseMode::Normal, MouseProtocol::Sgr).unwrap(),
            b"\x1b[<64;1;1M"
        );
        let down = ev(MouseButton::WheelDown, MouseEventKind::Press, 0, 0);
        assert_eq!(
            encode_mouse(down, MouseMode::Normal, MouseProtocol::Normal).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 65, 33, 33]
        );
        let rel = ev(MouseButton::WheelUp, MouseEventKind::Release, 0, 0);
        assert_eq!(
            encode_mouse(rel, MouseMode::Normal, MouseProtocol::Sgr),
            None
        );
    }

    #[test]
    fn sgr_pixel_uses_pixel_or_derives() {
        let mut e = ev(MouseButton::Left, MouseEventKind::Press, 2, 3);
        assert_eq!(
            encode_mouse(e, MouseMode::Normal, MouseProtocol::SgrPixel).unwrap(),
            b"\x1b[<0;31;41M" // derived from 10x20 cells
        );
        e.pixel = Some((100, 200));
        assert_eq!(
            encode_mouse(e, MouseMode::Normal, MouseProtocol::SgrPixel).unwrap(),
            b"\x1b[<0;101;201M"
        );
    }

    #[test]
    fn utf8_protocol_encodes_large_coordinates() {
        let e = ev(MouseButton::Left, MouseEventKind::Press, 0, 200);
        let bytes = encode_mouse(e, MouseMode::Normal, MouseProtocol::Utf8).unwrap();
        // col 200 -> 233 -> 2-byte UTF-8.
        assert_eq!(&bytes[..4], &[0x1b, b'[', b'M', 32]);
        assert_eq!(&bytes[4..6], "é".as_bytes()); // U+00E9 = 233
        assert_eq!(bytes[6], 33);
    }
}
