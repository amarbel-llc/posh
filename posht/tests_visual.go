package main

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

// staticModel renders a fixed scene; the user just judges it.
type staticModel struct {
	render func(width int) string
}

func (m staticModel) Init() tea.Cmd                       { return nil }
func (m staticModel) Update(tea.Msg) (TestModel, tea.Cmd) { return m, nil }
func (m staticModel) View(w int) string                   { return m.render(w) }

const reset = "\x1b[0m"

func colors16View(int) string {
	var b strings.Builder
	b.WriteString("  Every cell below must be a distinct, solid color.\n\n")

	b.WriteString("  fg standard: ")
	for c := 30; c <= 37; c++ {
		fmt.Fprintf(&b, "\x1b[%dm▉▉%d%s ", c, c-30, reset)
	}
	b.WriteString("\n  fg bright:   ")
	for c := 90; c <= 97; c++ {
		fmt.Fprintf(&b, "\x1b[%dm▉▉%d%s ", c, c-90, reset)
	}
	b.WriteString("\n\n  bg standard: ")
	for c := 40; c <= 47; c++ {
		fmt.Fprintf(&b, "\x1b[%dm %d %s ", c, c-40, reset)
	}
	b.WriteString("\n  bg bright:   ")
	for c := 100; c <= 107; c++ {
		fmt.Fprintf(&b, "\x1b[%dm %d %s ", c, c-100, reset)
	}
	b.WriteString("\n")
	return b.String()
}

func colors256View(int) string {
	var b strings.Builder
	b.WriteString("  System colors, a 6x6x6 cube, and a smooth grayscale ramp.\n\n  ")
	for i := 0; i < 16; i++ {
		fmt.Fprintf(&b, "\x1b[48;5;%dm  ", i)
	}
	b.WriteString(reset + "\n\n")
	for row := 0; row < 6; row++ {
		b.WriteString("  ")
		for col := 0; col < 36; col++ {
			fmt.Fprintf(&b, "\x1b[48;5;%dm  ", 16+(col/6)*36+row*6+col%6)
		}
		b.WriteString(reset + "\n")
	}
	b.WriteString("\n  ")
	for i := 232; i <= 255; i++ {
		fmt.Fprintf(&b, "\x1b[48;5;%dm   ", i)
	}
	b.WriteString(reset + "\n")
	return b.String()
}

func truecolorView(width int) string {
	w := width - 6
	if w < 16 {
		w = 16
	}
	if w > 96 {
		w = 96
	}
	var b strings.Builder
	b.WriteString("  Each bar must be a perfectly smooth gradient — no banding.\n\n")
	bar := func(label string, rgb func(v int) (int, int, int), colon bool) {
		fmt.Fprintf(&b, "  %-18s", label)
		for i := 0; i < w; i++ {
			r, g, bl := rgb(i * 255 / (w - 1))
			if colon {
				fmt.Fprintf(&b, "\x1b[48:2::%d:%d:%dm ", r, g, bl)
			} else {
				fmt.Fprintf(&b, "\x1b[48;2;%d;%d;%dm ", r, g, bl)
			}
		}
		b.WriteString(reset + "\n")
	}
	bar("red (38;2 form)", func(v int) (int, int, int) { return v, 0, 0 }, false)
	bar("green", func(v int) (int, int, int) { return 0, v, 0 }, false)
	bar("blue", func(v int) (int, int, int) { return 0, 0, v }, false)
	bar("gray (38:2:: form)", func(v int) (int, int, int) { return v, v, v }, true)
	b.WriteString("\n  The gray bar uses the colon SGR form — it must look as\n" +
		"  smooth as the others, not fall back to odd colors.\n")
	return b.String()
}

func attrsView(int) string {
	rows := []struct{ sgr, label, expect string }{
		{"1", "bold", "heavier weight"},
		{"2", "dim", "fainter"},
		{"3", "italic", "slanted"},
		{"4", "underline", "underlined"},
		{"5", "blink", "blinking (some terminals disable this)"},
		{"7", "reverse", "fg/bg swapped"},
		{"8", "hidden", "invisible (the brackets stay)"},
		{"9", "strikethrough", "struck through"},
		{"1;3;4", "bold+italic+under", "all three combined"},
	}
	var b strings.Builder
	b.WriteString("  Each sample must show its attribute; text after it must be plain.\n\n")
	for _, r := range rows {
		fmt.Fprintf(&b, "  %-19s [\x1b[%sm%s%s] — %s\n",
			r.label, r.sgr, "sample text", reset, r.expect)
	}
	return b.String()
}

