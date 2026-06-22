// charm-tui-poc command bar: a bubbletea v2 command palette modeled on
// trapeze's "/" Commands dialog (the charm.land/bubbletea/v2 stack). Throwaway
// POC content for the posh client-side TUI host. The command list is a
// hardcoded subset of trapeze's defaults; selecting a command just echoes it
// and exits (the POC has no real actions). Esc cancels.
package main

import (
	"fmt"
	"os"
	"strings"

	"charm.land/bubbles/v2/key"
	"charm.land/bubbles/v2/textinput"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
)

type command struct {
	name     string
	shortcut string
}

// A representative subset of trapeze's default system commands
// (internal/ui/dialog/commands.go defaultCommands).
var allCommands = []command{
	{"New Session", "ctrl+n"},
	{"Sessions", "ctrl+s"},
	{"Switch Model", "ctrl+l"},
	{"Toggle Thinking Mode", ""},
	{"Open File Picker", "ctrl+f"},
	{"Open External Editor", "ctrl+o"},
	{"Toggle Help", "ctrl+g"},
	{"Initialize Project", ""},
	{"Toggle Yolo Mode", "ctrl+y"},
	{"Quit", "ctrl+c"},
}

var (
	panelStyle = lipgloss.NewStyle().
			Border(lipgloss.RoundedBorder()).
			BorderForeground(lipgloss.Color("63")).
			Padding(1, 2).
			Width(46)

	titleStyle = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("63"))
	selStyle   = lipgloss.NewStyle().
			Bold(true).
			Foreground(lipgloss.Color("231")).
			Background(lipgloss.Color("63"))
	dimStyle  = lipgloss.NewStyle().Faint(true)
	helpStyle = lipgloss.NewStyle().Faint(true)
)

type keymap struct {
	up, down, sel, cancel key.Binding
}

type model struct {
	input    textinput.Model
	keys     keymap
	filtered []command
	selected int
	chosen   string
}

func newModel() model {
	in := textinput.New()
	in.Placeholder = "Type to filter"
	in.Prompt = "/ "
	in.Focus()
	m := model{
		input: in,
		keys: keymap{
			up:     key.NewBinding(key.WithKeys("up")),
			down:   key.NewBinding(key.WithKeys("down")),
			sel:    key.NewBinding(key.WithKeys("enter")),
			cancel: key.NewBinding(key.WithKeys("esc")),
		},
	}
	m.recompute()
	return m
}

func (m *model) recompute() {
	q := strings.ToLower(strings.TrimSpace(m.input.Value()))
	m.filtered = nil
	for _, c := range allCommands {
		if q == "" || strings.Contains(strings.ToLower(c.name), q) {
			m.filtered = append(m.filtered, c)
		}
	}
	if m.selected >= len(m.filtered) {
		m.selected = len(m.filtered) - 1
	}
	if m.selected < 0 {
		m.selected = 0
	}
}

func (m model) Init() tea.Cmd { return nil }

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	if key, ok := msg.(tea.KeyPressMsg); ok {
		switch {
		case keyMatches(key, m.keys.cancel):
			return m, tea.Quit
		case keyMatches(key, m.keys.sel):
			if len(m.filtered) > 0 {
				m.chosen = m.filtered[m.selected].name
			}
			return m, tea.Quit
		case keyMatches(key, m.keys.up):
			if m.selected > 0 {
				m.selected--
			}
			return m, nil
		case keyMatches(key, m.keys.down):
			if m.selected < len(m.filtered)-1 {
				m.selected++
			}
			return m, nil
		}
	}
	var cmd tea.Cmd
	m.input, cmd = m.input.Update(msg)
	m.recompute()
	return m, cmd
}

func keyMatches(k tea.KeyPressMsg, b key.Binding) bool {
	return key.Matches(k, b)
}

func (m model) View() tea.View {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Commands"))
	b.WriteByte('\n')
	b.WriteString(m.input.View())
	b.WriteString("\n\n")
	if len(m.filtered) == 0 {
		b.WriteString(dimStyle.Render("(no matches)"))
		b.WriteByte('\n')
	}
	for i, c := range m.filtered {
		row := c.name
		if c.shortcut != "" {
			row = fmt.Sprintf("%-22s %s", c.name, c.shortcut)
		}
		if i == m.selected {
			b.WriteString(selStyle.Render("› " + row))
		} else {
			b.WriteString("  " + row)
		}
		b.WriteByte('\n')
	}
	b.WriteByte('\n')
	b.WriteString(helpStyle.Render("↑/↓ choose · enter run · esc cancel"))
	return tea.NewView(panelStyle.Render(b.String()))
}

func main() {
	final, err := tea.NewProgram(newModel()).Run()
	if err != nil {
		fmt.Fprintln(os.Stderr, "command-bar:", err)
		os.Exit(1)
	}
	if m, ok := final.(model); ok && m.chosen != "" {
		fmt.Println("ran:", m.chosen)
	}
}
