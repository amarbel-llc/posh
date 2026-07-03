//! Integration tests exercising the full parser -> dispatch -> screen path
//! through the public API.

use posh_term::{
    Color, Cursor, CursorShape, KittyFlags, MouseMode, MouseProtocol, ScreenSwitch, SemanticMark,
    Style, Terminal, UnderlineStyle,
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

// --- printing, wrapping, wide chars -----------------------------------------

#[test]
fn print_and_cursor() {
    let mut t = term();
    feed(&mut t, "hello");
    assert_eq!(row_text(&t, 0), "hello");
    assert_eq!(pos(&t), (0, 5));
}

#[test]
fn crlf_moves_lines() {
    let mut t = term();
    feed(&mut t, "ab\r\ncd");
    assert_eq!(row_text(&t, 0), "ab");
    assert_eq!(row_text(&t, 1), "cd");
    assert_eq!(pos(&t), (1, 2));
}

#[test]
fn autowrap_and_wrap_flag() {
    let mut t = term();
    feed(&mut t, "0123456789AB");
    assert_eq!(row_text(&t, 0), "0123456789");
    assert_eq!(row_text(&t, 1), "AB");
    assert!(t.screen().row(0).unwrap().wrapped());
    assert!(!t.screen().row(1).unwrap().wrapped());
}

#[test]
fn pending_wrap_semantics() {
    let mut t = term();
    feed(&mut t, "0123456789");
    // Cursor sticks at the last column until the next print.
    assert_eq!(pos(&t), (0, 9));
    // CR clears the pending wrap; no spurious line feed.
    feed(&mut t, "\rX");
    assert_eq!(pos(&t), (0, 1));
    assert_eq!(row_text(&t, 0), "X123456789");
    assert_eq!(row_text(&t, 1), "");
}

#[test]
fn autowrap_off_overwrites_last_column() {
    let mut t = term();
    feed(&mut t, "\x1b[?7l0123456789XY");
    assert_eq!(row_text(&t, 0), "012345678Y");
    assert_eq!(pos(&t), (0, 9));
}

#[test]
fn wide_char_occupies_two_cells() {
    let mut t = term();
    feed(&mut t, "中a");
    let head = t.screen().cell(0, 0).unwrap();
    let tail = t.screen().cell(0, 1).unwrap();
    assert_eq!(head.ch, '中');
    assert_eq!(head.width, 2);
    assert_eq!(tail.width, 0);
    assert_eq!(t.screen().cell(0, 2).unwrap().ch, 'a');
}

#[test]
fn wide_char_wraps_at_edge() {
    let mut t = term();
    feed(&mut t, "012345678中");
    // No room at column 9: blank spacer there, wide char wraps.
    assert_eq!(row_text(&t, 0), "012345678");
    assert_eq!(t.screen().cell(1, 0).unwrap().ch, '中');
    assert!(t.screen().row(0).unwrap().wrapped());
}

#[test]
fn overwriting_wide_head_blanks_spacer() {
    let mut t = term();
    feed(&mut t, "中\rX");
    assert_eq!(t.screen().cell(0, 0).unwrap().ch, 'X');
    assert!(t.screen().cell(0, 1).unwrap().is_blank());
    assert_eq!(t.screen().cell(0, 1).unwrap().width, 1);
}

#[test]
fn combining_mark_attaches_to_previous_cell() {
    let mut t = term();
    feed(&mut t, "e\u{301}x");
    let cell = t.screen().cell(0, 0).unwrap();
    assert_eq!(cell.ch, 'e');
    assert_eq!(cell.extra, vec!['\u{301}']);
    assert_eq!(t.screen().cell(0, 1).unwrap().ch, 'x');
}

#[test]
fn combining_mark_on_pending_wrap_cell() {
    let mut t = term();
    feed(&mut t, "012345678e\u{301}");
    let cell = t.screen().cell(0, 9).unwrap();
    assert_eq!(cell.ch, 'e');
    assert_eq!(cell.extra, vec!['\u{301}']);
}

// --- scrolling and regions ----------------------------------------------------

#[test]
fn scrollback_collects_scrolled_lines() {
    let mut t = term();
    for i in 0..8 {
        feed(&mut t, &format!("line{i}\r\n"));
    }
    assert_eq!(t.screen().scrollback_len(), 4);
    assert_eq!(row_text(&t, 0), "line4");
}

#[test]
fn scroll_region_basics() {
    let mut t = term();
    feed(&mut t, "aa\r\nbb\r\ncc\r\ndd\r\nee");
    // Region rows 2..4 (1-based), cursor homes after DECSTBM.
    feed(&mut t, "\x1b[2;4r");
    assert_eq!(pos(&t), (0, 0));
    assert_eq!(t.scroll_region(), (1, 3));
    // Scroll up inside the region only.
    feed(&mut t, "\x1b[S");
    assert_eq!(row_text(&t, 0), "aa");
    assert_eq!(row_text(&t, 1), "cc");
    assert_eq!(row_text(&t, 2), "dd");
    assert_eq!(row_text(&t, 3), "");
    assert_eq!(row_text(&t, 4), "ee");
    // Nothing went to scrollback (region scroll).
    assert_eq!(t.screen().scrollback_len(), 0);
}

#[test]
fn linefeed_scrolls_only_region() {
    let mut t = term();
    feed(&mut t, "top\r\nA\r\nB\r\nC\r\nbottom");
    feed(&mut t, "\x1b[2;4r\x1b[4;1H\n");
    assert_eq!(row_text(&t, 0), "top");
    assert_eq!(row_text(&t, 1), "B");
    assert_eq!(row_text(&t, 2), "C");
    assert_eq!(row_text(&t, 3), "");
    assert_eq!(row_text(&t, 4), "bottom");
}

#[test]
fn reverse_index_scrolls_down_at_top() {
    let mut t = term();
    feed(&mut t, "one\r\ntwo");
    feed(&mut t, "\x1b[1;1H\x1bM");
    assert_eq!(row_text(&t, 0), "");
    assert_eq!(row_text(&t, 1), "one");
    assert_eq!(row_text(&t, 2), "two");
}

#[test]
fn origin_mode_addressing() {
    let mut t = term();
    feed(&mut t, "\x1b[2;4r\x1b[?6h\x1b[1;1HX");
    // Row 1 in origin mode is the region top (absolute row 2).
    assert_eq!(row_text(&t, 1), "X");
    // Cursor cannot leave the region while origin mode is set.
    feed(&mut t, "\x1b[9;1HY");
    assert_eq!(row_text(&t, 3), "Y");
}

#[test]
fn il_dl_within_region() {
    let mut t = term();
    feed(&mut t, "a\r\nb\r\nc\r\nd\r\ne");
    feed(&mut t, "\x1b[2;1H\x1b[L");
    assert_eq!(row_text(&t, 1), "");
    assert_eq!(row_text(&t, 2), "b");
    assert_eq!(row_text(&t, 4), "d");
    feed(&mut t, "\x1b[2;1H\x1b[M");
    assert_eq!(row_text(&t, 1), "b");
}

// --- erase / edit ------------------------------------------------------------

#[test]
fn erase_line_variants() {
    let mut t = term();
    feed(&mut t, "abcdefghij\x1b[1;5H\x1b[K");
    assert_eq!(row_text(&t, 0), "abcd");
    feed(&mut t, "\x1b[2;1Habcdefghij\x1b[2;5H\x1b[1K");
    assert_eq!(row_text(&t, 1), "     fghij");
    feed(&mut t, "\x1b[2;5H\x1b[2K");
    assert_eq!(row_text(&t, 1), "");
}

#[test]
fn erase_display_variants() {
    let mut t = term();
    feed(&mut t, "11111\r\n22222\r\n33333");
    feed(&mut t, "\x1b[2;3H\x1b[J");
    assert_eq!(row_text(&t, 0), "11111");
    assert_eq!(row_text(&t, 1), "22");
    assert_eq!(row_text(&t, 2), "");
    feed(&mut t, "\x1b[2;3H\x1b[1J");
    assert_eq!(row_text(&t, 0), "");
    assert_eq!(row_text(&t, 1), "");
    feed(&mut t, "x\x1b[2J");
    assert_eq!(row_text(&t, 1), "");
}

#[test]
fn ed3_clears_scrollback() {
    let mut t = term();
    for _ in 0..10 {
        feed(&mut t, "x\r\n");
    }
    assert!(t.screen().scrollback_len() > 0);
    feed(&mut t, "\x1b[3J");
    assert_eq!(t.screen().scrollback_len(), 0);
}

#[test]
fn erase_does_not_bce() {
    // posh#110: posh-term is non-BCE (kitty-parity, ADR 0005). An erase under a
    // non-default bg pen clears to the DEFAULT background, not the pen's, so the
    // remote client never paints a stuck colored region. kitty's terminfo has
    // `bce` unset; a client-cap-aware BCE is tracked in #115.
    let mut t = term();
    feed(&mut t, "\x1b[41m\x1b[2J");
    assert_eq!(t.screen().cell(2, 2).unwrap().style.bg, Color::Default);
    // Nor does any other pen attribute propagate onto the cleared cell.
    assert!(!t.screen().cell(2, 2).unwrap().style.bold);
}

#[test]
fn erase_line_and_insert_line_do_not_bce() {
    // Non-BCE holds across the other blank-creating ops, not just ED (#110): both
    // erase-to-EOL (EL) and insert-line (IL) under a bg pen fill the default bg.
    let mut t = term();
    // EL: red pen, then erase row 0 from the start.
    feed(&mut t, "\x1b[41m\x1b[1;1H\x1b[K");
    assert_eq!(
        t.screen().cell(0, 5).unwrap().style.bg,
        Color::Default,
        "EL under a bg pen"
    );
    // IL: red pen, then insert a blank line at row 2 — the inserted line is default.
    feed(&mut t, "\x1b[41m\x1b[2;1H\x1b[L");
    assert_eq!(
        t.screen().cell(1, 3).unwrap().style.bg,
        Color::Default,
        "IL under a bg pen"
    );
}

#[test]
fn scroll_does_not_bce_the_new_line() {
    // A scroll under a non-default bg pen must leave the newly-exposed line at
    // the DEFAULT background (#100). posh-term is non-BCE for kitty-parity (ADR
    // 0005; the dual of the #42 background-drop check) — carrying the pen bg onto
    // the scrolled-in line would paint a stuck colored line on the remote client.
    let mut t = term();
    feed(&mut t, "top\r\nA\r\nB\r\nC\r\nbottom");
    // Region rows 2..4, cursor at the bottom margin, a red bg pen, then LF to
    // scroll the region up by one.
    feed(&mut t, "\x1b[2;4r\x1b[4;1H\x1b[41m\n");
    // Row 3 (0-based) is the scrolled-in line; its cells must be default bg.
    assert_eq!(
        t.screen().cell(3, 0).unwrap().style.bg,
        Color::Default,
        "scrolled-in line must not carry the pen bg (#100 BCE-on-scroll)"
    );
}

#[test]
fn ich_dch_ech() {
    let mut t = term();
    feed(&mut t, "abcdef\x1b[1;2H\x1b[2@");
    assert_eq!(row_text(&t, 0), "a  bcdef");
    feed(&mut t, "\x1b[1;2H\x1b[2P");
    assert_eq!(row_text(&t, 0), "abcdef");
    feed(&mut t, "\x1b[1;2H\x1b[3X");
    assert_eq!(row_text(&t, 0), "a   ef");
}

#[test]
fn rep_repeats_last_char() {
    let mut t = term();
    feed(&mut t, "ab\x1b[3b");
    assert_eq!(row_text(&t, 0), "abbbb");
}

#[test]
fn insert_mode_shifts() {
    let mut t = term();
    feed(&mut t, "abc\x1b[1;1H\x1b[4hXY");
    assert_eq!(row_text(&t, 0), "XYabc");
    feed(&mut t, "\x1b[4l");
    feed(&mut t, "\x1b[1;1HZ");
    assert_eq!(row_text(&t, 0), "ZYabc");
}

// --- tabs ---------------------------------------------------------------------

#[test]
fn default_tab_stops() {
    let mut t = Terminal::new(5, 30);
    feed(&mut t, "\tx");
    assert_eq!(pos(&t), (0, 9));
    feed(&mut t, "\ty");
    assert_eq!(pos(&t), (0, 17));
}

#[test]
fn hts_tbc_cht_cbt() {
    let mut t = Terminal::new(5, 30);
    // Clear all stops, set custom ones at columns 4 and 7 (0-based 3, 6).
    feed(&mut t, "\x1b[3g\x1b[1;4H\x1bH\x1b[1;7H\x1bH\x1b[1;1H");
    feed(&mut t, "\t");
    assert_eq!(pos(&t), (0, 3));
    feed(&mut t, "\t");
    assert_eq!(pos(&t), (0, 6));
    feed(&mut t, "\t"); // no more stops: go to last column
    assert_eq!(pos(&t), (0, 29));
    feed(&mut t, "\x1b[2Z"); // CBT twice
    assert_eq!(pos(&t), (0, 3));
    feed(&mut t, "\x1b[I");
    assert_eq!(pos(&t), (0, 6));
}

// --- cursor ops -----------------------------------------------------------------

#[test]
fn cursor_movement_clamps() {
    let mut t = term();
    feed(&mut t, "\x1b[99;99H");
    assert_eq!(pos(&t), (4, 9));
    feed(&mut t, "\x1b[99A\x1b[99D");
    assert_eq!(pos(&t), (0, 0));
    feed(&mut t, "\x1b[2B\x1b[3C");
    assert_eq!(pos(&t), (2, 3));
    feed(&mut t, "\x1b[E");
    assert_eq!(pos(&t), (3, 0));
    feed(&mut t, "\x1b[2G\x1b[2d");
    assert_eq!(pos(&t), (1, 1));
}

#[test]
fn decsc_decrc_roundtrip() {
    let mut t = term();
    feed(&mut t, "\x1b[31m\x1b[2;3H\x1b7\x1b[m\x1b[1;1H\x1b8");
    assert_eq!(pos(&t), (1, 2));
    feed(&mut t, "x");
    assert_eq!(t.screen().cell(1, 2).unwrap().style.fg, Color::Indexed(1));
}

#[test]
fn ansi_save_restore_cursor() {
    let mut t = term();
    feed(&mut t, "\x1b[3;4H\x1b[s\x1b[1;1H\x1b[u");
    assert_eq!(pos(&t), (2, 3));
}

#[test]
fn decaln_fills_screen() {
    let mut t = term();
    feed(&mut t, "\x1b#8");
    assert_eq!(row_text(&t, 0), "EEEEEEEEEE");
    assert_eq!(row_text(&t, 4), "EEEEEEEEEE");
    assert_eq!(pos(&t), (0, 0));
}

// --- SGR -------------------------------------------------------------------------

#[test]
fn sgr_basic_attributes() {
    let mut t = term();
    feed(&mut t, "\x1b[1;3;4;5;7;9;31;42mx");
    let s = t.screen().cell(0, 0).unwrap().style;
    assert!(s.bold && s.italic && s.blink && s.inverse && s.strikethrough);
    assert_eq!(s.underline, UnderlineStyle::Single);
    assert_eq!(s.fg, Color::Indexed(1));
    assert_eq!(s.bg, Color::Indexed(2));
    feed(&mut t, "\x1b[my");
    assert_eq!(t.screen().cell(0, 1).unwrap().style, Style::default());
}

#[test]
fn sgr_resets_and_bright() {
    let mut t = term();
    feed(&mut t, "\x1b[1;2m\x1b[22m\x1b[96m\x1b[104mx");
    let s = t.screen().cell(0, 0).unwrap().style;
    assert!(!s.bold && !s.dim);
    assert_eq!(s.fg, Color::Indexed(14));
    assert_eq!(s.bg, Color::Indexed(12));
}

#[test]
fn sgr_256_and_rgb_semicolon_forms() {
    let mut t = term();
    feed(&mut t, "\x1b[38;5;123m\x1b[48;2;10;20;30mx");
    let s = t.screen().cell(0, 0).unwrap().style;
    assert_eq!(s.fg, Color::Indexed(123));
    assert_eq!(s.bg, Color::Rgb(10, 20, 30));
}

#[test]
fn sgr_colon_forms() {
    let mut t = term();
    feed(&mut t, "\x1b[38:5:200m\x1b[48:2::1:2:3mx");
    let s = t.screen().cell(0, 0).unwrap().style;
    assert_eq!(s.fg, Color::Indexed(200));
    assert_eq!(s.bg, Color::Rgb(1, 2, 3));
    feed(&mut t, "\x1b[38:2:9:8:7my");
    assert_eq!(t.screen().cell(0, 1).unwrap().style.fg, Color::Rgb(9, 8, 7));
}

#[test]
fn sgr_params_emits_semicolon_color_form() {
    // The renderer serializes cells back to the client through sgr_params;
    // 256-color and truecolor must use the universally-accepted semicolon
    // form, not the narrowly-supported colon subparameter form. Roundtrip
    // tests can't catch this — the parser accepts both forms.
    use posh_term::sgr_params;
    let with = |f: fn(&mut Style)| {
        let mut s = Style::default();
        f(&mut s);
        sgr_params(&s)
    };
    assert_eq!(with(|s| s.fg = Color::Indexed(1)), "0;31"); // basic
    assert_eq!(with(|s| s.fg = Color::Indexed(12)), "0;94"); // bright
    assert_eq!(with(|s| s.fg = Color::Indexed(123)), "0;38;5;123"); // 256
    assert_eq!(with(|s| s.bg = Color::Rgb(10, 20, 30)), "0;48;2;10;20;30"); // truecolor
    // Underline color (SGR 58) deliberately stays colon-delimited so terminals
    // that don't support the extension skip it as one opaque parameter.
    assert_eq!(with(|s| s.underline_color = Color::Indexed(5)), "0;58:5:5");
    assert_eq!(with(|s| s.underline_color = Color::Rgb(1, 2, 3)), "0;58:2:1:2:3");
}

#[test]
fn sgr_underline_styles_and_color() {
    let mut t = term();
    feed(&mut t, "\x1b[4:3m\x1b[58;2;250:0:0mx");
    let s = t.screen().cell(0, 0).unwrap().style;
    assert_eq!(s.underline, UnderlineStyle::Curly);
    feed(&mut t, "\x1b[4:0m\x1b[21m\x1b[58:5:99my");
    let s = t.screen().cell(0, 1).unwrap().style;
    assert_eq!(s.underline, UnderlineStyle::Double);
    assert_eq!(s.underline_color, Color::Indexed(99));
    feed(&mut t, "\x1b[24;59mz");
    let s = t.screen().cell(0, 2).unwrap().style;
    assert_eq!(s.underline, UnderlineStyle::None);
    assert_eq!(s.underline_color, Color::Default);
}

// --- alt screen --------------------------------------------------------------------

#[test]
fn alt_screen_1049() {
    let mut t = term();
    feed(&mut t, "primary\x1b[2;3H");
    feed(&mut t, "\x1b[?1049h");
    assert!(t.is_alt_screen());
    assert_eq!(row_text(&t, 0), ""); // alt starts cleared
    feed(&mut t, "alt");
    feed(&mut t, "\x1b[?1049l");
    assert!(!t.is_alt_screen());
    assert_eq!(row_text(&t, 0), "primary");
    assert_eq!(pos(&t), (1, 2)); // cursor restored
}

#[test]
fn alt_screen_no_scrollback() {
    let mut t = term();
    feed(&mut t, "\x1b[?1049h");
    for _ in 0..10 {
        feed(&mut t, "x\r\n");
    }
    assert_eq!(t.screen().scrollback_len(), 0);
}

#[test]
fn alt_screen_mode_47_preserves_content() {
    let mut t = term();
    feed(&mut t, "\x1b[?47halt!\x1b[?47l\x1b[?47h");
    assert_eq!(row_text(&t, 0), "alt!");
}

// --- modes and reports ----------------------------------------------------------------

#[test]
fn mode_getters() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2026h\x1b=",
    );
    assert!(t.app_cursor_keys());
    assert!(t.bracketed_paste());
    assert_eq!(t.mouse_mode(), MouseMode::ButtonEvent);
    assert_eq!(t.mouse_protocol(), MouseProtocol::Sgr);
    assert!(t.focus_reporting());
    assert!(t.synchronized_output());
    assert!(t.app_keypad());
    feed(
        &mut t,
        "\x1b[?1l\x1b[?2004l\x1b[?1002l\x1b[?1006l\x1b[?2026l\x1b>",
    );
    assert!(!t.app_cursor_keys());
    assert!(!t.bracketed_paste());
    assert_eq!(t.mouse_mode(), MouseMode::None);
    assert_eq!(t.mouse_protocol(), MouseProtocol::Normal);
    assert!(!t.synchronized_output());
    assert!(!t.app_keypad());
}

