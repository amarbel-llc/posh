package main

import (
	"bytes"
	"strings"
	"testing"
	"time"
)

// walkOnce runs walkRun with a recording sleep and returns its output, page
// count, and the holds it asked for.
func walkOnce(tests []*Test) (string, int, []time.Duration) {
	var buf bytes.Buffer
	var holds []time.Duration
	n := walkRun(tests, autoWidth, walkHold, &buf, func(d time.Duration) { holds = append(holds, d) })
	return buf.String(), n, holds
}

// --walk must be as reproducible as --auto: same bytes, same page count.
func TestWalkRunIsDeterministic(t *testing.T) {
	a, na, _ := walkOnce(registry())
	b, nb, _ := walkOnce(registry())
	if na == 0 {
		t.Fatal("walkRun rendered no pages")
	}
	if na != nb || a != b {
		t.Fatal("walkRun output differs across runs at the same width")
	}
}

// The walk owns the alt screen for its whole run, and each test gets its own
// page: cleared from home, then held once for the fixed hold.
func TestWalkRunRendersOnePageAndOneHoldPerStaticTest(t *testing.T) {
	out, n, holds := walkOnce(registry())
	if want := len(selectedStaticPages(registry())); n != want {
		t.Fatalf("walkRun rendered %d pages, want one per static test (%d)", n, want)
	}
	if !strings.HasPrefix(out, "\x1b[?1049h") || !strings.HasSuffix(out, "\x1b[?1049l") {
		t.Error("walkRun must enter the alt screen first and leave it last")
	}
	if got := strings.Count(out, "\x1b[H\x1b[2J"); got != n {
		t.Errorf("walkRun cleared %d pages, want %d", got, n)
	}
	if len(holds) != n {
		t.Fatalf("walkRun held %d times, want once per page (%d)", len(holds), n)
	}
	for _, h := range holds {
		if h != walkHold {
			t.Errorf("walkRun held %v, want the fixed %v", h, walkHold)
		}
	}
	for _, id := range []string{"mouse", "keys", "altscroll", "resize", "cursor"} {
		if strings.Contains(out, "── "+id+" ·") {
			t.Errorf("walkRun unexpectedly rendered interactive test %q", id)
		}
	}
}

// One page per test only avoids scroll loss if every page fits a 24-row
// screen: at most 23 newlines, so the last line never scrolls the header off.
func TestWalkPagesFitOneScreenful(t *testing.T) {
	for _, p := range selectedStaticPages(registry()) {
		var buf bytes.Buffer
		renderPage(p, autoWidth, &buf)
		if lines := strings.Count(buf.String(), "\n"); lines > 23 {
			t.Errorf("page %q has %d newlines; it would scroll a 24-row screen", p.test.ID, lines)
		}
	}
}

// --only / --skip flow through to --walk via t.Selected, as they do to --auto.
func TestWalkRunRespectsSelection(t *testing.T) {
	tests := registry()
	if err := applyFilters(tests, "truecolor", ""); err != nil {
		t.Fatal(err)
	}
	out, n, _ := walkOnce(tests)
	if n != 1 || !strings.Contains(out, "── truecolor ·") {
		t.Fatalf("--only truecolor should walk exactly the truecolor page, got %d pages", n)
	}
}

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
