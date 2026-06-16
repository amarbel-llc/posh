package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// reportMD renders the human-readable markdown run report. It is written to
// the -o file when that flag is given; it is no longer printed to stdout (the
// JSON receipt, or its path, owns stdout — see main).
func reportMD(tests []*Test) string {
	var b strings.Builder
	host, _ := os.Hostname()
	b.WriteString("# POSHT terminal capability report\n\n")
	fmt.Fprintf(&b, "- date: %s\n", time.Now().Format(time.RFC3339))
	fmt.Fprintf(&b, "- host: %s\n", host)
	fmt.Fprintf(&b, "- TERM: %s, COLORTERM: %s\n",
		os.Getenv("TERM"), os.Getenv("COLORTERM"))
	// Run-mode breadcrumbs: without these a report can't be classified
	// as baseline vs in-posh, which decides whether a FAIL is a terminal
	// quirk or a posh gap.
	if s := os.Getenv("POSH_SESSION"); s != "" {
		fmt.Fprintf(&b, "- POSH_SESSION: %s\n", s)
	}
	if c := os.Getenv("SSH_CONNECTION"); c != "" {
		fmt.Fprintf(&b, "- SSH_CONNECTION: %s\n", c)
	}
	fmt.Fprintf(&b, "- posht: v%s (%s)\n\n", version, gitSHA)

	b.WriteString("| test | feature | result |\n|---|---|---|\n")
	var pass, fail int
	for _, t := range tests {
		result := t.Verdict.String()
		if !t.Selected {
			result = "deselected"
		}
		switch t.Verdict {
		case Pass:
			pass++
		case Fail:
			fail++
		}
		fmt.Fprintf(&b, "| %s | %s | %s |\n", t.ID, t.Title, result)
	}
	fmt.Fprintf(&b, "\n**%d pass, %d fail** out of %d tests.\n", pass, fail, len(tests))
	if fail > 0 {
		b.WriteString("\nFailures worth cross-checking against the known gaps in" +
			" `docs/manual-testing.md` before filing new issues.\n")
	}
	return b.String()
}

// receipt is the machine-readable run record (the --json output). It carries
// the same run-context breadcrumbs as the markdown report plus the process
// ancestry (so a reader can tell bare-terminal from in-posh/in-zmx runs
// without trusting env vars) and any per-test structured detail.
type receipt struct {
	Date        string         `json:"date"`
	Host        string         `json:"host"`
	Term        string         `json:"term"`
	Colorterm   string         `json:"colorterm"`
	PoshSession string         `json:"posh_session,omitempty"`
	SSHConn     string         `json:"ssh_connection,omitempty"`
	Version     string         `json:"posht_version"`
	GitSHA      string         `json:"posht_git_sha"`
	ProcessTree []procInfo     `json:"process_tree"`
	Tests       []testResult   `json:"tests"`
	Details     map[string]any `json:"details,omitempty"`
}

type testResult struct {
	ID       string `json:"id"`
	Title    string `json:"title"`
	Selected bool   `json:"selected"`
	Verdict  string `json:"verdict"`
}

// defaultReceiptPath is where the JSON receipt lands when --json is not
// given: ~/.local/log/posht/<datetime>-<terminal>.json. The datetime is a
// filename-safe local timestamp; the terminal label comes from the process
// tree (terminalFromTree) rather than $TERM, because $TERM lies on macOS
// (iTerm2 and Terminal.app both inherit "xterm-kitty"). Falls back to $TERM
// only when the tree names no known terminal. Returns "" if the home dir
// can't be resolved (the caller then falls back to stdout).
func defaultReceiptPath(tree []procInfo) string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return ""
	}
	term := terminalFromTree(tree)
	if term == "" {
		term = os.Getenv("TERM")
	}
	if term == "" {
		term = "unknown"
	}
	stamp := time.Now().Format("20060102-150405")
	name := fmt.Sprintf("%s-%s.json", stamp, sanitizeForFilename(term))
	return filepath.Join(home, ".local", "log", "posht", name)
}

// sanitizeForFilename reduces a value to characters safe in a path segment,
// collapsing anything else to '-' so $TERM values like "xterm-kitty" survive
// intact while exotic ones can't escape the directory or confuse the shell.
func sanitizeForFilename(s string) string {
	var b strings.Builder
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9',
			r == '-', r == '_', r == '.':
			b.WriteRune(r)
		default:
			b.WriteRune('-')
		}
	}
	if b.Len() == 0 {
		return "unknown"
	}
	return b.String()
}

// reportJSON renders the indented JSON receipt. details is the root model's
// per-test reporter output, keyed by test ID; tree is the already-walked
// process ancestry (shared with defaultReceiptPath so ps isn't walked twice).
func reportJSON(tests []*Test, details map[string]any, tree []procInfo) string {
	host, _ := os.Hostname()
	r := receipt{
		Date:        time.Now().Format(time.RFC3339),
		Host:        host,
		Term:        os.Getenv("TERM"),
		Colorterm:   os.Getenv("COLORTERM"),
		PoshSession: os.Getenv("POSH_SESSION"),
		SSHConn:     os.Getenv("SSH_CONNECTION"),
		Version:     version,
		GitSHA:      gitSHA,
		ProcessTree: tree,
		Details:     details,
	}
	for _, t := range tests {
		verdict := t.Verdict.String()
		if !t.Selected {
			verdict = "deselected"
		}
		r.Tests = append(r.Tests, testResult{
			ID: t.ID, Title: t.Title, Selected: t.Selected, Verdict: verdict,
		})
	}
	out, err := json.MarshalIndent(r, "", "  ")
	if err != nil {
		// MarshalIndent only fails on unencodable detail values; degrade to
		// a receipt without details rather than losing the whole run.
		r.Details = nil
		out, _ = json.MarshalIndent(r, "", "  ")
	}
	return string(out) + "\n"
}