#[test]
fn da_and_dsr_reports() {
    let mut t = term();
    feed(&mut t, "\x1b[c");
    assert_eq!(t.take_responses(), b"\x1b[?62;22c");
    feed(&mut t, "\x1b[>c");
    assert_eq!(t.take_responses(), b"\x1b[>1;10;0c");
    feed(&mut t, "\x1b[5n");
    assert_eq!(t.take_responses(), b"\x1b[0n");
    feed(&mut t, "\x1b[3;5H\x1b[6n");
    assert_eq!(t.take_responses(), b"\x1b[3;5R");
    feed(&mut t, "\x1b[?6n");
    assert_eq!(t.take_responses(), b"\x1b[?3;5R");
}

#[test]
fn dsr6_respects_origin_mode() {
    let mut t = term();
    feed(&mut t, "\x1b[2;4r\x1b[?6h\x1b[2;2H\x1b[6n");
    assert_eq!(t.take_responses(), b"\x1b[2;2R");
}

#[test]
fn decrqm_reports() {
    let mut t = term();
    feed(&mut t, "\x1b[?2004$p");
    assert_eq!(t.take_responses(), b"\x1b[?2004;2$y");
    feed(&mut t, "\x1b[?2004h\x1b[?2004$p");
    assert_eq!(t.take_responses(), b"\x1b[?2004;1$y");
    feed(&mut t, "\x1b[?31337$p");
    assert_eq!(t.take_responses(), b"\x1b[?31337;0$y");
    feed(&mut t, "\x1b[4$p");
    assert_eq!(t.take_responses(), b"\x1b[4;2$y");
}

