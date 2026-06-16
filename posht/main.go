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
	"path/filepath"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

// Provenance, flowed from the repo's version.env (POSH_VERSION) + git rev by
// the nix flake via -ldflags -X (github #71). They are `var` (not `const`) so
// `-X` can override them; the "-dev"/"unknown" defaults mark a non-nix
// `go build`, mirroring the Rust crates' inert 0.0.0 placeholder. See
// eng-versioning(7).
var (
	version = "0.0.0-dev"
	gitSHA  = "unknown"
)

// versionLine is the `posht --version` / report provenance string,
// `posht <version> (<sha>)`. Factored out so the provenance guard test
// (version_test.go, github #71) checks the exact format main prints.
func versionLine() string {
	return "posht " + version + " (" + gitSHA + ")"
}

func main() {
	list := flag.Bool("list", false, "print test IDs and titles, then exit")
	only := flag.String("only", "", "comma-separated test IDs: select just these")
	skip := flag.String("skip", "", "comma-separated test IDs: start deselected")
	auto := flag.Bool("auto", false, "non-interactively render the selected static "+
		"capability tests to stdout at a fixed width, then exit (deterministic — for "+
		"posh-vs-ssh recording diffs)")
	out := flag.String("o", "", "also write the markdown report to this file")
	jsonOut := flag.String("json", "", "JSON receipt destination: a file path, \"-\" for stdout, "+
		"or omit for the default ~/.local/log/posht/<datetime>-<term>.json")
	ver := flag.Bool("version", false, "print version, then exit")
	flag.Parse()

	tests := registry()

	if *ver {
		fmt.Println(versionLine())
		return
	}
	if *list {
		for _, t := range tests {
			fmt.Printf("%-14s %s\n", t.ID, t.Title)
		}
		return
	}
	if err := applyFilters(tests, *only, *skip); err != nil {
		fmt.Fprintln(os.Stderr, "posht:", err)
		os.Exit(64)
	}

	// --auto: deterministic, non-interactive render of the selected static
	// tests, then exit. No Bubble Tea, no alt screen, no receipt — the point is
	// a reproducible byte stream to record over posh vs ssh and diff.
	if *auto {
		autoRun(tests, autoWidth, os.Stdout)
		return
	}

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

	// Walk the process ancestry once and share it between the receipt body
	// and the default filename (which derives the real terminal from it).
	tree := processTree()

	// Resolve the JSON receipt destination:
	//   --json -      → stdout (and suppress the markdown report there)
	//   --json <path> → that file
	//   (omitted)     → the default ~/.local/log/posht/<datetime>-<term>.json
	// so every run leaves a machine-readable trail without being asked.
	jsonDest := *jsonOut
	if jsonDest == "" {
		jsonDest = defaultReceiptPath(tree) // "" only if $HOME can't be resolved
	}

	// stdout is the receipt: its JSON when asked for inline (--json -), else
	// just the path it was written to (the useful handle — the contents are
	// in the file). The markdown report is no longer auto-dumped to stdout;
	// it is produced only when -o asks for it.
	rec := reportJSON(tests, root.details, tree)
	switch jsonDest {
	case "-":
		fmt.Print(rec)
	case "":
		fmt.Fprintln(os.Stderr, "posht: no home dir for default receipt; skipping JSON (use --json -)")
	default:
		if err := os.MkdirAll(filepath.Dir(jsonDest), 0o755); err != nil {
			fmt.Fprintln(os.Stderr, "posht: create receipt dir:", err)
			os.Exit(1)
		}
		if err := os.WriteFile(jsonDest, []byte(rec), 0o644); err != nil {
			fmt.Fprintln(os.Stderr, "posht: write json receipt:", err)
			os.Exit(1)
		}
		fmt.Println(jsonDest)
	}

	if *out != "" {
		if err := os.WriteFile(*out, []byte(reportMD(tests)), 0o644); err != nil {
			fmt.Fprintln(os.Stderr, "posht: write report:", err)
			os.Exit(1)
		}
	}
	if failed(tests) {
		os.Exit(1)
	}
}

func applyFilters(tests []*Test, only, skip string) error {
	known := make(map[string]bool, len(tests))
	for _, t := range tests {
		known[t.ID] = true
	}
	if only != "" {
		want := splitIDs(only)
		// A typo'd ID must not silently select nothing and exit 0.
		if err := checkIDs(want, known, "--only"); err != nil {
			return err
		}
		for _, t := range tests {
			t.Selected = want[t.ID]
		}
	}
	if skip != "" {
		drop := splitIDs(skip)
		if err := checkIDs(drop, known, "--skip"); err != nil {
			return err
		}
		for _, t := range tests {
			if drop[t.ID] {
				t.Selected = false
			}
		}
	}
	return nil
}

func checkIDs(ids, known map[string]bool, flagName string) error {
	for id := range ids {
		if !known[id] {
			return fmt.Errorf("%s: unknown test id %q (see --list)", flagName, id)
		}
	}
	return nil
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
