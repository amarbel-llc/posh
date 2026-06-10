package main

import tea "github.com/charmbracelet/bubbletea"

type Verdict int

const (
	Pending Verdict = iota
	Pass
	Fail
	Skipped
)

func (v Verdict) String() string {
	switch v {
	case Pass:
		return "pass"
	case Fail:
		return "FAIL"
	case Skipped:
		return "skipped"
	}
	return "not run"
}

// TestModel is one live test screen. The root model routes messages to the
// current test and renders its View below the chrome.
type TestModel interface {
	Init() tea.Cmd
	Update(tea.Msg) (TestModel, tea.Cmd)
	View(width int) string
}

// capturer is implemented by tests that need raw keystrokes (the keyboard
// and paste tests). While Capturing() is true, the y/n/s verdict keys are
// forwarded to the test instead of being consumed by the chrome.
type capturer interface{ Capturing() bool }

// cleaner is implemented by tests that change terminal state (mouse
// reporting, cursor shape, window title) and must undo it on the way out.
type cleaner interface{ Cleanup() tea.Cmd }

type Test struct {
	ID, Title string
	Desc      string // one-liner for the checklist
	Notes     string // caveats / known posh gaps, shown during the test
	Selected  bool
	Verdict   Verdict
	New       func() TestModel
}

// registry returns every test, in run order, all selected by default.
func registry() []*Test {
	tests := []*Test{
		{
			ID: "colors16", Title: "16 ANSI colors",
			Desc: "standard + bright foreground and background (SGR 30-37/90-97/40-47/100-107)",
			New:  func() TestModel { return staticModel{render: colors16View} },
		},
		{
			ID: "colors256", Title: "256-color palette",
			Desc: "system row, 6x6x6 cube, grayscale ramp (SGR 38;5 / 48;5)",
			New:  func() TestModel { return staticModel{render: colors256View} },
		},
		{
			ID: "truecolor", Title: "24-bit truecolor",
			Desc: "RGB gradients, semicolon and colon SGR forms (38;2 / 38:2::)",
			New:  func() TestModel { return staticModel{render: truecolorView} },
		},
		{
			ID: "attrs", Title: "text attributes",
			Desc: "bold, dim, italic, blink, reverse, hidden, strikethrough (SGR 1-9)",
			New:  func() TestModel { return staticModel{render: attrsView} },
		},
		{
			ID: "underline", Title: "underline styles",
			Desc: "single/double/curly/dotted/dashed + colored underline (SGR 4:n, 58)",
			New:  func() TestModel { return staticModel{render: underlineView} },
		},
		{
			ID: "wide", Title: "wide characters",
			Desc: "CJK and emoji must occupy exactly two columns",
			New:  func() TestModel { return staticModel{render: wideView} },
		},
		{
			ID: "combining", Title: "combining characters",
			Desc: "combining marks must stack onto one column",
			New:  func() TestModel { return staticModel{render: combiningView} },
		},
		{
			ID: "boxdraw", Title: "DEC line drawing",
			Desc: "DEC special graphics charset (ESC ( 0) vs Unicode box drawing",
			New:  func() TestModel { return staticModel{render: boxdrawView} },
		},
		{
			ID: "wrap", Title: "autowrap / last column",
			Desc: "deferred wrap at the final column (raw demo outside the TUI)",
			Notes: "this is the behavior behind the emulation-80th-column lane " +
				"(posh#2); run it at a few widths if it looks off",
			New: func() TestModel { return newRawModel(wrapDemo, wrapExpect) },
		},
		{
			ID: "scrollregion", Title: "scroll regions",
			Desc: "DECSTBM margins: only rows 2-8 scroll (raw demo outside the TUI)",
			New:  func() TestModel { return newRawModel(scrollDemo, scrollExpect) },
		},
		{
			ID: "cursor", Title: "cursor shapes",
			Desc: "DECSCUSR cycles block / underline / bar, blinking and steady",
			New:  newCursorModel,
		},
		{
			ID: "mouse", Title: "mouse reporting",
			Desc: "click, drag, and wheel events with coordinates (SGR 1006 / all-motion)",
			Notes: "wheel behavior at a shell prompt is alternate-scroll territory " +
				"(mode 1007, posh#3/#28); here events should arrive as wheel up/down",
			New: func() TestModel { return &mouseModel{} },
		},
		{
			ID: "keys", Title: "keyboard input",
			Desc: "modifier combos, function keys, alt sequences echo back correctly",
			New:  func() TestModel { return &keysModel{} },
		},
		{
			ID: "paste", Title: "bracketed paste",
			Desc: "a paste must arrive as one atomic event, not loose keystrokes (mode 2004)",
			New:  func() TestModel { return &pasteModel{} },
		},
		{
			ID: "resize", Title: "window resize",
			Desc: "SIGWINCH propagation: live size must track your terminal window",
			New:  func() TestModel { return &resizeModel{} },
		},
		{
			ID: "title", Title: "window title",
			Desc: "OSC 0/2 sets your terminal window or tab title",
			New:  newTitleModel,
		},
		{
			ID: "hyperlink", Title: "hyperlinks",
			Desc: "OSC 8 anchors render and are clickable where supported",
			New:  func() TestModel { return staticModel{render: hyperlinkView} },
		},
		{
			ID: "clipboard", Title: "clipboard write",
			Desc: "OSC 52 copies a marker string to your local clipboard",
			Notes: "remote OSC 52 forwarding is a known posh gap (posh#27) — " +
				"over a posh remote this may legitimately fail today",
			New: func() TestModel { return &clipModel{} },
		},
		{
			ID: "bell", Title: "bell",
			Desc: "BEL rings an audible or visual bell",
			Notes: "remote BEL forwarding is a known posh gap (posh#27) — " +
				"over a posh remote this may legitimately fail today",
			New: func() TestModel { return &bellModel{} },
		},
		{
			ID: "graphics", Title: "kitty graphics",
			Desc: "inline image via the kitty graphics protocol (APC G)",
			Notes: "needs a kitty-graphics terminal locally; lost over posh remote " +
				"sync and attach replay today (posh#29)",
			New: func() TestModel { return staticModel{render: graphicsView} },
		},
	}
	for _, t := range tests {
		t.Selected = true
	}
	return tests
}
