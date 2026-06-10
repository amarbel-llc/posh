// Command posht ("diff on a POSH") is an interactive terminal-capability
// test. It renders each feature the posh terminal stack claims to support
// and asks the human on the other end to confirm what they actually see.
//
// It is built as a static binary so posh can scp it to a remote host and
// run it inside a posh session there — putting the whole pipeline (remote
// pty, terminal emulation, transport sync, local render) in the loop
// being judged. It starts with a checklist of features that the user can
// deselect, then walks through one test per screen (pass/fail/skip), and
// prints a markdown report on exit.
package main

import (
	"flag"
	"fmt"
	"os"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

const version = "0.1.0"

func main() {
	list := flag.Bool("list", false, "print test IDs and titles, then exit")
	only := flag.String("only", "", "comma-separated test IDs: select just these")
	skip := flag.String("skip", "", "comma-separated test IDs: start deselected")
	out := flag.String("o", "", "also write the markdown report to this file")
	ver := flag.Bool("version", false, "print version, then exit")
	flag.Parse()

	tests := registry()

	if *ver {
		fmt.Println("posht " + version)
		return
	}
	if *list {
		for _, t := range tests {
			fmt.Printf("%-14s %s\n", t.ID, t.Title)
		}
		return
	}
	applyFilters(tests, *only, *skip)

	p := tea.NewProgram(newRoot(tests), tea.WithAltScreen())
	res, err := p.Run()
	if err != nil {
		fmt.Fprintln(os.Stderr, "posht:", err)
		os.Exit(1)
	}

	root := res.(*rootModel)
	if !root.ran {
		return // quit from the checklist; nothing to report
	}
	rep := reportMD(tests)
	fmt.Print(rep)
	if *out != "" {
		if err := os.WriteFile(*out, []byte(rep), 0o644); err != nil {
			fmt.Fprintln(os.Stderr, "posht: write report:", err)
			os.Exit(1)
		}
	}
	if failed(tests) {
		os.Exit(1)
	}
}

func applyFilters(tests []*Test, only, skip string) {
	if only != "" {
		want := splitIDs(only)
		for _, t := range tests {
			t.Selected = want[t.ID]
		}
	}
	if skip != "" {
		drop := splitIDs(skip)
		for _, t := range tests {
			if drop[t.ID] {
				t.Selected = false
			}
		}
	}
}

func splitIDs(s string) map[string]bool {
	m := make(map[string]bool)
	for _, id := range strings.Split(s, ",") {
		if id = strings.TrimSpace(id); id != "" {
			m[id] = true
		}
	}
	return m
}

func failed(tests []*Test) bool {
	for _, t := range tests {
		if t.Selected && t.Verdict == Fail {
			return true
		}
	}
	return false
}
