package main

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

type phase int

const (
	phaseChecklist phase = iota
	phaseRun
	phaseSummary
)

var (
	styleHeader  = lipgloss.NewStyle().Bold(true).Reverse(true).Padding(0, 1)
	styleHint    = lipgloss.NewStyle().Faint(true)
	styleNote    = lipgloss.NewStyle().Foreground(lipgloss.Color("3"))
	stylePass    = lipgloss.NewStyle().Foreground(lipgloss.Color("2"))
	styleFail    = lipgloss.NewStyle().Foreground(lipgloss.Color("1")).Bold(true)
	styleSkipped = lipgloss.NewStyle().Foreground(lipgloss.Color("4"))
	styleCursor  = lipgloss.NewStyle().Bold(true)
)

type rootModel struct {
	tests  []*Test
	order  []int // indices into tests of the selected set, fixed at start
	pos    int   // position within order
	cursor int   // checklist cursor
	cur    TestModel
	phase  phase
	ran    bool // reached the run phase at least once
	width  int
	height int
}

func newRoot(tests []*Test) *rootModel {
	return &rootModel{tests: tests, width: 80, height: 24}
}

func (m *rootModel) Init() tea.Cmd { return nil }

func (m *rootModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	if size, ok := msg.(tea.WindowSizeMsg); ok {
		m.width, m.height = size.Width, size.Height
	}
	if key, ok := msg.(tea.KeyMsg); ok && key.Type == tea.KeyCtrlC {
		return m, tea.Quit
	}
	switch m.phase {
	case phaseChecklist:
		return m.updateChecklist(msg)
	case phaseRun:
		return m.updateRun(msg)
	default:
		return m.updateSummary(msg)
	}
}

// --- checklist -------------------------------------------------------------

func (m *rootModel) updateChecklist(msg tea.Msg) (tea.Model, tea.Cmd) {
	key, ok := msg.(tea.KeyMsg)
	if !ok {
		return m, nil
	}
	switch key.String() {
	case "up", "k":
		if m.cursor > 0 {
			m.cursor--
		}
	case "down", "j":
		if m.cursor < len(m.tests)-1 {
			m.cursor++
		}
	case " ":
		m.tests[m.cursor].Selected = !m.tests[m.cursor].Selected
	case "a":
		all := true
		for _, t := range m.tests {
			if !t.Selected {
				all = false
				break
			}
		}
		for _, t := range m.tests {
			t.Selected = !all
		}
	case "enter":
		m.order = m.order[:0]
		for i, t := range m.tests {
			if t.Selected {
				m.order = append(m.order, i)
			}
		}
		if len(m.order) == 0 {
			return m, nil
		}
		m.ran = true
		m.phase = phaseRun
		return m, m.startTest(0)
	case "q":
		return m, tea.Quit
	}
	return m, nil
}

// --- run -------------------------------------------------------------------

func (m *rootModel) startTest(pos int) tea.Cmd {
	m.pos = pos
	m.cur = m.tests[m.order[pos]].New()
	return m.cur.Init()
}

func (m *rootModel) cleanupCmd() tea.Cmd {
	if c, ok := m.cur.(cleaner); ok {
		return c.Cleanup()
	}
	return nil
}

func (m *rootModel) move(delta int) (tea.Model, tea.Cmd) {
	clean := m.cleanupCmd()
	next := m.pos + delta
	switch {
	case next < 0:
		return m, nil
	case next >= len(m.order):
		m.phase = phaseSummary
		m.cur = nil
		return m, clean
	default:
		return m, tea.Batch(clean, m.startTest(next))
	}
}

func (m *rootModel) updateRun(msg tea.Msg) (tea.Model, tea.Cmd) {
	if key, ok := msg.(tea.KeyMsg); ok && !key.Paste {
		capturing := false
		if c, ok := m.cur.(capturer); ok {
			capturing = c.Capturing()
		}
		if !capturing {
			t := m.tests[m.order[m.pos]]
			switch key.String() {
			case "y":
				t.Verdict = Pass
				return m.move(1)
			case "n":
				t.Verdict = Fail
				return m.move(1)
			case "s":
				t.Verdict = Skipped
				return m.move(1)
			case "right":
				return m.move(1)
			case "left":
				return m.move(-1)
			case "q":
				clean := m.cleanupCmd()
				m.phase = phaseSummary
				m.cur = nil
				return m, clean
			}
		}
	}
	var cmd tea.Cmd
	m.cur, cmd = m.cur.Update(msg)
	return m, cmd
}

