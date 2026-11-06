package main

import (
	"fmt"
	"io"
	"time"
)

// autoWidth is the fixed render width for --auto and --walk. A constant (not
// the live terminal size) is what makes the output deterministic: the same
// bytes every run and on every terminal, so a posh-vs-ssh recording of
// `posht --auto` is frame-aligned and diffable.
const autoWidth = 80

// walkHold is how long --walk holds each page by default: long enough that a
// posh session samples and syncs a frame showing it (a sub-second exit made
// posh capture a blank frame), short enough to walk every page quickly.
const walkHold = 1500 * time.Millisecond

// staticPage is one selected static test and its renderer.
type staticPage struct {
	test  *Test
	model staticModel
}

// selectedStaticPages returns the selected static (non-interactive) tests in
// registry order. The static tests are pure render functions (colors, attrs,
// gradients, …) with no timers, randomness, or terminal-size dependence, so
// their output is byte-identical across runs.
//
// Interactive/stateful tests (mouse, keys, cursor, altscroll, resize, …) are
// skipped: they need live input or animate, so they can't self-drive into a
// deterministic stream. --only/--skip still apply (they set t.Selected before
// this runs), so `posht --auto --only truecolor` renders just that one.
func selectedStaticPages(tests []*Test) []staticPage {
	var pages []staticPage
	for _, t := range tests {
		if !t.Selected {
			continue
		}
		sm, ok := t.New().(staticModel)
		if !ok {
			continue // interactive/stateful — no deterministic headless render
		}
		pages = append(pages, staticPage{test: t, model: sm})
	}
	return pages
}

// renderPage writes one test's header and its render at width. It resets SGR
// before the header so a previous test can't bleed style into it; the render
// fns already reset internally. \n (not \r\n): neither --auto nor --walk enters
// raw mode, so the pty's ONLCR cooks newlines, same as the interactive path
// relies on Bubble Tea's renderer to do.
func renderPage(p staticPage, width int, w io.Writer) {
	fmt.Fprintf(w, "\x1b[0m── %s · %s ──\n", p.test.ID, p.test.Title)
	fmt.Fprint(w, p.model.render(width))
}

// autoRun renders every selected static test to w at a fixed width, in
// registry order, as one scrolling primary-screen stream, and returns how
// many it rendered.
func autoRun(tests []*Test, width int, w io.Writer) int {
	pages := selectedStaticPages(tests)
	for _, p := range pages {
		renderPage(p, width, w)
		fmt.Fprint(w, "\n")
	}
	return len(pages)
}

// walkRun renders the same pages as autoRun, one per alternate-screen page:
// it enters the alt screen, and for each page clears it, renders the test from
// home, and holds for hold (via sleep) before the next; after the last it
// leaves the alt screen. It returns how many pages it rendered.
//
// This is the vehicle for posh-vs-ssh diffing that --auto cannot be: a posh
// session takes the alt screen over with no scrollback, so --auto's scrolling
// dump is lost, and its sub-second exit leaves posh sampling a blank frame.
// One page per test needs no scrollback, and the fixed hold gives posh time to
// sync each page, so a recording can be stepped frame by frame and diffed per
// test. Fixed renders, fixed page order and a fixed hold keep it
// deterministic.
func walkRun(tests []*Test, width int, hold time.Duration, w io.Writer, sleep func(time.Duration)) int {
	pages := selectedStaticPages(tests)
	fmt.Fprint(w, "\x1b[?1049h")
	for _, p := range pages {
		fmt.Fprint(w, "\x1b[0m\x1b[H\x1b[2J")
		renderPage(p, width, w)
		sleep(hold)
	}
	fmt.Fprint(w, "\x1b[0m\x1b[?1049l")
	return len(pages)
}