#[test]
fn alternate_scroll_tracked_reported_and_replayed() {
    // DECSET 1007 (alternate scroll): kitty defaults it ON, so the model
    // must track the app's setting for the client to sync/reset. github #28.
    let mut t = term();
    assert!(!t.alternate_scroll());
    feed(&mut t, "\x1b[?1007h");
    assert!(t.alternate_scroll());
    feed(&mut t, "\x1b[?1007$p");
    assert_eq!(t.take_responses(), b"\x1b[?1007;1$y");

    // dump_vt replays the mode (attach replay / remote sync).
    let dump = t.dump_vt();
    let mut r = term();
    r.process(&dump);
    assert!(r.alternate_scroll(), "dump_vt must replay DECSET 1007");

    feed(&mut t, "\x1b[?1007l");
    assert!(!t.alternate_scroll());
    feed(&mut t, "\x1b[?1007$p");
    assert_eq!(t.take_responses(), b"\x1b[?1007;2$y");
}

#[test]
fn xtversion_reports_posh_term() {
    let mut t = term();
    feed(&mut t, "\x1b[>0q");
    let resp = String::from_utf8(t.take_responses()).unwrap();
    assert!(resp.contains("posh-term"), "{resp}");
}

#[test]
fn xtwinops_reports() {
    let mut t = term();
    feed(&mut t, "\x1b[18t");
    assert_eq!(t.take_responses(), b"\x1b[8;5;10t");
    feed(&mut t, "\x1b[14t");
    assert_eq!(t.take_responses(), b"\x1b[4;100;100t");
    feed(&mut t, "\x1b[16t");
    assert_eq!(t.take_responses(), b"\x1b[6;20;10t");
}

