//! Integration tests for the kitty-parity extensions: reflow, DECCOLM,
//! graphics completion, dump_vt additions, OSC 52 selections, the color
//! stack, DECSTR, DECRQM coverage, selective erase, OSC 66, and mouse
//! encoding.

use posh_term::{
    encode_mouse, KittyFlags, Modifiers, MouseButton, MouseEvent, MouseEventKind, MouseMode,
    MouseProtocol, Terminal,
};

fn term() -> Terminal {
    Terminal::new(5, 10)
}

fn feed(t: &mut Terminal, s: &str) {
    t.process(s.as_bytes());
}

fn row_text(t: &Terminal, r: u16) -> String {
    t.screen().row(r).unwrap().text(true)
}

fn pos(t: &Terminal) -> (u16, u16) {
    let c = t.cursor();
    (c.row, c.col)
}

fn responses(t: &mut Terminal) -> String {
    String::from_utf8(t.take_responses()).unwrap()
}

fn roundtrip(t: &Terminal) -> Terminal {
    let mut t2 = Terminal::new(t.rows(), t.cols());
    t2.process(&t.dump_vt());
    t2
}

// --- resize reflow ------------------------------------------------------------

#[test]
fn reflow_narrow_then_widen_is_lossless() {
    let mut t = Terminal::new(4, 12);
    feed(&mut t, "first line!\r\nsecond\r\nthird");
    let before = t.dump_text();
    t.resize(4, 7);
    t.resize(4, 12);
    assert_eq!(t.dump_text(), before);
}

#[test]
fn reflow_wide_char_moves_to_next_row() {
    let mut t = Terminal::new(3, 6);
    feed(&mut t, "abcd中");
    t.resize(3, 5);
    assert_eq!(row_text(&t, 0), "abcd");
    assert!(t.screen().row(0).unwrap().wrapped());
    assert_eq!(t.screen().cell(1, 0).unwrap().ch, '中');
    assert_eq!(t.screen().cell(1, 0).unwrap().width, 2);
}

#[test]
fn reflow_preserves_cursor_logical_position() {
    let mut t = Terminal::new(3, 8);
    feed(&mut t, "abcdefgh"); // wrapped flag pending at col 7
    feed(&mut t, "ij"); // wraps; cursor now row 1 col 2
    assert_eq!(pos(&t), (1, 2));
    t.resize(3, 5);
    // Logical line "abcdefghij"; cursor stays on the cell after 'j'.
    feed(&mut t, "X");
    let text = t.dump_text();
    assert!(text.starts_with("abcdefghijX"), "{text:?}");
}

#[test]
fn reflow_rewraps_scrollback() {
    let mut t = Terminal::new(2, 8);
    feed(&mut t, "12345678\r\n\r\n\r\n"); // pushes the line into scrollback
    assert!(t.screen().scrollback_len() > 0);
    t.resize(2, 4);
    let joined = t.dump_text();
    assert!(joined.starts_with("12345678\n"), "{joined:?}");
}

#[test]
fn alt_screen_resize_truncates_not_reflows() {
    let mut t = Terminal::new(3, 8);
    feed(&mut t, "\x1b[?1049habcdefgh");
    t.resize(3, 4);
    assert_eq!(row_text(&t, 0), "abcd");
    assert!(!t.screen().row(0).unwrap().wrapped());
    assert_eq!(row_text(&t, 1), "");
}

// --- DECCOLM -------------------------------------------------------------------

#[test]
fn deccolm_requires_mode_40() {
    let mut t = term();
    feed(&mut t, "\x1b[?3h");
    assert_eq!(t.cols(), 10); // ignored without ?40
    feed(&mut t, "\x1b[?40h\x1b[?3h");
    assert_eq!(t.cols(), 132);
    feed(&mut t, "\x1b[?3l");
    assert_eq!(t.cols(), 80);
}

#[test]
fn deccolm_clears_homes_and_resets_margins() {
    let mut t = term();
    feed(&mut t, "\x1b[?40hhello\x1b[2;4r\x1b[?3h");
    assert_eq!(t.cols(), 132);
    assert_eq!(pos(&t), (0, 0));
    assert_eq!(t.scroll_region(), (0, 4));
    assert_eq!(row_text(&t, 0), "");
}

