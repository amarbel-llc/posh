package main

import (
	"strings"
	"testing"
)

// Provenance guard (github #71): posht must report both a version and a git
// sha, formatted `posht <version> (<sha>)`. Under plain `go test` (no ldflags)
// the components are the inert dev defaults, but both are non-empty and the
// shape must hold; the nix build flows the real values via -ldflags -X. A build
// product shipping without version+sha provenance trips this. See
// eng-versioning(7).
func TestVersionLineReportsVersionAndSHA(t *testing.T) {
	line := versionLine()
	rest, ok := strings.CutPrefix(line, "posht ")
	if !ok {
		t.Fatalf("missing %q prefix: %q", "posht ", line)
	}
	ver, sha, ok := strings.Cut(rest, " (")
	if !ok {
		t.Fatalf("missing \" (<sha>)\": %q", line)
	}
	sha, ok = strings.CutSuffix(sha, ")")
	if !ok {
		t.Fatalf("missing closing \")\": %q", line)
	}
	if ver == "" {
		t.Errorf("empty version in %q", line)
	}
	if sha == "" {
		t.Errorf("empty git sha in %q", line)
	}
}
