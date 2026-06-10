package main

import (
	"fmt"
	"os"
	"strings"
	"time"
)

// reportMD renders the markdown run report printed to stdout on exit
// (and written to the -o file when given).
func reportMD(tests []*Test) string {
	var b strings.Builder
	host, _ := os.Hostname()
	b.WriteString("# POSHT terminal capability report\n\n")
	fmt.Fprintf(&b, "- date: %s\n", time.Now().Format(time.RFC3339))
	fmt.Fprintf(&b, "- host: %s\n", host)
	fmt.Fprintf(&b, "- TERM: %s, COLORTERM: %s\n",
		os.Getenv("TERM"), os.Getenv("COLORTERM"))
	fmt.Fprintf(&b, "- posht: v%s\n\n", version)

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
