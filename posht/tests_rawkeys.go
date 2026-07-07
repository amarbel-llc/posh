package main

import (
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"golang.org/x/term"
)

// The rawkeys test answers a byte-level question that the interpreted `keys`
// test (which logs tea's key.String()) structurally cannot: what EXACT bytes
// does the terminal send for a given chord, and does posh forward them intact?
// This matters for keys like Shift+Enter that have no distinct legacy byte —
// per the kitty keyboard protocol they are identical to plain Enter (0x0d)
// unless the app has negotiated the Report-all-keys enhancement (0b1000), in
// which case Shift+Enter is CSI 13;2u. See docs/features/0013 and posh#126.
//
// It bypasses tea's key parser entirely: tea.Exec drops us to a raw terminal
// and we read stdin bytes directly, so nothing normalizes the sequence before
// we record it. The receipt (via Report) carries the raw hex per prompted key
// plus the terminal's negotiated kitty-keyboard flags — and because the top-
// level receipt already records TERM, POSH_SESSION, SSH_CONNECTION, and the
// process tree, running `posht --only rawkeys --json -` in each substate
// (baseline / local posh / remote posh / nested posh) yields directly-diffable
// captures that pinpoint where, if anywhere, a distinct sequence collapses.

// keyPrompt is one scripted capture step: a chord to press and a gloss that
// interprets whatever bytes come back.
type keyPrompt struct {
	label string // what the user should press, e.g. "Shift+Enter"
	hint  string // why we care / what to watch for
}

var rawKeyScript = []keyPrompt{
	{"Enter", "the plain baseline — expect a bare CR (0d) or LF (0a)"},
	{"Shift+Enter", "the key in question — same byte as Enter unless a distinct sequence is negotiated"},
	{"Ctrl+J", "Claude's chat:newline default — expect LF (0a)"},
	{"Alt+Enter", "distinct even in legacy: expect ESC+CR (1b 0d)"},
	{"Escape", "expect bare ESC (1b), or CSI 27u under kitty disambiguate"},
	{"Shift+Tab", "a known-distinct control: expect CSI Z (1b 5b 5a) — if THIS is wrong the problem is broad"},
}

// keyCapture records what one prompt produced.
type keyCapture struct {
	Label string `json:"label"`
	Hex   string `json:"hex"`   // space-separated bytes, e.g. "1b 0d"
	Caret string `json:"caret"` // cat -v style rendering, e.g. "^[^M"
	Gloss string `json:"gloss"` // decoded meaning of a recognized sequence
}

type rawKeysModel struct {
	kittyFlags   string       // reply to CSI ? u, or "(no reply / unsupported)"
	kittyRaw     string       // raw hex of the query reply, for auditing
	captures     []keyCapture // one per scripted prompt, in order
	freeCaptures []keyCapture // free-capture panel, most recent first
	ran          bool
	done         bool // scripted phase complete (free capture is optional after)
}

func newRawKeysModel() TestModel { return &rawKeysModel{} }

func (m *rawKeysModel) exec() tea.Cmd {
	return tea.Exec(&rawKeysCmd{m: m}, func(error) tea.Msg { return rawDoneMsg{} })
}

func (m *rawKeysModel) Init() tea.Cmd { return m.exec() }

func (m *rawKeysModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	switch msg := msg.(type) {
	case rawDoneMsg:
		m.ran = true
		m.done = true
	case tea.KeyMsg:
		if msg.String() == "r" {
			return m, m.exec()
		}
	}
	return m, nil
}

func (m *rawKeysModel) View(int) string {
	var b strings.Builder
	if !m.ran {
		return "  launching the raw-key capture on the primary screen…\n"
	}
	b.WriteString("  Raw key capture complete. Bytes seen (nothing interpreted):\n\n")
	fmt.Fprintf(&b, "  kitty keyboard flags (CSI ? u reply): %s\n", m.kittyFlags)
	if m.kittyRaw != "" {
		fmt.Fprintf(&b, "    (raw query reply: %s)\n", m.kittyRaw)
	}
	b.WriteString("\n  scripted keys:\n")
	for _, c := range m.captures {
		fmt.Fprintf(&b, "    %-12s %-14s %-10s %s\n", c.Label, c.Hex, c.Caret, c.Gloss)
	}
	if len(m.freeCaptures) > 0 {
		b.WriteString("\n  free capture (most recent first):\n")
		for _, c := range m.freeCaptures {
			fmt.Fprintf(&b, "    %-14s %-10s %s\n", c.Hex, c.Caret, c.Gloss)
		}
	}
	b.WriteString("\n  Compare Enter vs Shift+Enter: identical hex ⇒ the terminal is not\n" +
		"  sending a distinct sequence (expected in legacy/disambiguate mode).\n" +
		"  Press r to run the capture again, then record the verdict.\n")
	return b.String()
}

// Report contributes the negotiated flags and every capture to the JSON
// receipt, so runs across posh substates diff directly.
func (m *rawKeysModel) Report() any {
	return map[string]any{
		"kitty_flags":     m.kittyFlags,
		"kitty_query_raw": m.kittyRaw,
		"scripted":        m.captures,
		"free":            m.freeCaptures,
	}
}

