//! Drive mosh's C++ — the terminal emulator and the predictive-echo overlay —
//! from Rust through C-ABI shims (`csrc/shim.cc`, `csrc/predict_shim.cc`), built
//! by `build.rs`.
//!
//! This underpins the FFI-oracle approach (ADR 0004): mosh's reference behavior
//! becomes byte-for-byte checkable next to posh's Rust reimplementation. The
//! terminal side ([`MoshTerminal`]) needs no mosh changes; the predictor side
//! ([`MoshPredictor`]) became buildable-light after the timing.h decouple (#5)
//! and is made deterministic by an injected clock ([`MoshPredictor::set_clock`]).

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

extern "C" {
    // Terminal emulator (shim.cc).
    fn mosh_term_new(width: c_int, height: c_int) -> *mut c_void;
    fn mosh_term_free(handle: *mut c_void);
    fn mosh_term_feed(handle: *mut c_void, data: *const c_char, len: usize);
    fn mosh_term_render(handle: *mut c_void) -> *mut c_char;
    fn mosh_term_width(handle: *mut c_void) -> c_int;
    fn mosh_term_height(handle: *mut c_void) -> c_int;
    fn mosh_term_cursor_row(handle: *mut c_void) -> c_int;
    fn mosh_term_cursor_col(handle: *mut c_void) -> c_int;

    // Predictive-echo overlay (predict_shim.cc).
    fn mosh_clock_set(ms: u64);
    fn mosh_predict_new(
        width: c_int,
        height: c_int,
        display_pref: c_int,
        predict_overwrite: c_int,
    ) -> *mut c_void;
    fn mosh_predict_free(handle: *mut c_void);
    fn mosh_predict_set_send_interval(handle: *mut c_void, x: c_uint);
    fn mosh_predict_set_frame_sent(handle: *mut c_void, x: u64);
    fn mosh_predict_set_frame_acked(handle: *mut c_void, x: u64);
    fn mosh_predict_set_frame_late_acked(handle: *mut c_void, x: u64);
    fn mosh_predict_feed_server(handle: *mut c_void, data: *const c_char, len: usize);
    fn mosh_predict_key(handle: *mut c_void, byte: c_char);
    fn mosh_predict_render(handle: *mut c_void) -> *mut c_char;

    // Shared string free (defined in shim.cc).
    fn mosh_string_free(s: *mut c_char);
}

unsafe fn take_cstring(p: *mut c_char) -> String {
    assert!(!p.is_null(), "shim returned null string");
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    mosh_string_free(p);
    s
}

/// Safe owner of a mosh `Terminal::Emulator` instance behind the C shim.
pub struct MoshTerminal {
    handle: *mut c_void,
}

impl MoshTerminal {
    /// Creates an emulator of the given size.
    pub fn new(width: u16, height: u16) -> MoshTerminal {
        let handle = unsafe { mosh_term_new(c_int::from(width), c_int::from(height)) };
        assert!(!handle.is_null(), "mosh_term_new returned null");
        MoshTerminal { handle }
    }

    /// Feeds host/server VT bytes into the emulator.
    pub fn feed(&mut self, bytes: &[u8]) {
        unsafe { mosh_term_feed(self.handle, bytes.as_ptr().cast::<c_char>(), bytes.len()) };
    }

    /// Renders the framebuffer to a newline-joined plain-text grid.
    pub fn render(&self) -> String {
        unsafe { take_cstring(mosh_term_render(self.handle)) }
    }

    /// `(row, col)` cursor position, 0-based.
    pub fn cursor(&self) -> (i32, i32) {
        unsafe {
            (
                mosh_term_cursor_row(self.handle),
                mosh_term_cursor_col(self.handle),
            )
        }
    }

    /// `(width, height)` of the emulator grid.
    pub fn size(&self) -> (i32, i32) {
        unsafe { (mosh_term_width(self.handle), mosh_term_height(self.handle)) }
    }
}

impl Drop for MoshTerminal {
    fn drop(&mut self) {
        unsafe { mosh_term_free(self.handle) };
    }
}

/// mosh's prediction display modes (`PredictionEngine::DisplayPreference`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPreference {
    Always = 0,
    Never = 1,
    Adaptive = 2,
    Experimental = 3,
}

/// Safe owner of a mosh `PredictionEngine` (plus the `Emulator` it predicts
/// against) behind the C shim.
///
/// The clock is process-global (mosh's `Network::timestamp()` is a free
/// function), so predictor tests must run single-threaded — keep all driving in
/// one test function.
pub struct MoshPredictor {
    handle: *mut c_void,
}

impl MoshPredictor {
    /// Creates a predictor + emulator of the given size.
    pub fn new(width: u16, height: u16, pref: DisplayPreference, predict_overwrite: bool) -> MoshPredictor {
        let handle = unsafe {
            mosh_predict_new(
                c_int::from(width),
                c_int::from(height),
                pref as c_int,
                c_int::from(predict_overwrite),
            )
        };
        assert!(!handle.is_null(), "mosh_predict_new returned null");
        MoshPredictor { handle }
    }

    /// Sets the injected monotonic clock (ms). Process-global.
    pub fn set_clock(ms: u64) {
        unsafe { mosh_clock_set(ms) };
    }

    pub fn set_send_interval(&mut self, ms: u32) {
        unsafe { mosh_predict_set_send_interval(self.handle, ms) };
    }
    pub fn set_frame_sent(&mut self, n: u64) {
        unsafe { mosh_predict_set_frame_sent(self.handle, n) };
    }
    pub fn set_frame_acked(&mut self, n: u64) {
        unsafe { mosh_predict_set_frame_acked(self.handle, n) };
    }
    pub fn set_frame_late_acked(&mut self, n: u64) {
        unsafe { mosh_predict_set_frame_late_acked(self.handle, n) };
    }

    /// Feeds host/server VT bytes into the confirmed frame.
    pub fn feed_server(&mut self, bytes: &[u8]) {
        unsafe { mosh_predict_feed_server(self.handle, bytes.as_ptr().cast::<c_char>(), bytes.len()) };
    }

    /// Feeds one user keystroke byte to the predictor.
    pub fn key(&mut self, byte: u8) {
        unsafe { mosh_predict_key(self.handle, byte as c_char) };
    }

    /// Renders the confirmed frame with predictions overlaid.
    pub fn render(&self) -> String {
        unsafe { take_cstring(mosh_predict_render(self.handle)) }
    }
}

impl Drop for MoshPredictor {
    fn drop(&mut self) {
        unsafe { mosh_predict_free(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_text_and_tracks_cursor() {
        let mut t = MoshTerminal::new(20, 3);
        assert_eq!(t.size(), (20, 3));
        t.feed(b"hello");
        let grid = t.render();
        let row0 = grid.lines().next().unwrap();
        assert!(row0.starts_with("hello"), "row0 = {row0:?}");
        assert_eq!(row0.chars().count(), 20, "row should be padded to width");
        assert_eq!(t.cursor(), (0, 5));
    }

    #[test]
    fn handles_csi_cursor_positioning() {
        let mut t = MoshTerminal::new(20, 3);
        // Print A at home, CUP to (row 2, col 3) 1-based, print B.
        t.feed(b"A\x1b[2;3HB");
        let grid = t.render();
        let lines: Vec<&str> = grid.lines().collect();
        assert!(lines[0].starts_with('A'), "row0 = {:?}", lines[0]);
        // 1-based (2,3) -> 0-based (1,2); after printing B the cursor is at col 3.
        assert_eq!(&lines[1][2..3], "B", "row1 = {:?}", lines[1]);
        assert_eq!(t.cursor(), (1, 3));
    }
}
