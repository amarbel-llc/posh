package main

import (
	"bytes"
	"strings"
	"testing"
)

// The whole point of --auto is a reproducible byte stream, so two runs at the
// same width must be byte-identical.
func TestAutoRunIsDeterministic(t *testing.T) {
	var a, b bytes.Buffer
	na := autoRun(registry(), autoWidth, &a)
	nb := autoRun(registry(), autoWidth, &b)
	if na == 0 {
		t.Fatal("autoRun rendered no tests")
	}
	if na != nb {
		t.Fatalf("autoRun rendered a different count across runs: %d vs %d", na, nb)
	}
	if a.String() != b.String() {
		t.Fatal("autoRun output differs across runs at the same width")
	}
}

// Static visual tests must be rendered; interactive/stateful tests must be
// skipped (they can't self-drive into a deterministic stream).
func TestAutoRunRendersStaticSkipsInteractive(t *testing.T) {
	var buf bytes.Buffer
	autoRun(registry(), autoWidth, &buf)
	out := buf.String()

	for _, id := range []string{"colors16", "colors256", "truecolor", "attrs", "underline", "wide"} {
		if !strings.Contains(out, "── "+id+" ·") {
			t.Errorf("autoRun output missing static test header for %q", id)
		}
	}
	for _, id := range []string{"mouse", "keys", "altscroll", "resize", "cursor"} {
		if strings.Contains(out, "── "+id+" ·") {
			t.Errorf("autoRun unexpectedly rendered interactive test %q", id)
		}
	}
}

// --only / --skip flow through to --auto via t.Selected.
func TestAutoRunRespectsSelection(t *testing.T) {
	tests := registry()
	if err := applyFilters(tests, "truecolor", ""); err != nil {
		t.Fatal(err)
	}
	var buf bytes.Buffer
	n := autoRun(tests, autoWidth, &buf)
	if n != 1 {
		t.Fatalf("--only truecolor should render exactly 1 test, got %d", n)
	}
	if !strings.Contains(buf.String(), "── truecolor ·") {
		t.Error("autoRun did not render the selected truecolor test")
	}
}