// rawKeysCmd runs the guided capture. tea.Exec releases the terminal, but to
// cooked mode (see tests_raw.go's line-buffered reads) — so this test enters
// RAW mode itself on the real tty fd, so per-keypress bytes arrive unbuffered
// and unprocessed. It reads from os.Stdin directly (the fd it put in raw mode)
// rather than the Exec-provided reader, writes prompts, decodes the bytes, and
// stores the results back on the model.
type rawKeysCmd struct {
	m   *rawKeysModel
	in  io.Reader // Exec-provided; unused (we read the raw fd instead), kept for the interface
	out io.Writer
	fd  int
}

func (c *rawKeysCmd) Run() error {
	// Fresh run: clear prior captures so `r` re-runs cleanly.
	c.m.captures = c.m.captures[:0]
	c.m.freeCaptures = c.m.freeCaptures[:0]

	// Raw mode on the real tty: unbuffered, unprocessed bytes. tea.Exec leaves
	// the terminal cooked, which would line-buffer our per-key reads.
	c.fd = int(os.Stdin.Fd())
	if st, err := term.MakeRaw(c.fd); err == nil {
		defer func() { _ = term.Restore(c.fd, st) }()
	}

	fmt.Fprint(c.out, "\x1b[2J\x1b[H")
	fmt.Fprint(c.out, "  RAW KEY CAPTURE — bytes are shown exactly as received.\r\n")
	fmt.Fprint(c.out, "  Press each prompted key ONCE. Do not paste.\r\n\r\n")

	c.queryKittyFlags()

	for _, p := range rawKeyScript {
		fmt.Fprintf(c.out, "  Press %s  (%s)\r\n", p.label, p.hint)
		raw := c.readOneKey()
		cap := decodeCapture(p.label, raw)
		c.m.captures = append(c.m.captures, cap)
		fmt.Fprintf(c.out, "    → %-14s %-10s %s\r\n\r\n", cap.Hex, cap.Caret, cap.Gloss)
	}

	fmt.Fprint(c.out, "  Free capture: press any keys to see their bytes; press\r\n"+
		"  Enter on an empty read (or wait 3s idle) to finish.\r\n\r\n")
	c.freeCapture()

	fmt.Fprint(c.out, "\r\n  -- capture done; press Enter to return to posht --")
	c.readOneKey() // swallow the final Enter
	return nil
}

// queryKittyFlags sends the kitty protocol query (CSI ? u) followed by a
// primary device-attributes request (CSI c) as a sentinel: a terminal that
// supports the protocol replies CSI ? flags u before the DA reply; one that
// doesn't answers only the DA. Per the spec's detection method.
func (c *rawKeysCmd) queryKittyFlags() {
	fmt.Fprint(c.out, "\x1b[?u\x1b[c")
	buf := c.readWithTimeout(400 * time.Millisecond)
	c.m.kittyRaw = hexBytes(buf)
	if flags, ok := parseKittyFlagsReply(buf); ok {
		c.m.kittyFlags = flags
	} else {
		c.m.kittyFlags = "(no reply / protocol unsupported)"
	}
	fmt.Fprintf(c.out, "  kitty keyboard flags: %s\r\n\r\n", c.m.kittyFlags)
}

// readOneKey reads one keypress worth of bytes: the first read, plus any bytes
// that arrive within a short quiet window (so a multi-byte escape sequence from
// a single keypress is captured whole rather than split).
func (c *rawKeysCmd) readOneKey() []byte {
	return c.readWithTimeout(60 * time.Millisecond)
}

// readWithTimeout reads until a quiet gap of `quiet` passes after the first
// byte, or a hard cap elapses. It relies on the caller's terminal being in raw
// mode (tea.Exec) so bytes arrive unbuffered. A background goroutine feeds a
// channel; we assemble until quiet.
func (c *rawKeysCmd) readWithTimeout(quiet time.Duration) []byte {
	type readResult struct {
		b   byte
		err error
	}
	ch := make(chan readResult, 64)
	go func() {
		var one [1]byte
		for {
			// Read the raw tty fd directly (put in raw mode by Run), not the
			// cooked Exec-provided reader.
			n, err := os.Stdin.Read(one[:])
			if n > 0 {
				ch <- readResult{b: one[0]}
			}
			if err != nil {
				ch <- readResult{err: err}
				return
			}
		}
	}()

	var out []byte
	// Wait up to 3s for the FIRST byte (a prompted key the user hasn't
	// pressed yet), then only `quiet` between subsequent bytes.
	first := time.NewTimer(3 * time.Second)
	defer first.Stop()
	select {
	case r := <-ch:
		if r.err != nil {
			return out
		}
		out = append(out, r.b)
	case <-first.C:
		return out
	}
	for {
		t := time.NewTimer(quiet)
		select {
		case r := <-ch:
			t.Stop()
			if r.err != nil {
				return out
			}
			out = append(out, r.b)
		case <-t.C:
			return out
		}
	}
}