#[test]
fn decscusr_and_cursor_visibility() {
    let mut t = term();
    assert_eq!(t.cursor().shape, CursorShape::Block);
    assert!(t.cursor_blink());
    feed(&mut t, "\x1b[4 q");
    assert_eq!(t.cursor().shape, CursorShape::Underline);
    assert!(!t.cursor_blink());
    feed(&mut t, "\x1b[5 q");
    assert_eq!(t.cursor().shape, CursorShape::Bar);
    assert!(t.cursor_blink());
    feed(&mut t, "\x1b[?25l");
    assert!(!t.cursor().visible);
    feed(&mut t, "\x1b[?25h");
    assert!(t.cursor().visible);
}

// --- OSC ------------------------------------------------------------------------------

#[test]
fn osc_titles() {
    let mut t = term();
    feed(&mut t, "\x1b]2;hello world\x07");
    assert_eq!(t.title(), "hello world");
    feed(&mut t, "\x1b]0;via st\x1b\\");
    assert_eq!(t.title(), "via st");
}

#[test]
fn osc4_set_and_query() {
    let mut t = term();
    feed(&mut t, "\x1b]4;1;rgb:12/34/56\x07");
    assert_eq!(t.palette()[1], (0x12, 0x34, 0x56));
    feed(&mut t, "\x1b]4;1;?\x07");
    assert_eq!(t.take_responses(), b"\x1b]4;1;rgb:1212/3434/5656\x07");
    feed(&mut t, "\x1b]104;1\x07");
    assert_eq!(t.palette()[1], (205, 0, 0));
}

#[test]
fn osc_dynamic_colors() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b]10;#aabbcc\x07\x1b]11;rgb:01/02/03\x07\x1b]12;#fff\x07",
    );
    assert_eq!(t.fg_color(), Some((0xaa, 0xbb, 0xcc)));
    assert_eq!(t.bg_color(), Some((1, 2, 3)));
    assert_eq!(t.cursor_color(), Some((255, 255, 255)));
    feed(&mut t, "\x1b]11;?\x1b\\");
    assert_eq!(t.take_responses(), b"\x1b]11;rgb:0101/0202/0303\x1b\\");
    feed(&mut t, "\x1b]110\x07\x1b]111\x07\x1b]112\x07");
    assert_eq!(t.fg_color(), None);
    assert_eq!(t.bg_color(), None);
}

#[test]
fn osc7_pwd() {
    let mut t = term();
    feed(&mut t, "\x1b]7;file://myhost/home/user/my%20dir\x07");
    assert_eq!(t.pwd(), "/home/user/my dir");
}

#[test]
fn osc8_hyperlinks() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b]8;id=x;https://example.com\x07link\x1b]8;;\x07plain",
    );
    let cell = t.screen().cell(0, 0).unwrap();
    assert_ne!(cell.hyperlink, 0);
    assert_eq!(t.hyperlink(cell.hyperlink), Some("https://example.com"));
    let plain = t.screen().cell(0, 4).unwrap();
    assert_eq!(plain.hyperlink, 0);
}

#[test]
fn osc52_clipboard() {
    let mut t = term();
    feed(&mut t, "\x1b]52;c;aGVsbG8=\x07"); // "hello"
    assert_eq!(t.clipboard(), b"hello");
    feed(&mut t, "\x1b]52;c;?\x07");
    assert_eq!(t.take_responses(), b"\x1b]52;c;aGVsbG8=\x07");
}