#[test]
fn decncsm_preserves_content() {
    let mut t = term();
    feed(&mut t, "\x1b[?40h\x1b[?95hkeep\x1b[?3h");
    assert_eq!(t.cols(), 132);
    assert!(t.dump_text().contains("keep"));
}

#[test]
fn deccolm_modes_reported_by_decrqm() {
    let mut t = term();
    feed(&mut t, "\x1b[?3$p\x1b[?40$p\x1b[?95$p");
    assert_eq!(responses(&mut t), "\x1b[?3;2$y\x1b[?40;2$y\x1b[?95;2$y");
    feed(&mut t, "\x1b[?40h\x1b[?95h\x1b[?3h");
    feed(&mut t, "\x1b[?3$p\x1b[?40$p\x1b[?95$p");
    assert_eq!(responses(&mut t), "\x1b[?3;1$y\x1b[?40;1$y\x1b[?95;1$y");
}

// --- kitty graphics through the terminal ------------------------------------------

const RGBA1: &str = "AAAAAA=="; // 1x1 RGBA zeros

#[test]
fn graphics_placement_advances_cursor() {
    let mut t = term();
    feed(
        &mut t,
        &format!("\x1b_Ga=T,f=32,s=1,v=1,i=1,c=3,r=2;{RGBA1}\x1b\\"),
    );
    // Cursor lands one cell past the bottom-right corner.
    assert_eq!(pos(&t), (1, 3));
}

#[test]
fn graphics_c1_keeps_cursor() {
    let mut t = term();
    feed(
        &mut t,
        &format!("\x1b_Ga=T,f=32,s=1,v=1,i=1,c=3,r=2,C=1;{RGBA1}\x1b\\"),
    );
    assert_eq!(pos(&t), (0, 0));
}

#[test]
fn graphics_delete_at_cursor() {
    let mut t = term();
    feed(
        &mut t,
        &format!("\x1b_Ga=T,f=32,s=1,v=1,i=1,c=2,r=2,C=1;{RGBA1}\x1b\\"),
    );
    feed(&mut t, "\x1b[2;2H\x1b_Ga=d,d=c\x1b\\");
    assert!(t.placements().is_empty());
    assert!(t.images().contains_key(&1));
    // Uppercase form frees the data of placement-less images.
    feed(&mut t, "\x1b[1;1H\x1b_Ga=T,f=32,s=1,v=1,i=1,c=2,r=2,C=1;");
    feed(
        &mut t,
        &format!("{RGBA1}\x1b\\\x1b[1;1H\x1b_Ga=d,d=C\x1b\\"),
    );
    assert!(t.images().is_empty());
}

#[test]
fn graphics_relative_placement_resolved() {
    let mut t = term();
    feed(&mut t, "\x1b[2;3H");
    feed(
        &mut t,
        &format!("\x1b_Ga=T,f=32,s=1,v=1,i=1,p=1,C=1;{RGBA1}\x1b\\"),
    );
    feed(
        &mut t,
        &format!("\x1b_Ga=T,f=32,s=1,v=1,i=2,p=1,P=1,Q=1,H=2,V=1,C=1;{RGBA1}\x1b\\"),
    );
    let p = t.placements().iter().find(|p| p.image_id == 2).unwrap();
    assert_eq!((p.row, p.col), (2, 4)); // parent (1,2) + (V=1,H=2)
    assert_eq!((p.parent_image, p.parent_placement), (1, 1));
}

#[test]
fn graphics_frames_and_animation_exposed() {
    let mut t = term();
    feed(&mut t, &format!("\x1b_Ga=t,f=32,s=1,v=1,i=4;{RGBA1}\x1b\\"));
    feed(
        &mut t,
        &format!("\x1b_Ga=f,f=32,s=1,v=1,i=4,z=40;{RGBA1}\x1b\\"),
    );
    feed(&mut t, "\x1b_Ga=a,i=4,s=3,v=1\x1b\\");
    t.take_responses();
    assert_eq!(t.image_frames(4).len(), 1);
    assert_eq!(t.image_frames(4)[0].gap_ms, 40);
    let st = t.animation_state(4).unwrap();
    assert_eq!((st.state, st.loops), (3, 1));
}