// freeCapture loops reading keypresses until an empty/idle read or a lone
// Enter, recording each. Most-recent-first in the model for display.
func (c *rawKeysCmd) freeCapture() {
	for i := 0; i < 64; i++ {
		raw := c.readWithTimeout(60 * time.Millisecond)
		if len(raw) == 0 {
			return // idle timeout: done
		}
		if len(raw) == 1 && (raw[0] == '\r' || raw[0] == '\n') {
			return // lone Enter ends free capture
		}
		cap := decodeCapture("", raw)
		fmt.Fprintf(c.out, "    → %-14s %-10s %s\r\n", cap.Hex, cap.Caret, cap.Gloss)
		// Prepend (most recent first) for the receipt/display.
		c.m.freeCaptures = append([]keyCapture{cap}, c.m.freeCaptures...)
	}
}

func (c *rawKeysCmd) SetStdin(r io.Reader)  { c.in = r }
func (c *rawKeysCmd) SetStdout(w io.Writer) { c.out = w }
func (c *rawKeysCmd) SetStderr(io.Writer)   {}

// --- decoding helpers (pure; unit-testable) --------------------------------

func hexBytes(b []byte) string {
	if len(b) == 0 {
		return "(none)"
	}
	parts := make([]string, len(b))
	for i, x := range b {
		parts[i] = fmt.Sprintf("%02x", x)
	}
	return strings.Join(parts, " ")
}

// caretRender renders bytes cat -v style: control bytes as ^X, 0x7f as ^?,
// ESC as ^[, printable ASCII verbatim, other bytes as \xNN.
func caretRender(b []byte) string {
	var s strings.Builder
	for _, x := range b {
		switch {
		case x == 0x7f:
			s.WriteString("^?")
		case x < 0x20:
			s.WriteByte('^')
			s.WriteByte(x + '@') // 0x1b -> ^[, 0x0d -> ^M, 0x0a -> ^J
		case x < 0x7f:
			s.WriteByte(x)
		default:
			fmt.Fprintf(&s, "\\x%02x", x)
		}
	}
	return s.String()
}

// decodeCapture builds a keyCapture from raw bytes, glossing recognized
// sequences relevant to the Enter/Shift+Enter question.
func decodeCapture(label string, raw []byte) keyCapture {
	return keyCapture{
		Label: label,
		Hex:   hexBytes(raw),
		Caret: caretRender(raw),
		Gloss: glossBytes(raw),
	}
}

// glossBytes names a sequence when it matches a form we care about.
func glossBytes(b []byte) string {
	switch {
	case len(b) == 0:
		return "(nothing captured)"
	case len(b) == 1 && b[0] == 0x0d:
		return "CR (0x0d) — legacy Enter; Shift+Enter is identical here"
	case len(b) == 1 && b[0] == 0x0a:
		return "LF (0x0a) — Ctrl+J / newline"
	case len(b) == 1 && b[0] == 0x1b:
		return "bare ESC (0x1b)"
	case len(b) == 2 && b[0] == 0x1b && b[1] == 0x0d:
		return "ESC+CR (1b 0d) — Alt+Enter form / Claude's /terminal-setup newline"
	case len(b) == 2 && b[0] == 0x1b && b[1] == 0x0a:
		return "ESC+LF (1b 0a)"
	case string(b) == "\x1b[27u":
		return "CSI 27u — kitty-disambiguated Escape"
	case string(b) == "\x1b[13u":
		return "CSI 13u — kitty Enter (report-all)"
	case string(b) == "\x1b[13;2u":
		return "CSI 13;2u — kitty Shift+Enter (distinct! report-all negotiated)"
	case string(b) == "\x1b[Z":
		return "CSI Z — Shift+Tab (legacy back-tab)"
	case string(b) == "\x1b[9;2u":
		return "CSI 9;2u — kitty Shift+Tab (report-all)"
	case len(b) >= 3 && b[0] == 0x1b && b[1] == '[':
		return "a CSI sequence (ESC [ …)"
	case len(b) >= 2 && b[0] == 0x1b:
		return "an ESC-led sequence"
	default:
		return "literal input"
	}
}

// parseKittyFlagsReply extracts the flags from a CSI ? flags u reply embedded
// in the query response, returning the decimal flags string and whether a
// reply was present. A terminal without the protocol answers only the DA (no
// CSI ? … u), so ok is false.
func parseKittyFlagsReply(b []byte) (string, bool) {
	s := string(b)
	// Look for the "\x1b[?" ... "u" reply. The DA reply is "\x1b[?...c", so
	// require the 'u' terminator specifically.
	start := strings.Index(s, "\x1b[?")
	for start >= 0 {
		rest := s[start+len("\x1b[?"):]
		// The reply body is digits terminated by 'u' (flags) or 'c' (DA).
		end := strings.IndexAny(rest, "uc")
		if end >= 0 && rest[end] == 'u' {
			flags := rest[:end]
			if flags == "" {
				flags = "0"
			}
			return flags, true
		}
		next := strings.Index(s[start+1:], "\x1b[?")
		if next < 0 {
			break
		}
		start = start + 1 + next
	}
	return "", false
}
