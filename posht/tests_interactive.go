package main

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

// --- mouse reporting ---------------------------------------------------------

type mouseModel struct {
	events int
	wheel  int
	log    []string
}

func (m *mouseModel) Init() tea.Cmd { return tea.EnableMouseAllMotion }

func (m *mouseModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	if mouse, ok := msg.(tea.MouseMsg); ok {
		m.events++
		ev := tea.MouseEvent(mouse)
		if ev.Button == tea.MouseButtonWheelUp || ev.Button == tea.MouseButtonWheelDown {
			m.wheel++
		}
		entry := fmt.Sprintf("%-24s at col %d, row %d", ev.String(), ev.X+1, ev.Y+1)
		m.log = append(m.log, entry)
		if len(m.log) > 5 {
			m.log = m.log[len(m.log)-5:]
		}
	}
	return m, nil
}

func (m *mouseModel) View(int) string {
	var b strings.Builder
	b.WriteString("  Mouse reporting is on (all-motion + SGR encoding).\n" +
		"  Click, drag, and scroll the wheel anywhere — events and their\n" +
		"  coordinates must appear below and track your pointer.\n\n")
	if m.events == 0 {
		b.WriteString("  waiting for mouse input…\n")
	} else {
		fmt.Fprintf(&b, "  %d events (%d wheel) — last 5:\n", m.events, m.wheel)
		for _, l := range m.log {
			b.WriteString("    " + l + "\n")
		}
	}
	return b.String()
}

func (m *mouseModel) Cleanup() tea.Cmd { return tea.DisableMouse }

// --- keyboard input ----------------------------------------------------------

type keysModel struct {
	log  []string
	done bool
}

func (m *keysModel) Capturing() bool { return !m.done }

func (m *keysModel) Init() tea.Cmd { return nil }

func (m *keysModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	key, ok := msg.(tea.KeyMsg)
	if !ok {
		return m, nil
	}
	if m.done {
		return m, nil
	}
	if key.Type == tea.KeyEsc {
		m.done = true
		return m, nil
	}
	m.log = append(m.log, key.String())
	if len(m.log) > 8 {
		m.log = m.log[len(m.log)-8:]
	}
	return m, nil
}

func (m *keysModel) View(int) string {
	var b strings.Builder
	b.WriteString("  Type keys with modifiers — ctrl+arrows, alt+letters,\n" +
		"  shift+tab, F-keys, home/end/pgup — and check each echoes back\n" +
		"  as the key you pressed (not garbage, not a plain variant).\n\n")
	if m.done {
		b.WriteString("  capture finished — judge the log, then y/n/s.\n\n")
	}
	if len(m.log) == 0 {
		b.WriteString("  waiting for keys…\n")
	} else {
		for _, l := range m.log {
			b.WriteString("    " + l + "\n")
		}
	}
	return b.String()
}

// --- bracketed paste ---------------------------------------------------------

type pasteModel struct {
	pastes []int // rune counts of received paste events
	loose  int   // non-paste keys received while capturing
	done   bool
}

func (m *pasteModel) Capturing() bool { return !m.done }

func (m *pasteModel) Init() tea.Cmd { return nil }

func (m *pasteModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	key, ok := msg.(tea.KeyMsg)
	if !ok || m.done {
		return m, nil
	}
	switch {
	case key.Paste:
		m.pastes = append(m.pastes, len(key.Runes))
	case key.Type == tea.KeyEsc:
		m.done = true
	default:
		m.loose++
	}
	return m, nil
}

func (m *pasteModel) View(int) string {
	var b strings.Builder
	b.WriteString("  Paste some multi-character text now (clipboard paste, not\n" +
		"  typing). With bracketed paste (mode 2004) working, the whole\n" +
		"  paste arrives as ONE atomic event:\n\n")
	for i, n := range m.pastes {
		fmt.Fprintf(&b, "    paste %d: one event, %d characters ✓\n", i+1, n)
	}
	if m.loose > 0 {
		fmt.Fprintf(&b, "    %d loose keystrokes — if these came from your paste,\n"+
			"    bracketed paste is BROKEN (that's a fail)\n", m.loose)
	}
	if len(m.pastes) == 0 && m.loose == 0 {
		b.WriteString("    waiting for a paste…\n")
	}
	return b.String()
}

// --- window resize -----------------------------------------------------------

type resizeModel struct {
	w, h    int
	changes int
}

func (m *resizeModel) Init() tea.Cmd { return nil }

func (m *resizeModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	if size, ok := msg.(tea.WindowSizeMsg); ok {
		if m.w != 0 {
			m.changes++
		}
		m.w, m.h = size.Width, size.Height
	}
	return m, nil
}

func (m *resizeModel) View(w int) string {
	cur := fmt.Sprintf("%d × %d", m.w, m.h)
	if m.w == 0 {
		cur = fmt.Sprintf("%d × ? (no size event yet)", w)
	}
	ruler := "  ├" + strings.Repeat("─", max(0, w-6)) + "┤"
	return fmt.Sprintf("  Resize your terminal window. The size below must track it\n"+
		"  live, and the ruler must always span the full width:\n\n"+
		"      current size: %s   (%d resize events)\n\n%s\n",
		cur, m.changes, ruler)
}
