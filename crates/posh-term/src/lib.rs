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
//! - `Terminal::dump_text(&self) -> String` (plain text, scrollback included)
//! - `Terminal::take_responses(&mut self) -> Vec<u8>` (DA/DSR/etc. replies)
//! - `Terminal::screen(&self) -> &Screen` (cell-level read access)
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
pub use terminal::{Cursor, CursorShape, Terminal};
pub use wcwidth::wcwidth;
