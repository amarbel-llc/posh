// posh-palette is the command-palette renderer for the posh client: a
// long-running bubbletea (v2) subprocess that draws a chord-summoned,
// filterable command list and reports the user's selection back to the client
// over a JSON-RPC 2.0 control channel on fd 3. It renders to its PTY (stdout);
// the client composites that screen onto the live session view.
//
// The renderer is deliberately generic — it knows nothing about posh's session
// or what any command does. The client configures the palette (ui.show) and
// dispatches the action a chosen command carries. The wire contract is RFC 0005
// (docs/rfcs/0005-palette-control-protocol.md).
package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"sync"

	"charm.land/bubbles/v2/key"
	"charm.land/bubbles/v2/textinput"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
)

// protocolVersion is the RFC 0005 version this renderer speaks.
const protocolVersion = 1

// --- JSON-RPC 2.0 framing (RFC 0005 §2) ---

type rpcError struct {
	Code    int             `json:"code"`
	Message string          `json:"message"`
	Data    json.RawMessage `json:"data,omitempty"`
}

// rpcMessage is the union of request/response/notification. A non-empty Method
// marks a request (with ID) or notification (no ID); an empty Method with an ID
// marks a response to one of our own requests.
type rpcMessage struct {
	JSONRPC string           `json:"jsonrpc"`
	ID      *json.RawMessage `json:"id,omitempty"`
	Method  string           `json:"method,omitempty"`
	Params  json.RawMessage  `json:"params,omitempty"`
	Result  json.RawMessage  `json:"result,omitempty"`
	Error   *rpcError        `json:"error,omitempty"`
}

// conn serializes JSON-RPC writes to the control channel (fd 3) across the
// control-reader goroutine and the bubbletea model, which both originate
// messages. Reads happen only in the reader goroutine.
type conn struct {
	mu  sync.Mutex
	w   *os.File
	seq int64
}

func (c *conn) write(m rpcMessage) {
	m.JSONRPC = "2.0"
	b, err := json.Marshal(m)
	if err != nil {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	_, _ = c.w.Write(append(b, '\n'))
}

// request issues a client-bound JSON-RPC request with a fresh id. The renderer
// does not block on the response (the reader goroutine discards it); the action
// is fire-and-forget from the UI's perspective (RFC 0005 §4.1).
func (c *conn) request(method string, params json.RawMessage) {
	c.mu.Lock()
	c.seq++
	id := json.RawMessage(fmt.Sprintf("%d", c.seq))
	c.mu.Unlock()
	c.write(rpcMessage{ID: &id, Method: method, Params: params})
}

func (c *conn) notify(method string, params json.RawMessage) {
	c.write(rpcMessage{Method: method, Params: params})
}

func (c *conn) respond(id *json.RawMessage, result json.RawMessage) {
	c.write(rpcMessage{ID: id, Result: result})
}

func (c *conn) respondError(id *json.RawMessage, code int, msg string) {
	c.write(rpcMessage{ID: id, Error: &rpcError{Code: code, Message: msg}})
}

// --- protocol payloads (RFC 0005 §3, §5) ---

type initParams struct {
	Protocol int `json:"protocol"`
}

type initResult struct {
	Name     string `json:"name"`
	Version  string `json:"version"`
	Protocol int    `json:"protocol"`
}

type action struct {
	Method string          `json:"method"`
	Params json.RawMessage `json:"params,omitempty"`
}

type command struct {
	Name string `json:"name"`
	// Action is issued to the client when this command is chosen; nil for a
	// no-op entry. Opaque to the renderer (RFC 0005 §5).
	Action *action `json:"action,omitempty"`
}

type showParams struct {
	View     string    `json:"view"`
	Commands []command `json:"commands,omitempty"`
	Title    string    `json:"title,omitempty"`
	Prompt   string    `json:"prompt,omitempty"`
}

// bubbletea messages produced from control input.
type showMsg showParams
type hideMsg struct{}

// --- styling ---

var (
	// The palette wears the yellow double-border the chord indicator used.
	panelStyle = lipgloss.NewStyle().
			Border(lipgloss.DoubleBorder()).
			BorderForeground(lipgloss.Color("214")).
			Padding(1, 2).
			Width(46)

	titleStyle = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("214"))
	// Selection highlight keeps its purple (63), not the yellow border.
	selStyle = lipgloss.NewStyle().
			Bold(true).
			Foreground(lipgloss.Color("231")).
			Background(lipgloss.Color("63"))
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
	conn     *conn
	input    textinput.Model
	keys     keymap
	title    string
	commands []command
	filtered []command
	selected int
}