func underlineView(int) string {
	rows := []struct{ sgr, label string }{
		{"4:1", "single"},
		{"4:2", "double"},
		{"4:3", "curly"},
		{"4:4", "dotted"},
		{"4:5", "dashed"},
	}
	var b strings.Builder
	b.WriteString("  Styled underlines (colon-form SGR 4:n). Terminals without a\n" +
		"  style fall back to a plain underline — bare text is a failure.\n\n")
	for _, r := range rows {
		fmt.Fprintf(&b, "  %-8s \x1b[%smunderlined sample%s\n", r.label, r.sgr, reset)
	}
	b.WriteString(fmt.Sprintf("\n  colored  \x1b[4:3m\x1b[58:2::255:80:80mred curly underline (SGR 58)%s\n", reset))
	return b.String()
}

func wideView(int) string {
	var b strings.Builder
	b.WriteString("  Wide characters must occupy exactly two columns each:\n" +
		"  all four | bars must line up under the ▼ marker.\n\n")
	b.WriteString("            1234567890▼\n")
	b.WriteString("  CJK       漢字テスト|\n")
	b.WriteString("  emoji     🦀🚀🌍🎉⏰|\n")
	b.WriteString("  hangul    한글테스트|\n")
	b.WriteString("  narrow    0123456789|\n")
	b.WriteString("\n  ZWJ cluster (rendering varies; should be one glyph, not parts):\n")
	b.WriteString("  family    👨‍👩‍👧‍👦|\n")
	return b.String()
}

func combiningView(int) string {
	var b strings.Builder
	b.WriteString("  Combining marks must stack onto the base character —\n" +
		"  one column each, never a separate cell.\n\n")
	b.WriteString("  e + ◌́  →  é      n + ◌̃  →  ñ      a + ◌̈  →  ä\n\n")
	b.WriteString("  All three | bars must align:\n")
	b.WriteString("  plain     abcde|\n")
	b.WriteString("  combined  áb́ćd́é|\n")
	b.WriteString("  stacked   á̧b̧́ḉḑ́ȩ́|\n")
	return b.String()
}

func boxdrawView(int) string {
	const so, si = "\x1b(0", "\x1b(B" // select DEC special graphics / ASCII
	var b strings.Builder
	b.WriteString("  Both boxes must look identical — clean line-drawn frames.\n" +
		"  Letters (lqkxmj) in the left box mean the DEC charset is broken.\n\n")
	b.WriteString("  DEC special graphics     Unicode box drawing\n")
	b.WriteString("  " + so + "lqqqqqqqqk" + si + "               ┌────────┐\n")
	b.WriteString("  " + so + "x" + si + " posh   " + so + "x" + si + "               │ posh   │\n")
	b.WriteString("  " + so + "tqqqqqqqqu" + si + "               ├────────┤\n")
	b.WriteString("  " + so + "x" + si + " posht  " + so + "x" + si + "               │ posht  │\n")
	b.WriteString("  " + so + "mqqqqqqqqj" + si + "               └────────┘\n")
	return b.String()
}

func hyperlinkView(int) string {
	const link = "\x1b]8;;https://github.com/amarbel-llc/posh\x1b\\"
	const end = "\x1b]8;;\x1b\\"
	var b strings.Builder
	b.WriteString("  The text below is an OSC 8 hyperlink. It must render as the\n" +
		"  plain words (no escape garbage); in terminals with hyperlink\n" +
		"  support, hovering/ctrl-clicking it opens the posh repository.\n\n")
	b.WriteString("      " + link + "posh on GitHub" + end + "\n\n")
	b.WriteString("  Escape garbage around the words = fail. Plain but\n" +
		"  unclickable text is a pass if your terminal lacks hyperlinks.\n")
	return b.String()
}
