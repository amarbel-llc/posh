package main

import (
	"fmt"
	"io"
	"os"
	"strings"
	"syscall"
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
// It also answers the posh#130 question: Ctrl-^ (the command-palette summon
// key) is the C0 byte 0x1e in legacy mode, but under the kitty keyboard
// protocol it is a CSI-u sequence (CSI 54;5u — base key '6'=54, ctrl=5, since
// Ctrl-^ is Ctrl+Shift+6 on a US layout). posh's palette-open decision matches
// only the raw 0x1e, so once an app negotiates kitty (which the RFC 0010 daemon
// now lets it do), Ctrl-^ arrives as CSI-u and the palette becomes unreachable.
// Capturing the exact bytes OUTSIDE vs INSIDE posh pins down where 0x1e turns
// into (or stays) a CSI-u sequence.
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
	{"Escape", "expect bare ESC (1b), or CSI 27u under kitty disambiguate"},
	{"Shift+Tab", "a known-distinct control: expect CSI Z (1b 5b 5a) — if THIS is wrong the problem is broad"},
	{"Ctrl+^", "posh#130 palette key: expect C0 1e in legacy mode, or CSI 54;5u under kitty (the palette only matches 1e)"},
	// Alt+Enter dropped: on macOS it is commonly intercepted by a global
	// hotkey daemon (Hammerspoon) before the terminal sees it, so it can't be
	// captured here. Its reference value (ESC+CR) is documented in the
	// posh-term encoder test (enter_encoding_matches_kitty_c0_table) instead.
}

// keyCapture records what one prompt produced.
type keyCapture struct {
	Label string `json:"label"`
	Hex   string `json:"hex"`   // space-separated bytes, e.g. "1b 0d"
	Caret string `json:"caret"` // cat -v style rendering, e.g. "^[^M"
	Gloss string `json:"gloss"` // decoded meaning of a recognized sequence
	raw   []byte // the exact bytes, retained for len checks (not serialized)
}

// rawLen returns the captured bytes (for emptiness checks in the free panel).
func (k keyCapture) rawLen() []byte { return k.raw }