func newModel(c *conn) model {
	in := textinput.New()
	in.Placeholder = "Type to filter"
	in.Prompt = "/ "
	return model{
		view:  viewNone,
		conn:  c,
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
		if msg.View != "palette" {
			m.view = viewNone
			return m, nil
		}
		m.view = viewPalette
		m.commands = msg.Commands
		m.title = msg.Title
		if m.title == "" {
			m.title = "Commands"
		}
		if msg.Prompt != "" {
			m.input.Prompt = msg.Prompt
		}
		m.input.SetValue("")
		m.input.Focus()
		m.selected = 0
		m.recompute()
		return m, nil
	case hideMsg:
		m.view = viewNone
		return m, nil
	case tea.KeyPressMsg:
		if m.view != viewPalette {
			return m, nil // hidden: the client owns the keyboard
		}
		switch {
		case key.Matches(msg, m.keys.cancel):
			m.conn.notify("ui.cancelled", nil)
			m.view = viewNone
			return m, nil
		case key.Matches(msg, m.keys.sel):
			m.choose()
			m.view = viewNone
			return m, nil
		case key.Matches(msg, m.keys.up):
			if m.selected > 0 {
				m.selected--
			}
			return m, nil
		case key.Matches(msg, m.keys.down):
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

// choose issues the selected command's action to the client, or reports a
// cancel when there is nothing to select (RFC 0005 §4.1/§4.2).
func (m model) choose() {
	if len(m.filtered) == 0 {
		m.conn.notify("ui.cancelled", nil)
		return
	}
	a := m.filtered[m.selected].Action
	if a == nil || a.Method == "" {
		m.conn.notify("ui.cancelled", nil) // no-op entry
		return
	}
	m.conn.request(a.Method, a.Params)
}

func (m model) View() tea.View {
	if m.view == viewPalette {
		return tea.NewView(m.paletteView())
	}
	return tea.NewView("")
}

func (m model) paletteView() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render(m.title))
	b.WriteByte('\n')
	b.WriteString(m.input.View())
	b.WriteString("\n\n")
	if len(m.filtered) == 0 {
		b.WriteString(dimStyle.Render("(no matches)"))
		b.WriteByte('\n')
	}
	for i, c := range m.filtered {
		if i == m.selected {
			b.WriteString(selStyle.Render("› " + c.Name))
		} else {
			b.WriteString("  " + c.Name)
		}
		b.WriteByte('\n')
	}
	b.WriteByte('\n')
	b.WriteString(helpStyle.Render("↑/↓ choose · enter run · esc cancel"))
	return panelStyle.Render(b.String())
}

// --- main + control reader ---

func main() {
	for _, a := range os.Args[1:] {
		if a == "--version" || a == "-version" || a == "-v" {
			fmt.Println(versionLine())
			return
		}
	}
	ctrl := os.NewFile(3, "control")
	if ctrl == nil {
		fmt.Fprintln(os.Stderr, "posh-palette: no control channel on fd 3")
		os.Exit(2)
	}
	c := &conn{w: ctrl}
	p := tea.NewProgram(newModel(c))
	go readControl(c, ctrl, p)
	if _, err := p.Run(); err != nil && !errors.Is(err, tea.ErrProgramKilled) {
		fmt.Fprintln(os.Stderr, "posh-palette:", err)
		os.Exit(1)
	}
}

// readControl pumps the control channel: it answers client→renderer requests
// (RFC 0005 §3) and forwards view changes to the program. Responses to the
// renderer's own requests are discarded (the UI does not block on them).
func readControl(c *conn, r *os.File, p *tea.Program) {
	sc := bufio.NewScanner(r)
	sc.Buffer(make([]byte, 0, 64*1024), 1<<20)
	for sc.Scan() {
		line := sc.Bytes()
		if len(strings.TrimSpace(string(line))) == 0 {
			continue
		}
		var m rpcMessage
		if json.Unmarshal(line, &m) != nil {
			continue // unparseable; id unrecoverable -> ignore (RFC 0005 §6)
		}
		if m.Method == "" {
			continue // a response to one of our requests; not awaited
		}
		switch m.Method {
		case "initialize":
			var ip initParams
			_ = json.Unmarshal(m.Params, &ip)
			if ip.Protocol < protocolVersion {
				c.respondError(m.ID, -32000, "unsupported protocol version")
				continue
			}
			res, _ := json.Marshal(initResult{
				Name:     "posh-palette",
				Version:  versionString(),
				Protocol: protocolVersion,
			})
			c.respond(m.ID, res)
		case "ui.show":
			var sp showParams
			if json.Unmarshal(m.Params, &sp) != nil || sp.View != "palette" {
				c.respondError(m.ID, -32602, "invalid params: unknown view")
				continue
			}
			c.respond(m.ID, json.RawMessage(`{}`))
			p.Send(showMsg(sp))
		case "ui.hide":
			c.respond(m.ID, json.RawMessage(`{}`))
			p.Send(hideMsg{})
		case "ui.shutdown":
			p.Kill() // forceful: cancels the program context even if wedged
			return
		default:
			if m.ID != nil {
				c.respondError(m.ID, -32601, "method not found: "+m.Method)
			}
		}
	}
	p.Kill() // control channel closed -> stop
}
