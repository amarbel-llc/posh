package main

import (
	"encoding/base64"
	"fmt"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

// Action tests emit their one-shot escape sequence from inside a line of the
// view that changes with each trigger: the renderer rewrites changed lines
// in full, so the sequence reaches the terminal exactly once per trigger.

// --- bell (BEL) --------------------------------------------------------------

type bellModel struct{ rings int }

func (m *bellModel) Init() tea.Cmd { return nil }

func (m *bellModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	if key, ok := msg.(tea.KeyMsg); ok && key.String() == "b" {
		m.rings++
	}
	return m, nil
}

func (m *bellModel) View(int) string {
	var b strings.Builder
	b.WriteString("  Press b to ring the bell (BEL, 0x07).\n\n")
	if m.rings == 0 {
		b.WriteString("  Expect an audible beep or a visual flash, per your\n" +
			"  terminal's bell configuration.\n")
	} else {
		fmt.Fprintf(&b, "  \arang %d time(s) — did you hear/see it each time?\n", m.rings)
	}
	return b.String()
}

// --- clipboard (OSC 52) ------------------------------------------------------

type clipModel struct{ copies int }

func (m *clipModel) Init() tea.Cmd { return nil }

func (m *clipModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	if key, ok := msg.(tea.KeyMsg); ok && key.String() == "c" {
		m.copies++
	}
	return m, nil
}

func (m *clipModel) View(int) string {
	var b strings.Builder
	b.WriteString("  Press c to copy a marker string to your LOCAL clipboard\n" +
		"  via OSC 52, then paste somewhere else to verify.\n\n")
	if m.copies > 0 {
		payload := fmt.Sprintf("POSHT-clipboard-%d", m.copies)
		enc := base64.StdEncoding.EncodeToString([]byte(payload))
		fmt.Fprintf(&b, "  \x1b]52;c;%s\x07sent copy #%d — your clipboard should now hold:\n\n      %s\n",
			enc, m.copies, payload)
	}
	return b.String()
}

// --- window title (OSC 2) ----------------------------------------------------

type titleModel struct{ sets int }

func newTitleModel() TestModel { return &titleModel{} }

func (m *titleModel) Init() tea.Cmd {
	m.sets = 1
	return tea.SetWindowTitle("POSHT title test 🎯 #1")
}

func (m *titleModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	if key, ok := msg.(tea.KeyMsg); ok && key.String() == "t" {
		m.sets++
		return m, tea.SetWindowTitle(fmt.Sprintf("POSHT title test 🎯 #%d", m.sets))
	}
	return m, nil
}

func (m *titleModel) View(int) string {
	return fmt.Sprintf("  Your terminal window/tab title should now read:\n\n"+
		"      POSHT title test 🎯 #%d\n\n"+
		"  Press t to bump the number and watch the title update live.\n", m.sets)
}

func (m *titleModel) Cleanup() tea.Cmd { return tea.SetWindowTitle("posht") }

// --- cursor shapes (DECSCUSR) ------------------------------------------------

var cursorShapes = []struct {
	code int
	name string
}{
	{1, "blinking block"},
	{2, "steady block"},
	{3, "blinking underline"},
	{4, "steady underline"},
	{5, "blinking bar"},
	{6, "steady bar"},
}

var cursorGen int // invalidates ticks from an abandoned cursor test

type cursorTickMsg struct{ gen int }

type cursorModel struct {
	gen  int
	step int
}

func newCursorModel() TestModel {
	cursorGen++
	return &cursorModel{gen: cursorGen}
}

func (m *cursorModel) tick() tea.Cmd {
	gen := m.gen
	return tea.Tick(1500*time.Millisecond, func(time.Time) tea.Msg {
		return cursorTickMsg{gen: gen}
	})
}

func (m *cursorModel) Init() tea.Cmd {
	return tea.Batch(tea.ShowCursor, m.tick())
}

func (m *cursorModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	if t, ok := msg.(cursorTickMsg); ok && t.gen == m.gen {
		m.step++
		return m, m.tick()
	}
	return m, nil
}

func (m *cursorModel) View(int) string {
	shape := cursorShapes[m.step%len(cursorShapes)]
	// Emitting DECSCUSR from this line is safe: the line changes every
	// tick, so the renderer rewrites it (and the sequence) each step.
	return fmt.Sprintf("  Watch the cursor (bottom of the screen): every 1.5s it\n"+
		"  cycles through the six DECSCUSR shapes.\n\n"+
		"  \x1b[%d qnow showing %d/6: %s\n\n"+
		"  Blinking variants must blink; steady ones must not.\n",
		shape.code, m.step%len(cursorShapes)+1, shape.name)
}

func (m *cursorModel) Cleanup() tea.Cmd {
	return tea.Batch(tea.HideCursor, func() tea.Msg {
		fmt.Print("\x1b[0 q") // restore the terminal's default cursor
		return nil
	})
}

// --- kitty graphics (APC G) --------------------------------------------------

var kittyImage = buildKittyImage()

// buildKittyImage encodes a 24x24 RGB gradient as a single-chunk kitty
// graphics transmission (a=T: transmit and place at the cursor; q=2:
// suppress responses so they don't leak into the input stream).
func buildKittyImage() string {
	const n = 24
	px := make([]byte, 0, n*n*3)
	for y := 0; y < n; y++ {
		for x := 0; x < n; x++ {
			px = append(px, byte(x*255/(n-1)), byte(y*255/(n-1)), 160)
		}
	}
	return fmt.Sprintf("\x1b_Gf=24,s=%d,v=%d,i=31337,a=T,t=d,q=2;%s\x1b\\",
		n, n, base64.StdEncoding.EncodeToString(px))
}

func graphicsView(int) string {
	return "  A small square image (red→ horizontal, green↓ vertical\n" +
		"  gradient on blue) should appear between the markers:\n\n" +
		"  --- image below ---\n" +
		"  " + kittyImage + "\n\n\n" +
		"  --- image above ---\n\n" +
		"  Nothing between the markers means the kitty graphics protocol\n" +
		"  was dropped (fine for non-kitty terminals — mark skip instead).\n"
}