type rawKeysModel struct {
	kittyFlags   string       // reply to CSI ? u, or "(no reply / unsupported)"
	kittyRaw     string       // raw hex of the query reply, for auditing
	captures     []keyCapture // one per scripted prompt, in order
	freeCaptures []keyCapture // free-capture panel, most recent first
	ran          bool
	done         bool // scripted phase complete (free capture is optional after)
	aborted      bool // the user pressed the abort key mid-capture
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
	header := "  Raw key capture complete. Bytes seen (nothing interpreted):\n\n"
	if m.aborted {
		header = "  Raw key capture ABORTED (partial results below):\n\n"
	}
	b.WriteString(header)
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
		"  Compare Ctrl+^ across substates: 1e outside but a CSI-u form inside\n" +
		"  (or vice versa) locates where the palette key changes shape (posh#130).\n" +
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
		"aborted":         m.aborted,
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

// The out-of-band control keys. Advance is decoupled from byte-arrival timing
// entirely: the user presses the key under test (taking any amount of time,
// tolerating multi-byte sequences delivered with gaps), then presses one of
// these to commit / retry / abort. `n` (0x6e) is safe as the commit key because
// none of the scripted chords' encodings contain it (they are \r, \n, ESC, [,
// digits, ;, u, Z). No mouse mode is enabled — that would let posh's own
// MouseFilter intercept the advance click and confound the substate comparison.
const (
	advanceKey = 'n' // commit the pending capture and move to the next prompt
	retryKey   = 'r' // discard the pending capture and re-capture this prompt
	abortKey   = 'q' // stop the capture early
)

func (c *rawKeysCmd) Run() error {
	c.m.captures = c.m.captures[:0]
	c.m.freeCaptures = c.m.freeCaptures[:0]
	c.m.aborted = false

	// Raw mode on the real tty: unbuffered, unprocessed bytes. tea.Exec leaves
	// the terminal cooked, which would line-buffer our per-key reads.
	c.fd = int(os.Stdin.Fd())
	if st, err := term.MakeRaw(c.fd); err == nil {
		defer func() { _ = term.Restore(c.fd, st) }()
	}

	fmt.Fprint(c.out, "\x1b[2J\x1b[H")
	fmt.Fprint(c.out, "  RAW KEY CAPTURE — bytes shown exactly as received.\r\n")
	fmt.Fprintf(c.out, "  Press the prompted key, then '%c' to record and advance.\r\n",
		advanceKey)
	fmt.Fprintf(c.out, "  '%c' re-captures the current key · '%c' aborts. Do not paste.\r\n\r\n",
		retryKey, abortKey)

	c.queryKittyFlags()

	for _, p := range rawKeyScript {
		cap, ctl := c.captureUntilAdvance(p.label, fmt.Sprintf("Press %s  (%s)", p.label, p.hint))
		if ctl == abortKey {
			c.m.aborted = true
			return nil
		}
		c.m.captures = append(c.m.captures, cap)
		fmt.Fprintf(c.out, "    recorded → %-14s %-10s %s\r\n\r\n", cap.Hex, cap.Caret, cap.Gloss)
	}

	fmt.Fprintf(c.out, "  Free capture: press any key then '%c' to record it; '%c' to finish.\r\n\r\n",
		advanceKey, abortKey)
	for i := 0; i < 64; i++ {
		cap, ctl := c.captureUntilAdvance("", "free capture — press a key")
		if ctl == abortKey {
			break
		}
		if len(cap.rawLen()) == 0 {
			continue // committed an empty capture; ignore
		}
		fmt.Fprintf(c.out, "    recorded → %-14s %-10s %s\r\n", cap.Hex, cap.Caret, cap.Gloss)
		c.m.freeCaptures = append([]keyCapture{cap}, c.m.freeCaptures...)
	}

	fmt.Fprintf(c.out, "\r\n  -- capture done; press '%c' to return to posht --", advanceKey)
	c.waitForKey(advanceKey)
	return nil
}

// captureUntilAdvance prompts, then accumulates raw bytes until the user
// presses a control key. It redraws a live hex preview of the pending bytes on
// every byte so the user sees exactly what will be recorded before committing.
// Returns the decoded capture and which control key ended it (advanceKey /
// retryKey resolve to advanceKey after a re-capture; abortKey propagates).
func (c *rawKeysCmd) captureUntilAdvance(label, prompt string) (keyCapture, byte) {
	for {
		fmt.Fprintf(c.out, "  %s\r\n", prompt)
		var pending []byte
		c.drawPending(pending)
		ctl := byte(0)
		for {
			b, ok := c.readByte()
			if !ok { // reader closed (EOF/error): treat as abort
				return decodeCapture(label, pending), abortKey
			}
			switch b {
			case advanceKey:
				ctl = advanceKey
			case retryKey:
				ctl = retryKey
			case abortKey:
				ctl = abortKey
			default:
				pending = append(pending, b)
				c.drawPending(pending)
				continue
			}
			break
		}
		fmt.Fprint(c.out, "\r\n")
		if ctl == retryKey {
			fmt.Fprint(c.out, "    (re-capturing)\r\n")
			continue
		}
		return decodeCapture(label, pending), ctl
	}
}

// drawPending redraws the live preview line (carriage-return overwrite) showing
// the bytes accumulated so far for the current prompt.
func (c *rawKeysCmd) drawPending(pending []byte) {
	fmt.Fprintf(c.out, "\r    pending: %-20s %-12s\x1b[K",
		hexBytes(pending), caretRender(pending))
}

// readByte blocks for the next raw byte read SYNCHRONOUSLY from the tty fd.
// There is deliberately no background reader goroutine: one blocked in
// os.Stdin.Read would survive Run() and steal the next byte from bubbletea when
// the test returns (eating the post-run keypress). Reading inline means nothing
// touches stdin once Run() returns. ok is false on EOF/error.
func (c *rawKeysCmd) readByte() (byte, bool) {
	var one [1]byte
	for {
		n, err := os.Stdin.Read(one[:])
		if n > 0 {
			return one[0], true
		}
		if err != nil {
			return 0, false
		}
	}
}

// waitForKey drains bytes until the given key is seen (or the fd closes).
func (c *rawKeysCmd) waitForKey(key byte) {
	for {
		b, ok := c.readByte()
		if !ok || b == key {
			return
		}
	}
}

// queryKittyFlags sends the kitty protocol query (CSI ? u) followed by a
// primary device-attributes request (CSI c) as a sentinel: a terminal that
// supports the protocol replies CSI ? flags u before the DA reply; one that
// doesn't answers only the DA. Per the spec's detection method. The reply is a
// terminal response (not user input), so it is read on a short timer — the only
// timed read in the harness. It uses a non-blocking fd + poll rather than a
// goroutine, so no reader can outlive this call.
func (c *rawKeysCmd) queryKittyFlags() {
	fmt.Fprint(c.out, "\x1b[?u\x1b[c")
	buf := c.readReply(400 * time.Millisecond)
	c.m.kittyRaw = hexBytes(buf)
	if flags, ok := parseKittyFlagsReply(buf); ok {
		c.m.kittyFlags = flags
	} else {
		c.m.kittyFlags = "(no reply / protocol unsupported)"
	}
	fmt.Fprintf(c.out, "  kitty keyboard flags: %s\r\n\r\n", c.m.kittyFlags)
}

// readReply collects the terminal's auto-emitted query response by temporarily
// putting the fd in non-blocking mode and polling: it waits up to `total` for
// the first byte, then drains whatever else arrives, stopping on a short quiet
// gap. The fd is restored to blocking before returning, so the synchronous
// readByte path (user input) works normally afterward. Used ONLY for the query
// reply, never for user keystrokes (those advance out-of-band via readByte).
func (c *rawKeysCmd) readReply(total time.Duration) []byte {
	if err := syscall.SetNonblock(c.fd, true); err != nil {
		return nil // can't poll; skip the query rather than block forever
	}
	defer func() { _ = syscall.SetNonblock(c.fd, false) }()

	var out []byte
	deadline := time.Now().Add(total)
	quiet := 60 * time.Millisecond
	var one [1]byte
	for {
		n, err := os.Stdin.Read(one[:])
		if n > 0 {
			out = append(out, one[0])
			deadline = time.Now().Add(quiet) // reset the quiet window
			continue
		}
		if err != nil { // EAGAIN (no data yet) or real error
			if len(out) > 0 && time.Now().After(deadline) {
				return out // got a reply, then quiet
			}
			if len(out) == 0 && time.Now().After(deadline) {
				return out // nothing came within the initial window
			}
			time.Sleep(5 * time.Millisecond)
		}
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
		raw:   append([]byte(nil), raw...),
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
	case len(b) == 1 && b[0] == 0x1e:
		return "C0 0x1e — legacy Ctrl-^ (the palette-open byte posh matches)"
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
	case string(b) == "\x1b[54;5u":
		return "CSI 54;5u — kitty Ctrl-^ (report-all; posh does NOT match this, posh#130)"
	case string(b) == "\x1b[Z":
		return "CSI Z — Shift+Tab (legacy back-tab)"
	case string(b) == "\x1b[9;2u":
		return "CSI 9;2u — kitty Shift+Tab (report-all)"
	case len(b) >= 3 && b[0] == 0x1b && b[1] == '[' && b[len(b)-1] == 'u':
		return "a CSI-u sequence (ESC [ … u) — kitty-encoded key"
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