// --- summary ---------------------------------------------------------------

func (m *rootModel) updateSummary(msg tea.Msg) (tea.Model, tea.Cmd) {
	key, ok := msg.(tea.KeyMsg)
	if !ok {
		return m, nil
	}
	switch key.String() {
	case "q", "enter":
		return m, tea.Quit
	case "b":
		m.phase = phaseRun
		return m, m.startTest(len(m.order) - 1)
	}
	return m, nil
}

// --- views -----------------------------------------------------------------

func (m *rootModel) View() string {
	switch m.phase {
	case phaseChecklist:
		return m.checklistView()
	case phaseRun:
		return m.runView()
	default:
		return m.summaryView()
	}
}

func (m *rootModel) checklistView() string {
	var b strings.Builder
	b.WriteString(styleHeader.Render("POSHT — posh terminal capability test") +
		styleHint.Render("  v"+version) + "\n\n")
	b.WriteString("  Deselect anything you don't want to exercise, then start.\n\n")
	for i, t := range m.tests {
		mark := "[ ]"
		if t.Selected {
			mark = "[x]"
		}
		line := fmt.Sprintf("  %s %-14s %s", mark, t.ID, t.Title)
		if i == m.cursor {
			line = styleCursor.Render(">" + line[1:])
		}
		b.WriteString(line + "\n")
	}
	b.WriteString("\n  " + styleHint.Render(m.tests[m.cursor].Desc) + "\n")
	b.WriteString("\n" + styleHint.Render(
		"  ↑/↓ move · space toggle · a all/none · enter start · q quit") + "\n")
	return b.String()
}

func (m *rootModel) runView() string {
	t := m.tests[m.order[m.pos]]
	var b strings.Builder

	badge := ""
	switch t.Verdict {
	case Pass:
		badge = "  " + stylePass.Render("✓ pass")
	case Fail:
		badge = "  " + styleFail.Render("✗ FAIL")
	case Skipped:
		badge = "  " + styleSkipped.Render("- skipped")
	}
	b.WriteString(styleHeader.Render(
		fmt.Sprintf("posht %d/%d — %s", m.pos+1, len(m.order), t.Title)) + badge + "\n\n")

	b.WriteString(m.cur.View(m.width))

	if t.Notes != "" {
		b.WriteString("\n" + styleNote.Render("  note: "+t.Notes) + "\n")
	}

	hint := "  [y] pass · [n] fail · [s] skip · ←/→ move · q end run"
	if c, ok := m.cur.(capturer); ok && c.Capturing() {
		hint = "  capturing input — press Esc to finish, then y/n/s"
	}
	b.WriteString("\n" + styleHint.Render(hint) + "\n")
	return b.String()
}

func (m *rootModel) summaryView() string {
	var b strings.Builder
	b.WriteString(styleHeader.Render("POSHT — summary") + "\n\n")
	var pass, fail, skip, pending int
	for _, i := range m.order {
		t := m.tests[i]
		var v string
		switch t.Verdict {
		case Pass:
			v = stylePass.Render("pass")
			pass++
		case Fail:
			v = styleFail.Render("FAIL")
			fail++
		case Skipped:
			v = styleSkipped.Render("skipped")
			skip++
		default:
			v = styleHint.Render("not run")
			pending++
		}
		b.WriteString(fmt.Sprintf("  %-14s %s\n", t.ID, v))
	}
	b.WriteString(fmt.Sprintf("\n  %d pass · %d fail · %d skipped · %d not run\n",
		pass, fail, skip, pending))
	b.WriteString("\n" + styleHint.Render(
		"  enter/q quit (markdown report prints to stdout) · b back to last test") + "\n")
	return b.String()
}