#[test]
fn graphics_unicode_placeholder_flag() {
    let mut t = term();
    feed(
        &mut t,
        &format!("\x1b_Ga=T,f=32,s=1,v=1,i=1,U=1;{RGBA1}\x1b\\"),
    );
    assert!(t.placements()[0].unicode);
    assert_eq!(pos(&t), (0, 0)); // virtual placements don't move the cursor
}

/// Base64 of a 2x2 RGB PNG (red, green / blue, white), generated with
/// Python3 zlib+struct at test-authoring time.
const PNG_2X2_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEk\
lEQVR4nGP4z8DAAMIM/4EAAB/uBfsL2WiLAAAAAElFTkSuQmCC";

#[test]
fn graphics_png_decoded_via_escape_stream() {
    let mut t = term();
    feed(&mut t, &format!("\x1b_Ga=t,f=100,i=1;{PNG_2X2_B64}\x1b\\"));
    assert!(responses(&mut t).contains("OK"));
    let img = &t.images()[&1];
    assert_eq!((img.width, img.height), (2, 2));
    #[rustfmt::skip]
    assert_eq!(img.data, [
        255, 0, 0, 255,   0, 255, 0, 255,
        0, 0, 255, 255,   255, 255, 255, 255,
    ]);
}

#[test]
fn graphics_bad_png_acks_ebadpng() {
    let mut t = term();
    // "\x89PNG\r\n\x1a\nfake" in base64.
    feed(&mut t, "\x1b_Ga=t,f=100,i=1;iVBORw0KGgpmYWtl\x1b\\");
    assert!(responses(&mut t).contains("EBADPNG"));
    assert!(t.images().is_empty());
}

#[test]
fn graphics_zlib_payload_roundtrip() {
    let mut t = term();
    // zlib.compress of 2x2 RGBA (10,20,30),(40,50,60)/(70,80,90),(100,110,120).
    feed(
        &mut t,
        "\x1b_Ga=t,f=32,s=2,v=2,o=z,i=2;eJzjEpH7r2Fk898tIOp/Sl7FfwAwCAcJ\x1b\\",
    );
    assert_eq!(responses(&mut t), "\x1b_Gi=2;OK\x1b\\");
    #[rustfmt::skip]
    assert_eq!(t.images()[&2].data, [
        10, 20, 30, 255,   40, 50, 60, 255,
        70, 80, 90, 255,   100, 110, 120, 255,
    ]);
}

#[test]
fn graphics_composed_frame_getter() {
    let mut t = term();
    // 2x2 root of (10,20,30,255), then a red 1x1 frame at (1,1).
    feed(
        &mut t,
        "\x1b_Ga=t,f=32,s=2,v=2,i=3;ChQe/woUHv8KFB7/ChQe/w==\x1b\\",
    );
    feed(&mut t, "\x1b_Ga=f,f=32,s=1,v=1,i=3,x=1,y=1;/wAA/w==\x1b\\");
    t.take_responses();
    assert_eq!(t.composed_frame(3, 0).unwrap(), [10, 20, 30, 255].repeat(4));
    #[rustfmt::skip]
    assert_eq!(t.composed_frame(3, 1).unwrap(), [
        10, 20, 30, 255,   10, 20, 30, 255,
        10, 20, 30, 255,   255, 0, 0, 255,
    ]);
    assert!(t.composed_frame(3, 2).is_none());
}

// --- dump_vt additions ---------------------------------------------------------

#[test]
fn dump_vt_replays_kitty_flag_stack() {
    let mut t = term();
    feed(&mut t, "\x1b[>1u\x1b[>15u\x1b[>3u");
    let mut t2 = roundtrip(&t);
    assert_eq!(t2.kitty_flags(), KittyFlags(3));
    // The whole stack survives: pops reveal the same entries.
    feed(&mut t2, "\x1b[<1u");
    assert_eq!(t2.kitty_flags(), KittyFlags(15));
    feed(&mut t2, "\x1b[<1u");
    assert_eq!(t2.kitty_flags(), KittyFlags(1));
}

