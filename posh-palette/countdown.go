package main

// The "countdown" view (PROTOTYPE; docs/plans/2026-10-24-app-modals-for-restart.md):
// an app-presented modal with a title, a body, the time the DAEMON says is left,
// and a row of buttons. The renderer keeps no clock: the client sends the
// remaining time with the show and again with each `ui.update`.
//
// Building blocks (posh#254, extracted as this view was built rather than
// up front): buttonRow, progressBar, ceilSeconds. They are free functions with
// no model state so the next view can compose them.

import (
	"fmt"
	"strings"

	"charm.land/bubbles/v2/key"
	tea "charm.land/bubbletea/v2"
)

// updateMsg is `ui.update` (a notification): new remaining time for the
// countdown on screen. Focus is untouched; with no countdown up it is ignored.
type updateMsg struct{ RemainingMs int64 }

// updateParams is the wire shape of `ui.update`.
type updateParams struct {
	RemainingMs int64 `json:"remaining_ms"`
}

// ceilSeconds rounds a millisecond count UP to whole seconds, so a countdown
// never reads 0s while time remains.
func ceilSeconds(ms int64) int64 {
	if ms <= 0 {
		return 0
	}
	return (ms + 999) / 1000
}

// buttonRow draws labels side by side; the focused one is highlighted and
// carries a leading marker so the focus survives a terminal without colour.
func buttonRow(labels []string, focus int) string {
	parts := make([]string, len(labels))
	for i, l := range labels {
		if i == focus {
			parts[i] = selStyle.Render("▶ " + l + " ")
		} else {
			parts[i] = "  " + l + " "
		}
	}
	return strings.Join(parts, "  ")
}

// progressBar is `width` cells filled in proportion to remaining/total; empty
// when the total is unknown.
func progressBar(remainingMs, totalMs int64, width int) string {
	if totalMs <= 0 || width <= 0 {
		return ""
	}
	filled := int((remainingMs*int64(width) + totalMs/2) / totalMs)
	filled = max(0, min(filled, width))
	return strings.Repeat("█", filled) + strings.Repeat("░", width-filled)
}

func (m model) countdownView() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render(m.title))
	b.WriteString("\n\n")
	b.WriteString(m.body)
	b.WriteString("\n\n")
	fmt.Fprintf(&b, "Time left: %ds", ceilSeconds(m.remainingMs))
	b.WriteByte('\n')
	panelWidth := 60
	if m.width > 0 {
		panelWidth = min(panelWidth, m.width-4)
	}
	panelWidth = max(panelWidth, 30)
	if bar := progressBar(m.remainingMs, m.totalMs, panelWidth-dialogStyle.GetHorizontalFrameSize()); bar != "" {
		b.WriteString(dimStyle.Render(bar))
		b.WriteByte('\n')
	}
	b.WriteByte('\n')
	labels := make([]string, len(m.commands))
	for i, c := range m.commands {
		labels[i] = c.Name
	}
	b.WriteString(buttonRow(labels, m.focus))
	b.WriteString("\n\n")
	b.WriteString(helpStyle.Render("←/→ choose · enter select · esc hide"))
	return dialogStyle.Width(panelWidth).Render(b.String())
}

// onCountdownKey handles a key press while the countdown is up. Only bound
// keys act: the view can be raised at a moment the user did not choose, so an
// unbound keystroke is ignored (not consumed as an answer, not a dismissal).
func (m model) onCountdownKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch {
	case key.Matches(msg, m.keys.left):
		m.focus = max(m.focus-1, 0)
	case key.Matches(msg, m.keys.right):
		m.focus = min(m.focus+1, max(len(m.commands)-1, 0))
	case key.Matches(msg, m.keys.sel):
		m.chooseButton()
		m.view = viewNone
	case key.Matches(msg, m.keys.cancel):
		m.conn.notify("ui.cancelled", nil)
		m.view = viewNone
	}
	return m, nil
}

// chooseButton issues the focused button's action to the client, or reports a
// cancel when it has none (RFC 0005 §4.1).
func (m model) chooseButton() {
	if m.focus >= len(m.commands) || m.commands[m.focus].Action == nil || m.commands[m.focus].Action.Method == "" {
		m.conn.notify("ui.cancelled", nil)
		return
	}
	a := m.commands[m.focus].Action
	m.conn.request(a.Method, a.Params)
}
