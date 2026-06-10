package main

import (
	"bufio"
	"fmt"
	"io"
	"os"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"golang.org/x/term"
)

// Raw tests can't run through the TUI renderer (it never lets a line reach
// the real last column, and it owns the scrolling). tea.Exec releases the
// terminal — cooked mode, primary screen — runs the demo, and restores the
// TUI afterwards, where the user records the verdict.

type rawDoneMsg struct{}

type rawModel struct {
	demo   func(out io.Writer)
	expect string
	runs   int
}

func newRawModel(demo func(io.Writer), expect string) TestModel {
	return &rawModel{demo: demo, expect: expect}
}

func (m *rawModel) exec() tea.Cmd {
	return tea.Exec(&rawCmd{fn: m.demo}, func(error) tea.Msg { return rawDoneMsg{} })
}

func (m *rawModel) Init() tea.Cmd { return m.exec() }

func (m *rawModel) Update(msg tea.Msg) (TestModel, tea.Cmd) {
	switch msg := msg.(type) {
	case rawDoneMsg:
		m.runs++
	case tea.KeyMsg:
		if msg.String() == "r" {
			return m, m.exec()
		}
	}
	return m, nil
}

func (m *rawModel) View(int) string {
	if m.runs == 0 {
		return "  running the raw demo on the primary screen…\n"
	}
	return "  The demo ran on your primary screen. You should have seen:\n\n" +
		m.expect + "\n  Press r to run it again, then record the verdict.\n"
}

// rawCmd adapts a plain writer function to tea.ExecCommand.
type rawCmd struct {
	fn  func(io.Writer)
	in  io.Reader
	out io.Writer
}

func (c *rawCmd) Run() error {
	c.fn(c.out)
	fmt.Fprint(c.out, "\r\n  -- press Enter to return to posht --")
	_, err := bufio.NewReader(c.in).ReadString('\n')
	return err
}

func (c *rawCmd) SetStdin(r io.Reader)  { c.in = r }
func (c *rawCmd) SetStdout(w io.Writer) { c.out = w }
func (c *rawCmd) SetStderr(io.Writer)   {}

func termWidth() int {
	if w, _, err := term.GetSize(int(os.Stdout.Fd())); err == nil && w > 8 {
		return w
	}
	return 80
}

// --- autowrap / last column ----------------------------------------------

func wrapDemo(out io.Writer) {
	w := termWidth()
	ruler := make([]byte, w)
	for i := range ruler {
		ruler[i] = byte('0' + (i+1)%10)
	}
	ruler[w-1] = '>'

	fmt.Fprintf(out, "\r\n1) A ruler of exactly %d columns — it must fill ONE line,\r\n"+
		"   ending with '>' in the last column, then 'X' lands on the next line:\r\n\r\n", w)
	out.Write(ruler)
	fmt.Fprint(out, "X  <- deferred wrap: this X must be in column 1\r\n")

	fmt.Fprint(out, "\r\n2) Same ruler, then CR + 'S' WITHOUT a newline — the wrap must\r\n"+
		"   still be pending, so 'S' overwrites column 1 of the SAME line:\r\n\r\n")
	out.Write(ruler)
	fmt.Fprint(out, "\rS\r\n")
}

const wrapExpect = "" +
	"    1) the ruler exactly filled one line, '>' sat in the last\n" +
	"       column, and 'X' started the next line (no blank line\n" +
	"       between — that would be a premature wrap);\n" +
	"    2) the second ruler line begins with 'S' and still ends\n" +
	"       with '>' — the pending wrap was cancelled by CR, not\n" +
	"       committed early.\n"

// --- scroll regions (DECSTBM) ----------------------------------------------

func scrollDemo(out io.Writer) {
	fmt.Fprint(out, "\x1b[2J\x1b[H")
	fmt.Fprint(out, "=== row 1: top guard — must never move ===\r\n")
	fmt.Fprint(out, "\x1b[9;1H=== row 9: bottom guard — must never move ===")
	fmt.Fprint(out, "\x1b[2;8r") // margins: rows 2-8
	fmt.Fprint(out, "\x1b[8;1H") // bottom of the region
	for i := 1; i <= 16; i++ {
		fmt.Fprintf(out, "scrolling line %02d of 16 — only rows 2-8 may move\r\n", i)
		time.Sleep(90 * time.Millisecond)
	}
	fmt.Fprint(out, "\x1b[r")     // clear margins
	fmt.Fprint(out, "\x1b[11;1H") // park below the guards for the prompt
}

const scrollExpect = "" +
	"    - the row-1 and row-9 guard lines never moved;\n" +
	"    - 16 numbered lines scrolled smoothly, confined to rows 2-8;\n" +
	"    - lines 10-16 were left visible inside the region at the end.\n"