#[test]
fn dump_vt_carries_synchronized_output() {
    let mut t = term();
    feed(&mut t, "\x1b[?2026h");
    let t2 = roundtrip(&t);
    assert!(t2.synchronized_output());
}

#[test]
fn dump_vt_recreates_custom_tab_stops() {
    let mut t = Terminal::new(5, 30);
    feed(&mut t, "\x1b[3g\x1b[1;5H\x1bH\x1b[1;9H\x1bH\x1b[1;1H");
    let mut t2 = roundtrip(&t);
    feed(&mut t2, "\r\t");
    assert_eq!(pos(&t2), (0, 4));
    feed(&mut t2, "\t");
    assert_eq!(pos(&t2), (0, 8));
    feed(&mut t2, "\t");
    assert_eq!(pos(&t2), (0, 29)); // no further stops
}

#[test]
fn dump_vt_carries_charsets_and_shift() {
    let mut t = term();
    feed(&mut t, "\x1b(A\x1b)0\x0e");
    let mut t2 = roundtrip(&t);
    feed(&mut t2, "q\x0f#x");
    assert_eq!(row_text(&t2, 0), "─£x"); // G1 DEC graphics, G0 UK
}

#[test]
fn dump_vt_carries_deccolm_state() {
    let mut t = term();
    feed(&mut t, "\x1b[?40h\x1b[?95hsticky\x1b[?3h");
    let mut t2 = Terminal::new(t.rows(), t.cols());
    t2.process(&t.dump_vt());
    assert_eq!(t2.cols(), 132);
    feed(&mut t2, "\x1b[?3$p\x1b[?40$p\x1b[?95$p");
    assert_eq!(responses(&mut t2), "\x1b[?3;1$y\x1b[?40;1$y\x1b[?95;1$y");
}

#[test]
fn dump_vt_carries_protected_cells() {
    let mut t = term();
    feed(&mut t, "\x1b[1\"qAB\x1b[0\"qCD");
    let mut t2 = roundtrip(&t);
    assert!(t2.screen().cell(0, 0).unwrap().style.protected);
    assert!(!t2.screen().cell(0, 2).unwrap().style.protected);
    // Behavioral check: selective erase spares the same cells.
    feed(&mut t2, "\x1b[?2J");
    assert_eq!(row_text(&t2, 0), "AB");
}

#[test]
fn dump_vt_alt_screen_keeps_both_kitty_stacks() {
    let mut t = term();
    feed(&mut t, "\x1b[>5u\x1b[?1049h\x1b[>2u");
    let mut t2 = roundtrip(&t);
    assert_eq!(t2.kitty_flags(), KittyFlags(2));
    feed(&mut t2, "\x1b[?1049l");
    assert_eq!(t2.kitty_flags(), KittyFlags(5));
}

// --- OSC 52 selections ------------------------------------------------------------

#[test]
fn osc52_selection_kinds() {
    let mut t = term();
    feed(&mut t, "\x1b]52;c;Y2xpcA==\x07"); // "clip"
    feed(&mut t, "\x1b]52;p;cHJpbQ==\x07"); // "prim"
    feed(&mut t, "\x1b]52;s;c2Vs\x07"); // "sel"
    assert_eq!(t.clipboard(), b"clip");
    assert_eq!(t.selection('c'), b"clip");
    assert_eq!(t.selection('p'), b"prim");
    assert_eq!(t.selection('s'), b"sel");
}

#[test]
fn osc52_combined_kinds_set_all_slots() {
    let mut t = term();
    feed(&mut t, "\x1b]52;cp;Ym90aA==\x07"); // "both"
    assert_eq!(t.selection('c'), b"both");
    assert_eq!(t.selection('p'), b"both");
    assert_eq!(t.selection('s'), b"");
}

#[test]
fn osc52_query_answers_from_right_slot() {
    let mut t = term();
    feed(&mut t, "\x1b]52;p;cHJpbQ==\x07\x1b]52;p;?\x07");
    assert_eq!(responses(&mut t), "\x1b]52;p;cHJpbQ==\x07");
    feed(&mut t, "\x1b]52;;?\x07"); // empty selection defaults to clipboard
    assert_eq!(responses(&mut t), "\x1b]52;c;\x07");
}

