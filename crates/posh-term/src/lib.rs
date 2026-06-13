//! posh-term: a standalone terminal emulation library.
//!
//! The crate is 100% safe Rust and intends to stay that way (github #36);
//! all libc FFI lives in the `posh` binary crate.
//!
//! This crate is a from-scratch Rust rewrite of the ghostty-vt terminal core.
//! It has no dependencies: callers feed it bytes from a PTY via
//! [`Terminal::process`], query the resulting screen state, and drain any
//! bytes the emulator wants to send back to the application (query replies)
//! via [`Terminal::take_responses`]. The only I/O it performs is reading
//! files named by kitty graphics `t=f`/`t=t` transmissions (shared-memory
//! `t=s` is answered with `EUNSUPPORTED`).
//!
//! # Frozen public API contract
//!
//! The `posh` binary builds against the API below. Implementations may add
//! items but must not remove or change these signatures:
//!
//! - `Terminal::new(rows: u16, cols: u16) -> Terminal`
//! - `Terminal::with_scrollback(rows: u16, cols: u16, scrollback: usize) -> Terminal`
//! - `Terminal::process(&mut self, bytes: &[u8])`
//! - `Terminal::resize(&mut self, rows: u16, cols: u16)`
//! - `Terminal::rows(&self) -> u16` / `Terminal::cols(&self) -> u16`
//! - `Terminal::title(&self) -> &str`
//! - `Terminal::cursor(&self) -> Cursor`
//! - `Terminal::generation(&self) -> u64` (bumped on every visible change)
//! - `Terminal::dump_vt(&self) -> Vec<u8>` (escape stream that reconstructs
//!   the screen, including attributes, cursor, and modes, on a real terminal)
//! - `Terminal::dump_vt_flat(&self) -> Vec<u8>` (single-screen variant: active
//!   grid only, never switches the target's buffers)
//! - `Terminal::dump_screen_switch(&self) -> Vec<u8>` (in-place repaint that
//!   substitutes an application screen switch on a single-screen target)
//! - `Terminal::take_screen_switch(&mut self) -> Option<ScreenSwitch>` /
//!   `Terminal::mid_escape(&self) -> bool` (switch detection for raw-stream
//!   forwarders)
//! - `Terminal::dump_text(&self) -> String` (plain text, scrollback included)
//! - `Terminal::primary_scrollback_len(&self) -> usize` /
//!   `Terminal::primary_scrollback_total(&self) -> u64` /
//!   `Terminal::dump_scrollback_row(&self, i: usize) -> Option<Vec<u8>>`
//!   (scrollback growth measurement and per-row serialization for the
//!   remote scrollback-sync protocol, RFC 0002)
//! - `Terminal::take_responses(&mut self) -> Vec<u8>` (DA/DSR/etc. replies)
//! - `Terminal::screen(&self) -> &Screen` (cell-level read access)
//! - `version() -> &'static str` (the emulator revision, flowed from
//!   version.env at build time; stamped into posh-rec's `.castx` `emu_rev`)
//! - `Color::to_rgb(self) -> Option<(u8,u8,u8)>` (palette/RGB resolution for
//!   renderers; `None` for the terminal default)
#![forbid(unsafe_code)]

/// Placeholder cell size in pixels, shared by the XTWINOPS reports, kitty
/// graphics extents, and SGR-pixel mouse coordinates so the three stay in
/// agreement.
pub(crate) const CELL_W: u32 = 10;
pub(crate) const CELL_H: u32 = 20;

pub mod base64;
mod cell;
mod csi;
mod dcs;
mod dump;
mod graphics;
mod inflate;
mod kitty_keys;
mod modes;
mod mouse;
mod osc;
mod parser;
mod png;
mod screen;
mod terminal;
mod wcwidth;

pub use cell::{Cell, Color, Style, UnderlineStyle};
pub use dump::sgr_params;
pub use graphics::{AnimationState, Frame, Image, ImageFormat, Placement};
pub use kitty_keys::{encode_key, KeyCode, KeyEvent, KeyEventType, KittyFlags, Modifiers};
pub use modes::{MouseMode, MouseProtocol};
pub use mouse::{encode_mouse, MouseButton, MouseEvent, MouseEventKind};
pub use screen::{Row, Screen, SemanticMark};
pub use terminal::{Cursor, CursorShape, ScreenSwitch, Terminal};
pub use wcwidth::wcwidth;

/// The emulator revision string. Flowed from the repo's `version.env`
/// (`POSH_VERSION`) at build time by `build.rs` — see eng-versioning(7). Used
/// by posh-rec to stamp the `.castx` `emu_rev` header so a recorded golden
/// frame can be audited against the emulator version that produced it.
pub fn version() -> &'static str {
    env!("POSH_VERSION")
}
