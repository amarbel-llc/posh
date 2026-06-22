// charm-tui-poc renderer: a long-running bubbletea v2 renderer driven by the
// host over a JSON-RPC-style control channel (newline-delimited JSON on fd 3),
// rendering to its PTY. The host tells it to show the command palette (modeled
// on trapeze's "/" Commands dialog, opened by Ctrl-^ or "/"); the renderer
// reports palette selections/cancels back as events. Throwaway POC content for
// the posh client-side TUI host.
//
// Protocol (one JSON object per line on fd 3):
//
//	host  -> renderer:  {"method":"show","params":{"view":"palette",
//	                       "commands":[{"name":"Quit","shortcut":""}]}}
//	                    {"method":"hide","params":{}}
//	renderer -> host:   {"method":"event","params":{"kind":"selected",
//	                       "command":"Quit"}}
//	                    {"method":"event","params":{"kind":"cancel"}}
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"charm.land/bubbles/v2/key"
	"charm.land/bubbles/v2/textinput"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
)

// --- control protocol ---

type rpcIn struct {
	Method string          `json:"method"`
	Params json.RawMessage `json:"params"`
}

type command struct {
	Name     string `json:"name"`
	Shortcut string `json:"shortcut"`
}

type showParams struct {
	View     string    `json:"view"`
	Commands []command `json:"commands,omitempty"`
}

type eventOut struct {
	Method string      `json:"method"`
	Params eventParams `json:"params"`
}

type eventParams struct {
	Kind    string `json:"kind"`
	Command string `json:"command,omitempty"`
}

// bubbletea messages produced from control input.
type showMsg showParams
type hideMsg struct{}

// --- styling ---

var (
	// The palette wears the yellow double-border the chord indicator used to.
	panelStyle = lipgloss.NewStyle().
			Border(lipgloss.DoubleBorder()).
			BorderForeground(lipgloss.Color("214")).
			Padding(1, 2).
			Width(46)

	titleStyle = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("214"))
	selStyle   = lipgloss.NewStyle().
			Bold(true).
			Foreground(lipgloss.Color("231")).
			Background(lipgloss.Color("214"))
	dimStyle  = lipgloss.NewStyle().Faint(true)
	helpStyle = lipgloss.NewStyle().Faint(true)
)

// --- model ---

type viewKind int

const (
	viewNone viewKind = iota
	viewPalette
)

type keymap struct {
	up, down, sel, cancel key.Binding
}

type model struct {
	view     viewKind
	ctrl     *os.File
	input    textinput.Model
	keys     keymap
	commands []command
	filtered []command
	selected int
}

func newModel(ctrl *os.File) model {
	in := textinput.New()
	in.Placeholder = "Type to filter"
	in.Prompt = "/ "
	return model{
		view:  viewNone,
		ctrl:  ctrl,
		input: in,
		keys: keymap{
			up:     key.NewBinding(key.WithKeys("up")),
			down:   key.NewBinding(key.WithKeys("down")),
			sel:    key.NewBinding(key.WithKeys("enter")),
			cancel: key.NewBinding(key.WithKeys("esc")),
		},
	}
}

func (m model) Init() tea.Cmd { return nil }

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case showMsg:
		switch msg.View {
		case "palette":
			m.view = viewPalette
			m.commands = msg.Commands
			m.input.SetValue("")
			m.input.Focus()
			m.selected = 0
			m.recompute()
		default:
			m.view = viewNone
		}
		return m, nil
	case hideMsg:
		m.view = viewNone
		return m, nil
	case tea.KeyPressMsg:
		if m.view != viewPalette {
			return m, nil // host owns chord keys; ignore otherwise
		}
		switch {
		case keyMatches(msg, m.keys.cancel):
			m.sendEvent("cancel", "")
			m.view = viewNone
			return m, nil
		case keyMatches(msg, m.keys.sel):
			if len(m.filtered) > 0 {
				m.sendEvent("selected", m.filtered[m.selected].Name)
			}
			m.view = viewNone
			return m, nil
		case keyMatches(msg, m.keys.up):
			if m.selected > 0 {
				m.selected--
			}
			return m, nil
		case keyMatches(msg, m.keys.down):
			if m.selected < len(m.filtered)-1 {
				m.selected++
			}
			return m, nil
		}
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(msg)
		m.recompute()
		return m, cmd
	}
	return m, nil
}

func keyMatches(k tea.KeyPressMsg, b key.Binding) bool {
	return key.Matches(k, b)
}

func (m *model) recompute() {
	q := strings.ToLower(strings.TrimSpace(m.input.Value()))
	m.filtered = nil
	for _, c := range m.commands {
		if q == "" || strings.Contains(strings.ToLower(c.Name), q) {
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

func (m model) sendEvent(kind, cmd string) {
	if m.ctrl == nil {
		return
	}
	b, _ := json.Marshal(eventOut{Method: "event", Params: eventParams{Kind: kind, Command: cmd}})
	m.ctrl.Write(append(b, '\n'))
}

func (m model) View() tea.View {
	switch m.view {
	case viewPalette:
		return tea.NewView(m.paletteView())
	default:
		return tea.NewView("")
	}
}

func (m model) paletteView() string {
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
		row := c.Name
		if c.Shortcut != "" {
			row = fmt.Sprintf("%-22s %s", c.Name, c.Shortcut)
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
	return panelStyle.Render(b.String())
}

// --- main + control reader ---

func main() {
	ctrl := os.NewFile(3, "control")
	p := tea.NewProgram(newModel(ctrl))
	if ctrl != nil {
		go readControl(ctrl, p)
	}
	if _, err := p.Run(); err != nil {
		fmt.Fprintln(os.Stderr, "renderer:", err)
		os.Exit(1)
	}
}

func readControl(ctrl *os.File, p *tea.Program) {
	sc := bufio.NewScanner(ctrl)
	for sc.Scan() {
		var in rpcIn
		if json.Unmarshal(sc.Bytes(), &in) != nil {
			continue
		}
		switch in.Method {
		case "show":
			var sp showParams
			_ = json.Unmarshal(in.Params, &sp)
			p.Send(showMsg(sp))
		case "hide":
			p.Send(hideMsg{})
		}
	}
	p.Quit() // control channel closed -> shut down
}
