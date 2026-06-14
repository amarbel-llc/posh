package main

import (
	"fmt"
	"io"
)

// autoWidth is the fixed render width for --auto. A constant (not the live
// terminal size) is what makes the output deterministic: the same bytes every
// run and on every terminal, so a posh-vs-ssh recording of `posht --auto` is
// frame-aligned and diffable.
const autoWidth = 80

// autoRun renders every selected static (non-interactive) test to w at a fixed
// width, in registry order, and returns how many it rendered. The static tests
// are pure render functions (colors, attrs, gradients, …) with no timers,
// randomness, or terminal-size dependence, so the output is byte-identical
// across runs.
//
// Interactive/stateful tests (mouse, keys, cursor, altscroll, resize, …) are
// skipped: they need live input or animate, so they can't self-drive into a
// deterministic stream. --only/--skip still apply (they set t.Selected before
// this runs), so `posht --auto --only truecolor` renders just that one.
func autoRun(tests []*Test, width int, w io.Writer) int {
	rendered := 0
	for _, t := range tests {
		if !t.Selected {
			continue
		}
		sm, ok := t.New().(staticModel)
		if !ok {
			continue // interactive/stateful — no deterministic headless render
		}
		// Reset SGR before the header so a previous test can't bleed style into
		// it; the render fns already reset internally. \n (not \r\n): --auto does
		// not enter raw mode, so the pty's ONLCR cooks newlines, same as the
		// interactive path relies on Bubble Tea's renderer to do.
		fmt.Fprintf(w, "\x1b[0m── %s · %s ──\n", t.ID, t.Title)
		fmt.Fprint(w, sm.render(width))
		fmt.Fprint(w, "\n")
		rendered++
	}
	return rendered
}