// --- color stack ---------------------------------------------------------------------

#[test]
fn color_stack_push_pop() {
    let mut t = term();
    feed(&mut t, "\x1b]4;1;rgb:11/22/33\x07\x1b]10;#aabbcc\x07");
    feed(&mut t, "\x1b[#P"); // XTPUSHCOLORS
    feed(&mut t, "\x1b]4;1;rgb:ff/ff/ff\x07\x1b]10;#000000\x07");
    assert_eq!(t.palette()[1], (0xff, 0xff, 0xff));
    feed(&mut t, "\x1b[#Q"); // XTPOPCOLORS
    assert_eq!(t.palette()[1], (0x11, 0x22, 0x33));
    assert_eq!(t.fg_color(), Some((0xaa, 0xbb, 0xcc)));
}

#[test]
fn color_stack_report() {
    let mut t = term();
    feed(&mut t, "\x1b[#R");
    assert_eq!(responses(&mut t), "\x1b[?0;0#Q");
    feed(&mut t, "\x1b[#P\x1b[#P\x1b[#R");
    assert_eq!(responses(&mut t), "\x1b[?2;2#Q");
    feed(&mut t, "\x1b[#Q\x1b[#R");
    assert_eq!(responses(&mut t), "\x1b[?1;1#Q");
}

#[test]
fn color_stack_pop_empty_is_noop() {
    let mut t = term();
    feed(&mut t, "\x1b]4;1;rgb:11/22/33\x07\x1b[#Q");
    assert_eq!(t.palette()[1], (0x11, 0x22, 0x33));
}

// --- DECSTR vs RIS ---------------------------------------------------------------------