#[test]
fn osc52_writes_bump_sequence() {
    // The remote client forwards copies on seq change, so duplicate
    // payloads must still advance the counter; queries must not. github #27.
    let mut t = term();
    assert_eq!(t.clipboard_seq(), 0);
    feed(&mut t, "\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(t.clipboard_seq(), 1);
    assert_eq!(t.clipboard_kinds(), "c");
    feed(&mut t, "\x1b]52;c;aGVsbG8=\x07"); // identical copy
    assert_eq!(t.clipboard_seq(), 2);
    feed(&mut t, "\x1b]52;p;eWFuaw==\x07"); // primary slot
    assert_eq!(t.clipboard_seq(), 3);
    assert_eq!(t.clipboard_kinds(), "p");
    feed(&mut t, "\x1b]52;c;?\x07"); // query: no write
    let _ = t.take_responses();
    assert_eq!(t.clipboard_seq(), 3);
}

#[test]
fn osc133_prompt_marks() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07",
    );
    assert_eq!(t.row_mark(0), Some(SemanticMark::InputStart)); // B overwrote A
    assert_eq!(t.row_mark(1), Some(SemanticMark::OutputStart));
    assert_eq!(t.row_mark(2), Some(SemanticMark::CommandEnd));
}

#[test]
fn osc9_and_99_notifications_and_pointer() {
    let mut t = term();
    feed(&mut t, "\x1b]9;build done\x07");
    assert_eq!(t.last_notification(), Some("build done"));
    feed(&mut t, "\x1b]99;i=1:d=0;hello\x07");
    assert_eq!(t.last_notification(), Some("hello"));
    feed(&mut t, "\x1b]22;pointer\x07");
    assert_eq!(t.pointer_shape(), "pointer");
}

// --- DCS -------------------------------------------------------------------------------

#[test]
fn decrqss_reports() {
    let mut t = term();
    feed(&mut t, "\x1b[1;31m\x1bP$qm\x1b\\");
    assert_eq!(t.take_responses(), b"\x1bP1$r0;1;31m\x1b\\");
    feed(&mut t, "\x1b[3 q\x1bP$q q\x1b\\");
    assert_eq!(t.take_responses(), b"\x1bP1$r3 q\x1b\\");
    feed(&mut t, "\x1b[2;4r\x1bP$qr\x1b\\");
    assert_eq!(t.take_responses(), b"\x1bP1$r2;4r\x1b\\");
    feed(&mut t, "\x1bP$qz\x1b\\");
    assert_eq!(t.take_responses(), b"\x1bP0$r\x1b\\");
}

#[test]
fn xtgettcap_known_and_unknown() {
    let mut t = term();
    // "colors" hex-encoded.
    feed(&mut t, "\x1bP+q636f6c6f7273\x1b\\");
    assert_eq!(t.take_responses(), b"\x1bP1+r636F6C6F7273=323536\x1b\\");
    // "RGB" boolean capability.
    feed(&mut t, "\x1bP+q524742\x1b\\");
    assert_eq!(t.take_responses(), b"\x1bP1+r524742\x1b\\");
    // Unknown capability "zz" (7a7a).
    feed(&mut t, "\x1bP+q7a7a\x1b\\");
    assert_eq!(t.take_responses(), b"\x1bP0+r7A7A\x1b\\");
}

// --- kitty keyboard (terminal side) ---------------------------------------------------

#[test]
fn kitty_keyboard_stack() {
    let mut t = term();
    assert_eq!(t.kitty_flags(), KittyFlags(0));
    feed(&mut t, "\x1b[>1u");
    assert_eq!(t.kitty_flags(), KittyFlags(1));
    feed(&mut t, "\x1b[>15u");
    assert_eq!(t.kitty_flags(), KittyFlags(15));
    feed(&mut t, "\x1b[?u");
    assert_eq!(t.take_responses(), b"\x1b[?15u");
    feed(&mut t, "\x1b[<1u");
    assert_eq!(t.kitty_flags(), KittyFlags(1));
    feed(&mut t, "\x1b[=5;1u");
    assert_eq!(t.kitty_flags(), KittyFlags(5));
}

#[test]
fn kitty_keyboard_separate_alt_stack() {
    let mut t = term();
    feed(&mut t, "\x1b[>3u\x1b[?1049h");
    assert_eq!(t.kitty_flags(), KittyFlags(0)); // alt screen has its own stack
    feed(&mut t, "\x1b[>1u");
    assert_eq!(t.kitty_flags(), KittyFlags(1));
    feed(&mut t, "\x1b[?1049l");
    assert_eq!(t.kitty_flags(), KittyFlags(3));
}

// --- kitty graphics through the terminal ------------------------------------------------

#[test]
fn kitty_graphics_transmit_via_apc() {
    let mut t = term();
    let pixels = [0u8; 4];
    let payload = {
        // Local base64 for the test (1x1 RGBA zeros).
        assert_eq!(pixels.len(), 4);
        "AAAAAA=="
    };
    feed(
        &mut t,
        &format!("\x1b_Ga=T,f=32,s=1,v=1,i=9;{payload}\x1b\\"),
    );
    assert_eq!(t.take_responses(), b"\x1b_Gi=9;OK\x1b\\");
    assert_eq!(t.images().len(), 1);
    assert_eq!(t.placements().len(), 1);
    assert_eq!(t.images()[&9].width, 1);
}

// --- charsets, C1, reset -----------------------------------------------------------------

#[test]
fn dec_special_graphics() {
    let mut t = term();
    feed(&mut t, "\x1b(0lqk\x1b(B-");
    assert_eq!(row_text(&t, 0), "┌─┐-");
}

#[test]
fn shift_out_uses_g1() {
    let mut t = term();
    feed(&mut t, "\x1b)0\x0eq\x0fq");
    assert_eq!(row_text(&t, 0), "─q");
}

#[test]
fn c1_eight_bit_controls() {
    let mut t = term();
    t.process(b"ab\x85cd"); // 8-bit NEL
    assert_eq!(row_text(&t, 0), "ab");
    assert_eq!(row_text(&t, 1), "cd");
    t.process(b"\xc2\x8d"); // UTF-8 encoded RI
    assert_eq!(pos(&t), (0, 2));
}

#[test]
fn nel_ind() {
    let mut t = term();
    feed(&mut t, "ab\x1bEcd\x1bDe");
    assert_eq!(row_text(&t, 0), "ab");
    assert_eq!(row_text(&t, 1), "cd");
    assert_eq!(row_text(&t, 2), "  e");
}

#[test]
fn lnm_makes_lf_do_cr() {
    let mut t = term();
    feed(&mut t, "\x1b[20hab\ncd");
    assert_eq!(row_text(&t, 1), "cd");
    feed(&mut t, "\x1b[20l");
}

#[test]
fn bel_counts() {
    let mut t = term();
    feed(&mut t, "a\x07b\x07\x07");
    assert_eq!(t.bell_count(), 3);
}

