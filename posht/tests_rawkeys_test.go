package main

import "testing"

func TestCaretRender(t *testing.T) {
	cases := []struct {
		in   []byte
		want string
	}{
		{[]byte{0x0d}, "^M"},
		{[]byte{0x0a}, "^J"},
		{[]byte{0x1b}, "^["},
		{[]byte{0x1b, 0x0d}, "^[^M"},
		{[]byte{0x7f}, "^?"},
		{[]byte("abc"), "abc"},
		{[]byte{0x1b, '[', '1', '3', ';', '2', 'u'}, "^[[13;2u"},
		{[]byte{0xff}, "\\xff"},
	}
	for _, c := range cases {
		if got := caretRender(c.in); got != c.want {
			t.Errorf("caretRender(%v) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestGlossBytes(t *testing.T) {
	cases := []struct {
		in       []byte
		contains string
	}{
		{[]byte{0x0d}, "Shift+Enter is identical"},
		{[]byte{0x0a}, "Ctrl+J"},
		{[]byte{0x1b}, "bare ESC"},
		{[]byte{0x1b, 0x0d}, "Alt+Enter form"},
		{[]byte("\x1b[13;2u"), "Shift+Enter (distinct!"},
		{[]byte("\x1b[13u"), "kitty Enter"},
		{[]byte("\x1b[27u"), "disambiguated Escape"},
		{[]byte("\x1b[Z"), "Shift+Tab"},
	}
	for _, c := range cases {
		got := glossBytes(c.in)
		if !contains(got, c.contains) {
			t.Errorf("glossBytes(%v) = %q, want substring %q", c.in, got, c.contains)
		}
	}
}

func TestParseKittyFlagsReply(t *testing.T) {
	// A kitty-capable terminal answers CSI ? flags u before the DA (CSI ? … c).
	flags, ok := parseKittyFlagsReply([]byte("\x1b[?5u\x1b[?62;c"))
	if !ok || flags != "5" {
		t.Errorf("kitty reply: got (%q, %v), want (\"5\", true)", flags, ok)
	}
	// Flags of 0 (protocol present, no enhancements) report "0", not absent.
	flags, ok = parseKittyFlagsReply([]byte("\x1b[?0u\x1b[?62c"))
	if !ok || flags != "0" {
		t.Errorf("zero-flags reply: got (%q, %v), want (\"0\", true)", flags, ok)
	}
	// A terminal without the protocol answers only the DA (no CSI ? … u).
	if _, ok := parseKittyFlagsReply([]byte("\x1b[?62;c")); ok {
		t.Error("DA-only reply must report the protocol absent")
	}
	// Empty input: absent.
	if _, ok := parseKittyFlagsReply(nil); ok {
		t.Error("empty reply must report the protocol absent")
	}
}

func contains(s, sub string) bool {
	for i := 0; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return true
		}
	}
	return false
}