#[test]
fn decstr_xterm_documented_set() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b[?25l\x1b[?6h\x1b[4h\x1b[?1h\x1b=\x1b[2;4r\x1b[31m\x1b[1\"q\x1b[!p",
    );
    assert!(t.cursor().visible); // DECTCEM restored
    assert!(!t.app_cursor_keys());
    assert!(!t.app_keypad());
    assert_eq!(t.scroll_region(), (0, 4));
    feed(&mut t, "\x1b[?6$p\x1b[?7$p\x1b[4$p");
    // origin reset, autowrap reset (xterm DECSTR turns autowrap off),
    // insert mode replaced.
    assert_eq!(responses(&mut t), "\x1b[?6;2$y\x1b[?7;2$y\x1b[4;2$y");
    feed(&mut t, "x");
    let cell = t.screen().cell(0, 1).unwrap().clone();
    assert_eq!(cell.style, posh_term::Style::default());
    assert!(!cell.style.protected);
}

#[test]
fn decstr_does_not_clear_screen_ris_does() {
    let mut t = term();
    feed(&mut t, "keep\x1b[?2004h\x1b[!p");
    assert_eq!(row_text(&t, 0), "keep"); // DECSTR leaves content
    assert!(t.bracketed_paste()); // and unrelated modes
    feed(&mut t, "\x1bc");
    assert_eq!(row_text(&t, 0), ""); // RIS clears
    assert!(!t.bracketed_paste());
}

#[test]
fn decstr_resets_kitty_keyboard() {
    let mut t = term();
    feed(&mut t, "\x1b[>7u\x1b[!p");
    assert_eq!(t.kitty_flags(), KittyFlags(0));
}

// --- DECRQM coverage ----------------------------------------------------------------------

#[test]
fn decrqm_answers_all_tracked_modes() {
    let mut t = term();
    for n in [
        1u16, 3, 5, 6, 7, 8, 9, 12, 25, 40, 47, 95, 1000, 1002, 1003, 1004, 1005, 1006, 1016, 1047,
        1048, 1049, 2004, 2026,
    ] {
        feed(&mut t, &format!("\x1b[?{n}$p"));
        let resp = responses(&mut t);
        let expect_set = format!("\x1b[?{n};1$y");
        let expect_reset = format!("\x1b[?{n};2$y");
        assert!(
            resp == expect_set || resp == expect_reset,
            "mode {n} reported {resp:?}"
        );
    }
    feed(&mut t, "\x1b[?31337$p");
    assert_eq!(responses(&mut t), "\x1b[?31337;0$y");
}

#[test]
fn decrqm_reflects_set_state() {
    let mut t = term();
    feed(&mut t, "\x1b[?1003h\x1b[?1003$p\x1b[?1000$p");
    assert_eq!(responses(&mut t), "\x1b[?1003;1$y\x1b[?1000;2$y");
    feed(&mut t, "\x1b[?1049h\x1b[?1049$p\x1b[?47$p");
    assert_eq!(responses(&mut t), "\x1b[?1049;1$y\x1b[?47;1$y");
}

// --- DECSCA + DECSED/DECSEL ------------------------------------------------------------------

#[test]
fn decsed_skips_protected_cells() {
    let mut t = term();
    feed(&mut t, "\x1b[1\"qAB\x1b[0\"qCD\x1b[1;1H\x1b[?0J");
    assert_eq!(row_text(&t, 0), "AB");
    // Plain ED ignores protection.
    feed(&mut t, "\x1b[1;1H\x1b[0J");
    assert_eq!(row_text(&t, 0), "");
}

#[test]
fn decsel_variants() {
    let mut t = term();
    feed(&mut t, "ab\x1b[1\"qP\x1b[0\"qcd");
    feed(&mut t, "\x1b[1;4H\x1b[?1K"); // selective erase to left
    assert_eq!(row_text(&t, 0), "  P d");
    feed(&mut t, "\x1b[1;1H\x1b[?2K"); // selective erase whole line
    assert_eq!(row_text(&t, 0), "  P");
    feed(&mut t, "\x1b[1;1H\x1b[2K");
    assert_eq!(row_text(&t, 0), "");
}

#[test]
fn decsca_2_unprotects() {
    let mut t = term();
    feed(&mut t, "\x1b[1\"qA\x1b[2\"qB\x1b[1;1H\x1b[?2J");
    assert_eq!(row_text(&t, 0), "A");
}

// --- OSC 66 -------------------------------------------------------------------------------------

#[test]
fn osc66_inserts_text_and_records_payload() {
    let mut t = term();
    feed(&mut t, "\x1b]66;s=2:w=4;Hi\x07after");
    assert_eq!(t.screen().cell(0, 0).unwrap().ch, 'H');
    assert_eq!(t.screen().cell(0, 1).unwrap().ch, 'i');
    // w=4 advances the cursor to the declared width before "after".
    assert_eq!(t.screen().cell(0, 4).unwrap().ch, 'a');
    assert_eq!(t.last_text_size(), Some("s=2:w=4;Hi"));
}

#[test]
fn osc66_without_width_behaves_like_print() {
    let mut t = term();
    feed(&mut t, "\x1b]66;s=2;ab\x07c");
    assert_eq!(row_text(&t, 0), "abc");
}

// --- mouse encoding (public surface) ---------------------------------------------------------

#[test]
fn encode_mouse_public_api() {
    let mut e = MouseEvent::new(MouseButton::Left, MouseEventKind::Press, 4, 9);
    e.mods = Modifiers::SHIFT;
    assert_eq!(
        encode_mouse(e, MouseMode::Normal, MouseProtocol::Sgr).unwrap(),
        b"\x1b[<4;10;5M"
    );
    assert_eq!(encode_mouse(e, MouseMode::None, MouseProtocol::Sgr), None);
}

#[test]
fn encode_mouse_follows_terminal_modes() {
    let mut t = term();
    feed(&mut t, "\x1b[?1002h\x1b[?1006h");
    let drag = MouseEvent::new(MouseButton::Left, MouseEventKind::Motion, 1, 1);
    let bytes = encode_mouse(drag, t.mouse_mode(), t.mouse_protocol()).unwrap();
    assert_eq!(bytes, b"\x1b[<32;2;2M");
    let hover = MouseEvent::new(MouseButton::None, MouseEventKind::Motion, 1, 1);
    assert_eq!(
        encode_mouse(hover, t.mouse_mode(), t.mouse_protocol()),
        None
    );
}