#[test]
fn ris_full_reset() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b[31mhi\x1b[?2004h\x1b[2;4r\x1b]2;t\x07\x1b[>1u\x1bc",
    );
    assert_eq!(row_text(&t, 0), "");
    assert!(!t.bracketed_paste());
    assert_eq!(t.scroll_region(), (0, 4));
    assert_eq!(t.kitty_flags(), KittyFlags(0));
    feed(&mut t, "x");
    assert_eq!(t.screen().cell(0, 0).unwrap().style, Style::default());
}

#[test]
fn decstr_soft_reset() {
    let mut t = term();
    feed(&mut t, "\x1b[?25l\x1b[?6h\x1b[2;4r\x1b[4h\x1b[31m\x1b[!p");
    assert!(t.cursor().visible);
    assert_eq!(t.scroll_region(), (0, 4));
    feed(&mut t, "x");
    assert_eq!(t.screen().cell(0, 0).unwrap().style, Style::default());
}

// --- resize -------------------------------------------------------------------------------

#[test]
fn resize_reflows_primary() {
    let mut t = term();
    feed(&mut t, "hello\r\nworld");
    t.resize(3, 4);
    assert_eq!((t.rows(), t.cols()), (3, 4));
    // Logical lines rewrapped; the head of the first line scrolled out.
    assert_eq!(t.screen().scrollback_row(0).unwrap().text(true), "hell");
    assert_eq!(row_text(&t, 0), "o");
    assert_eq!(row_text(&t, 1), "worl");
    assert_eq!(row_text(&t, 2), "d");
    assert_eq!(pos(&t), (2, 1));
    // Widening rejoins the wrapped lines and restores the original text.
    t.resize(5, 10);
    assert_eq!(row_text(&t, 0), "hello");
    assert_eq!(row_text(&t, 1), "world");
    assert_eq!(pos(&t), (1, 5));
    assert_eq!(t.screen().scrollback_len(), 0);
}

#[test]
fn resize_shrink_keeps_cursor_line_via_scrollback() {
    let mut t = term();
    feed(&mut t, "1\r\n2\r\n3\r\n4\r\n5");
    t.resize(2, 10);
    assert_eq!(row_text(&t, 1), "5");
    assert_eq!(pos(&t), (1, 1));
    assert_eq!(t.screen().scrollback_len(), 3);
}

// --- generation ---------------------------------------------------------------------------

#[test]
fn generation_bumps_on_visible_changes() {
    let mut t = term();
    let g0 = t.generation();
    feed(&mut t, "x");
    assert!(t.generation() > g0);
    let g1 = t.generation();
    feed(&mut t, "\x1b[5n"); // pure query: no visible change
    assert_eq!(t.generation(), g1);
}

// --- dumps ----------------------------------------------------------------------------------

#[test]
fn dump_text_includes_scrollback_and_trims() {
    let mut t = term();
    for i in 0..7 {
        feed(&mut t, &format!("l{i}  \r\n"));
    }
    let text = t.dump_text();
    assert!(text.starts_with("l0\nl1\n"));
    assert!(text.contains("l6\n"));
    assert!(!text.contains("  \n"));
}

#[test]
fn dump_text_joins_wrapped_lines() {
    let mut t = term();
    feed(&mut t, "0123456789abc");
    let text = t.dump_text();
    assert!(text.starts_with("0123456789abc\n"), "{text:?}");
}

fn roundtrip(t: &Terminal) -> Terminal {
    let mut t2 = Terminal::new(t.rows(), t.cols());
    t2.process(&t.dump_vt());
    t2
}

fn assert_grids_equal(a: &Terminal, b: &Terminal) {
    assert_eq!(a.rows(), b.rows());
    assert_eq!(a.cols(), b.cols());
    for r in 0..a.rows() {
        for c in 0..a.cols() {
            let ca = a.screen().cell(r, c).unwrap();
            let cb = b.screen().cell(r, c).unwrap();
            assert_eq!(ca.ch, cb.ch, "ch mismatch at {r},{c}");
            assert_eq!(ca.width, cb.width, "width mismatch at {r},{c}");
            assert_eq!(ca.style, cb.style, "style mismatch at {r},{c}");
            assert_eq!(ca.extra, cb.extra, "extra mismatch at {r},{c}");
            assert_eq!(
                a.hyperlink(ca.hyperlink),
                b.hyperlink(cb.hyperlink),
                "hyperlink mismatch at {r},{c}"
            );
        }
    }
}

fn assert_state_equal(a: &Terminal, b: &Terminal) {
    assert_grids_equal(a, b);
    assert_eq!(a.cursor(), b.cursor());
    assert_eq!(a.title(), b.title());
    assert_eq!(a.scroll_region(), b.scroll_region());
    assert_eq!(a.is_alt_screen(), b.is_alt_screen());
    assert_eq!(a.bracketed_paste(), b.bracketed_paste());
    assert_eq!(a.app_cursor_keys(), b.app_cursor_keys());
    assert_eq!(a.mouse_mode(), b.mouse_mode());
    assert_eq!(a.mouse_protocol(), b.mouse_protocol());
    assert_eq!(a.kitty_flags(), b.kitty_flags());
}

#[test]
fn dump_visible_rows_round_trips_each_row() {
    // Per-row serialization (FDR 0005 scroll-view): replaying the rows onto a
    // fresh same-size terminal reproduces the visible grid.
    let mut t = term();
    feed(&mut t, "abc\r\ndef\r\nghi");
    let rows = t.dump_visible_rows();
    assert_eq!(rows.len(), 5, "one entry per visible grid row");

    let mut v = term();
    for (i, row) in rows.iter().enumerate() {
        // Strip the final row's CRLF so it does not scroll the grid.
        if i + 1 == rows.len() {
            v.process(row.strip_suffix(b"\r\n").unwrap_or(row));
        } else {
            v.process(row);
        }
    }
    for r in 0..5 {
        assert_eq!(row_text(&v, r), row_text(&t, r), "row {r}");
    }
}

#[test]
fn dump_vt_roundtrip_simple() {
    let mut t = term();
    feed(&mut t, "hello\r\nworld\x1b[2;3H");
    assert_state_equal(&t, &roundtrip(&t));
}

#[test]
fn dump_vt_roundtrip_styles() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b[1;31mred\x1b[0;4:3;58:5:2m curly \x1b[38;2;1;2;3mrgb\x1b[m.",
    );
    feed(&mut t, "\r\n\x1b[7;100minv\x1b[m");
    assert_state_equal(&t, &roundtrip(&t));
}

#[test]
fn dump_vt_roundtrip_wide_and_combining() {
    let mut t = term();
    feed(&mut t, "中文 e\u{301} ok\r\nx");
    assert_state_equal(&t, &roundtrip(&t));
}

#[test]
fn dump_vt_roundtrip_modes_and_title() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b]2;my title\x07\x1b[?1h\x1b[?2004h\x1b[?1003h\x1b[?1016h\x1b[>13u",
    );
    feed(&mut t, "\x1b[3 q\x1b[?25l\x1b[2;4r");
    let t2 = roundtrip(&t);
    assert_state_equal(&t, &t2);
    assert_eq!(t2.title(), "my title");
    assert!(!t2.cursor().visible);
    assert_eq!(t2.cursor().shape, CursorShape::Underline);
}

#[test]
fn dump_vt_roundtrip_alt_screen() {
    let mut t = term();
    feed(&mut t, "primary stuff\x1b[?1049halt line\x1b[2;2H");
    let t2 = roundtrip(&t);
    assert_state_equal(&t, &t2);
    // Leaving the alt screen must reveal the same primary content.
    let (mut t3, mut t4) = (t, t2);
    feed(&mut t3, "\x1b[?1049l");
    feed(&mut t4, "\x1b[?1049l");
    assert_grids_equal(&t3, &t4);
    assert_eq!(pos(&t3), pos(&t4));
}

// --- title (OSC 0/1/2) dump round-trip ---------------------------------------
// The roaming wedge (#90) correlates strongly with window-title / frontmost-
// process changes (OSC 0/2). The transport diffs successive `dump_vt` outputs;
// if a title makes `dump_vt` non-idempotent (re-parsing a dump and re-dumping
// changes the bytes), the client's reconstructed base length can diverge from
// the server's — exactly the `apply_diff` short-base condition that wedges.

/// `dump_vt` must be idempotent: parsing a dump into a fresh terminal and
/// re-dumping yields byte-identical output. A length change here is the
/// apply-stall origin we are hunting.
fn assert_dump_vt_idempotent(t: &Terminal) {
    let once = t.dump_vt();
    let mut t2 = Terminal::new(t.rows(), t.cols());
    t2.process(&once);
    let twice = t2.dump_vt();
    assert_eq!(
        once,
        twice,
        "dump_vt not idempotent:\n once={:?}\n twice={:?}",
        String::from_utf8_lossy(&once),
        String::from_utf8_lossy(&twice),
    );
}

#[test]
fn dump_vt_title_variants_round_trip() {
    let cases: &[&[u8]] = &[
        b"\x1b]2;short\x07",
        b"\x1b]0;both window and icon\x07",
        b"\x1b]1;icon only\x07",
        b"\x1b]2;\x07",                             // explicit empty title
        b"\x1b]2;with;semicolons;in;it\x07",        // semicolons are title content
        b"\x1b]2;unicode \xc3\xa9\xe2\x9c\x94\x07", // "é ✔" (valid UTF-8)
        b"\x1b]2;tab\there\x07",                    // embedded C0 (tab) is content
        b"\x1b]2;latin1-\xff-byte\x07",             // invalid UTF-8 -> from_utf8_lossy
    ];
    for raw in cases {
        let mut t = term();
        t.process(raw);
        let t2 = roundtrip(&t);
        assert_eq!(
            t.title(),
            t2.title(),
            "title mismatch for {:?}",
            String::from_utf8_lossy(raw),
        );
        assert_dump_vt_idempotent(&t);
    }
}

#[test]
fn dump_vt_idempotent_across_title_changes() {
    // Rapid OSC title updates (the frontmost-process-change scenario): every
    // intermediate state must leave dump_vt idempotent.
    let mut t = term();
    for title in ["one", "second longer title", "x", "", "back to normal"] {
        t.process(format!("\x1b]2;{title}\x07").as_bytes());
        assert_dump_vt_idempotent(&t);
    }
}

#[test]
fn process_title_split_byte_by_byte_matches_whole() {
    // ADR-0003: a title OSC split across read boundaries must reassemble to the
    // same state as delivered whole (a frontmost-process change landing
    // mid-read). dump_vt of the two must be byte-identical.
    let stream: &[u8] = b"prompt$ \x1b]0;user@host: ~/work\x07ready\r\nmore \xc3\xa9 text";
    let mut whole = term();
    whole.process(stream);

    let mut split = term();
    for b in stream {
        split.process(&[*b]);
    }
    assert_eq!(whole.title(), split.title(), "title differs whole vs split");
    assert_eq!(
        whole.dump_vt(),
        split.dump_vt(),
        "dump_vt differs whole vs split",
    );
}

#[test]
fn dump_vt_roundtrip_scrollback() {
    let mut t = term();
    for i in 0..12 {
        feed(&mut t, &format!("scroll line {i}\r\n"));
    }
    feed(&mut t, "end");
    let t2 = roundtrip(&t);
    assert_state_equal(&t, &t2);
    assert_eq!(t.screen().scrollback_len(), t2.screen().scrollback_len());
    assert_eq!(t.dump_text(), t2.dump_text());
}

#[test]
fn dump_vt_roundtrip_wrapped_scrollback() {
    let mut t = term();
    feed(&mut t, "0123456789abcdef\r\n"); // wrapped logical line
    for i in 0..6 {
        feed(&mut t, &format!("{i}\r\n"));
    }
    let t2 = roundtrip(&t);
    assert_eq!(t.dump_text(), t2.dump_text());
    assert_state_equal(&t, &t2);
}

#[test]
fn dump_vt_roundtrip_pending_wrap() {
    let mut t = term();
    feed(&mut t, "0123456789"); // leaves pending wrap
    let t2 = roundtrip(&t);
    assert_state_equal(&t, &t2);
    // Behavioral check: the next print must wrap on both.
    let (mut t3, mut t4) = (t, t2);
    feed(&mut t3, "X");
    feed(&mut t4, "X");
    assert_grids_equal(&t3, &t4);
    assert_eq!(pos(&t3), pos(&t4));
}

#[test]
fn dump_vt_roundtrip_hyperlinks_and_marks() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b]133;A\x07$ \x1b]8;;http://a\x07link\x1b]8;;\x07 done",
    );
    let t2 = roundtrip(&t);
    assert_state_equal(&t, &t2);
    assert_eq!(t2.row_mark(0), Some(SemanticMark::PromptStart));
}

#[test]
fn dump_vt_roundtrip_colors() {
    let mut t = term();
    feed(
        &mut t,
        "\x1b]4;5;rgb:01/02/03\x07\x1b]10;#abcdef\x07\x1b]11;#123456\x07",
    );
    let t2 = roundtrip(&t);
    assert_eq!(t2.palette()[5], (1, 2, 3));
    assert_eq!(t2.fg_color(), Some((0xab, 0xcd, 0xef)));
    assert_eq!(t2.bg_color(), Some((0x12, 0x34, 0x56)));
}

#[test]
fn dump_vt_roundtrip_origin_mode() {
    let mut t = term();
    feed(&mut t, "\x1b[2;4r\x1b[?6h\x1b[2;3HX");
    let t2 = roundtrip(&t);
    assert_state_equal(&t, &t2);
    // Both must interpret subsequent origin-relative addressing the same.
    let (mut t3, mut t4) = (t, t2);
    feed(&mut t3, "\x1b[1;1HY");
    feed(&mut t4, "\x1b[1;1HY");
    assert_grids_equal(&t3, &t4);
}

#[test]
fn dump_vt_roundtrip_full_session() {
    // A realistically sized terminal with a fish-like mixed workload.
    let mut t = Terminal::new(24, 80);
    feed(
        &mut t,
        "\x1b]2;fish /home/user\x07\x1b]7;file:///home/user\x07",
    );
    for i in 0..40 {
        feed(
            &mut t,
            &format!("\x1b]133;A\x07\x1b[1;32m~>\x1b[m \x1b]133;B\x07echo {i}\r\n"),
        );
        feed(&mut t, &format!("\x1b]133;C\x07{i}\r\n\x1b]133;D;0\x07"));
    }
    feed(
        &mut t,
        "\x1b[4:3;58:5:1munder\x1b[m wide 中文 e\u{301}combo ",
    );
    feed(
        &mut t,
        "\x1b]8;id=k;https://fishshell.com\x07docs\x1b]8;;\x07",
    );
    feed(&mut t, "\x1b[?2004h\x1b[>1u\x1b[2 q\x1b[?1h");
    let mut t2 = Terminal::new(24, 80);
    t2.process(&t.dump_vt());
    assert_state_equal(&t, &t2);
    assert_eq!(t.dump_text(), t2.dump_text());
    assert_eq!(t2.pwd(), "/home/user");
}

// --- screen-switch detection & single-screen dumps (terminal takeover) ------

#[test]
fn take_screen_switch_reports_alt_and_reset() {
    let mut t = term();
    assert_eq!(t.take_screen_switch(), None);
    feed(&mut t, "\x1b[?1049h");
    assert_eq!(t.take_screen_switch(), Some(ScreenSwitch::Alt));
    assert_eq!(t.take_screen_switch(), None); // taken
    // Redundant re-entry still reports: the raw bytes must be withheld
    // from a single-screen target either way.
    feed(&mut t, "\x1b[?1049h");
    assert_eq!(t.take_screen_switch(), Some(ScreenSwitch::Alt));
    feed(&mut t, "\x1b[?1047l");
    assert_eq!(t.take_screen_switch(), Some(ScreenSwitch::Alt));
    feed(&mut t, "\x1b[?47h");
    assert_eq!(t.take_screen_switch(), Some(ScreenSwitch::Alt));
    feed(&mut t, "\x1bc");
    assert_eq!(t.take_screen_switch(), Some(ScreenSwitch::Reset));
}

#[test]
fn screen_switch_not_reported_for_queries_or_other_modes() {
    let mut t = term();
    // DECRQM mentions 1049 but switches nothing.
    feed(&mut t, "\x1b[?1049$p");
    assert_eq!(t.take_screen_switch(), None);
    feed(&mut t, "\x1b[?2004h\x1b[?1000h\x1b[!p");
    assert_eq!(t.take_screen_switch(), None);
}

#[test]
fn mid_escape_tracks_esc_and_csi_but_not_strings() {
    let mut t = term();
    t.process(b"\x1b");
    assert!(t.mid_escape());
    t.process(b"[?10");
    assert!(t.mid_escape());
    t.process(b"49h");
    assert!(!t.mid_escape());
    // String payloads are not held: only their introducing ESC is.
    t.process(b"\x1b]2;tit");
    assert!(!t.mid_escape());
    t.process(b"le\x07");
    assert!(!t.mid_escape());
}

#[test]
fn dump_vt_flat_never_switches_screens() {
    let mut t = term();
    feed(&mut t, "primary\x1b[?1049halt text\x1b[2;2H");
    let flat = t.dump_vt_flat();
    let s = String::from_utf8(flat.clone()).unwrap();
    assert!(!s.contains("\x1b[?1049"), "flat dump must not switch: {s:?}");
    // A fresh terminal fed the flat dump shows the active (alt) grid and
    // cursor on its single screen.
    let mut v = Terminal::new(t.rows(), t.cols());
    v.process(&flat);
    assert_grids_equal(&t, &v);
    assert_eq!(pos(&t), pos(&v));
    assert!(!v.is_alt_screen());
}

#[test]
fn dump_vt_flat_draws_grid_without_scrollback_replay() {
    let mut t = term();
    for i in 0..9 {
        feed(&mut t, &format!("line {i}\r\n"));
    }
    let mut v = Terminal::new(t.rows(), t.cols());
    v.process(&t.dump_vt_flat());
    assert_grids_equal(&t, &v);
    assert_eq!(pos(&t), pos(&v));
    assert_eq!(v.screen().scrollback_len(), 0);
}

#[test]
fn dump_screen_switch_repaints_active_screen_in_place() {
    // `outer` plays the attached client's real terminal: it mirrored the
    // session byte for byte up to the switch, which it receives as the
    // substitute repaint instead of the raw 1049.
    let mut session = term();
    let mut outer = term();
    let before = "primary one\r\ntwo\x1b[1;3H";
    feed(&mut session, before);
    feed(&mut outer, before);

    feed(&mut session, "\x1b[?1049h");
    outer.process(&session.dump_screen_switch());
    // Post-switch drawing passes through raw again.
    feed(&mut session, "ALT");
    feed(&mut outer, "ALT");
    assert_grids_equal(&session, &outer);
    assert_eq!(pos(&session), pos(&outer));
    assert!(!outer.is_alt_screen(), "outer must never switch buffers");

    // Leaving the alt screen restores the primary content and the
    // 1049-saved cursor, again without the outer switching.
    feed(&mut session, "\x1b[?1049l");
    outer.process(&session.dump_screen_switch());
    assert_grids_equal(&session, &outer);
    assert_eq!(pos(&session), pos(&outer));
    assert!(!outer.is_alt_screen());
}

#[test]
fn dump_screen_switch_restores_region_and_modes_it_normalizes() {
    let mut session = Terminal::new(6, 20);
    let mut outer = Terminal::new(6, 20);
    // Scroll region + origin mode + autowrap off, kept in sync raw.
    let before = "\x1b[2;4r\x1b[?6h\x1b[?7lcontent";
    feed(&mut session, before);
    feed(&mut outer, before);
    feed(&mut session, "\x1b[?1049h");
    outer.process(&session.dump_screen_switch());
    assert_eq!(session.scroll_region(), outer.scroll_region());
    // Both must interpret origin-relative addressing identically after.
    feed(&mut session, "\x1b[1;1HY");
    feed(&mut outer, "\x1b[1;1HY");
    assert_grids_equal(&session, &outer);
    assert_eq!(pos(&session), pos(&outer));
}

// --- misc public surface -----------------------------------------------------------------

#[test]
fn cursor_struct_shape() {
    let t = term();
    let c: Cursor = t.cursor();
    assert_eq!((c.row, c.col), (0, 0));
    assert!(c.visible);
    assert_eq!(c.shape, CursorShape::Block);
}

#[test]
fn unknown_sequences_are_ignored() {
    let mut t = term();
    feed(&mut t, "\x1b[?9999h\x1b[<5;6;7z\x1b]7777;x\x07ok");
    assert_eq!(row_text(&t, 0), "ok");
}
