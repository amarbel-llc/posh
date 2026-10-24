package main

import (
	"encoding/json"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
)

// The countdown view (PROTOTYPE, docs/plans/2026-10-24-app-modals-for-restart.md):
// a title, a body, the time the DAEMON says is left, and a row of buttons. The
// renderer keeps no clock of its own — it draws what the client last told it.

func countdownShow(remainingMs int64) showMsg {
	return showMsg{
		View:  "countdown",
		Title: "Restarting agent",
		Body:  "The agent restarts when the countdown ends.",
		Commands: []command{
			{Name: "Cancel restart", Action: &action{Method: "app.modal.choose", Params: json.RawMessage(`{"id":"r1","button":"cancel"}`)}},
			{Name: "Restart now", Action: &action{Method: "app.modal.choose", Params: json.RawMessage(`{"id":"r1","button":"now"}`)}},
		},
		RemainingMs: remainingMs,
		TotalMs:     30000,
	}
}

func shownCountdown(t *testing.T, c *conn, remainingMs int64) model {
	t.Helper()
	m, _ := newModel(c).Update(countdownShow(remainingMs))
	return m.(model)
}

func TestCountdownIsAKnownView(t *testing.T) {
	if !knownView("countdown") {
		t.Fatal("countdown must be a known view (an old renderer answers -32602 and the client degrades to its banner)")
	}
}

func TestCountdownRendersTitleBodyTimeAndButtons(t *testing.T) {
	c, _ := captureConn(t)
	out := plain(shownCountdown(t, c, 26400).View().Content)
	for _, want := range []string{"Restarting agent", "restarts when the countdown ends", "27s", "Cancel restart", "Restart now"} {
		if !strings.Contains(out, want) {
			t.Errorf("view lacks %q:\n%s", want, out)
		}
	}
}

func TestCountdownRoundsUpSoItNeverShowsZeroBeforeTheEnd(t *testing.T) {
	c, _ := captureConn(t)
	if out := plain(shownCountdown(t, c, 100).View().Content); !strings.Contains(out, "1s") {
		t.Errorf("100 ms left must read 1s:\n%s", out)
	}
	if out := plain(shownCountdown(t, c, 0).View().Content); !strings.Contains(out, "0s") {
		t.Errorf("0 ms left must read 0s:\n%s", out)
	}
}

func TestCountdownFocusStartsOnTheFirstButtonAndEnterChoosesIt(t *testing.T) {
	c, collect := captureConn(t)
	m := shownCountdown(t, c, 20000)
	if m.focus != 0 {
		t.Fatalf("focus starts on the first (safest) button, got %d", m.focus)
	}
	after, _ := m.Update(tea.KeyPressMsg{Code: tea.KeyEnter})
	if after.(model).view != viewNone {
		t.Error("choosing takes the view down")
	}
	msgs := collect()
	if len(msgs) != 1 || msgs[0].Method != "app.modal.choose" || msgs[0].ID == nil {
		t.Fatalf("want one app.modal.choose request, got %+v", msgs)
	}
	if string(msgs[0].Params) != `{"id":"r1","button":"cancel"}` {
		t.Errorf("params = %s", msgs[0].Params)
	}
}

func TestCountdownArrowsMoveFocusAndEnterChoosesThatButton(t *testing.T) {
	c, collect := captureConn(t)
	m := shownCountdown(t, c, 20000)
	next, _ := m.Update(tea.KeyPressMsg{Code: tea.KeyRight})
	if next.(model).focus != 1 {
		t.Fatalf("right moves focus to the second button, got %d", next.(model).focus)
	}
	stay, _ := next.(model).Update(tea.KeyPressMsg{Code: tea.KeyRight})
	if stay.(model).focus != 1 {
		t.Error("right clamps at the last button")
	}
	back, _ := stay.(model).Update(tea.KeyPressMsg{Code: tea.KeyLeft})
	if back.(model).focus != 0 {
		t.Error("left moves back")
	}
	_, _ = next.(model).Update(tea.KeyPressMsg{Code: tea.KeyEnter})
	msgs := collect()
	if len(msgs) != 1 || string(msgs[0].Params) != `{"id":"r1","button":"now"}` {
		t.Fatalf("enter on the second button chooses it, got %+v", msgs)
	}
}

func TestCountdownEscapeNotifiesCancelledAndHides(t *testing.T) {
	c, collect := captureConn(t)
	after, _ := shownCountdown(t, c, 20000).Update(tea.KeyPressMsg{Code: tea.KeyEscape})
	if after.(model).view != viewNone {
		t.Error("esc takes the view down")
	}
	msgs := collect()
	if len(msgs) != 1 || msgs[0].Method != "ui.cancelled" || msgs[0].ID != nil {
		t.Fatalf("want one ui.cancelled notification, got %+v", msgs)
	}
}

// The countdown appears unprompted at a moment the user did not choose (it is
// the banner's Ctrl-^ review, but it can also be re-summoned as time passes):
// a stray character meant for the session must be neither consumed as an
// answer nor dismiss it.
func TestCountdownIgnoresAnUnboundKey(t *testing.T) {
	c, collect := captureConn(t)
	after, _ := shownCountdown(t, c, 20000).Update(tea.KeyPressMsg{Code: 'x', Text: "x"})
	if after.(model).view != viewCountdown {
		t.Error("an unbound key must leave the countdown up")
	}
	if msgs := collect(); len(msgs) != 0 {
		t.Errorf("an unbound key sends nothing, got %+v", msgs)
	}
}

func TestCountdownUpdateChangesTheTimeAndKeepsFocus(t *testing.T) {
	c, _ := captureConn(t)
	m := shownCountdown(t, c, 20000)
	moved, _ := m.Update(tea.KeyPressMsg{Code: tea.KeyRight})
	updated, _ := moved.(model).Update(updateMsg{RemainingMs: 4200})
	um := updated.(model)
	if um.focus != 1 {
		t.Errorf("an update must keep focus, got %d", um.focus)
	}
	if out := plain(um.View().Content); !strings.Contains(out, "5s") {
		t.Errorf("4.2 s left reads 5s:\n%s", out)
	}
}

func TestCountdownUpdateOutsideACountdownIsIgnored(t *testing.T) {
	c, _ := captureConn(t)
	updated, _ := newModel(c).Update(updateMsg{RemainingMs: 1000})
	if updated.(model).view != viewNone {
		t.Error("an update with no countdown up must not raise a view")
	}
}

func TestCountdownShowAgainResetsFocusToTheFirstButton(t *testing.T) {
	c, _ := captureConn(t)
	moved, _ := shownCountdown(t, c, 20000).Update(tea.KeyPressMsg{Code: tea.KeyRight})
	again, _ := moved.(model).Update(countdownShow(9000))
	if again.(model).focus != 0 {
		t.Error("a fresh ui.show starts from the safest button again")
	}
}

func TestButtonRowMarksOnlyTheFocusedButton(t *testing.T) {
	row := plain(buttonRow([]string{"Cancel restart", "Restart now"}, 1))
	if !strings.Contains(row, "Cancel restart") || !strings.Contains(row, "Restart now") {
		t.Fatalf("both labels present: %q", row)
	}
	if strings.Count(row, "▶") != 1 || strings.Index(row, "▶") < strings.Index(row, "Cancel restart") {
		t.Errorf("the focus marker sits on the second button only: %q", row)
	}
}

func TestProgressBarFillsInProportionToTimeLeft(t *testing.T) {
	if got := progressBar(15000, 30000, 10); strings.Count(got, "█") != 5 || strings.Count(got, "░") != 5 {
		t.Errorf("half left fills half: %q", got)
	}
	if got := progressBar(0, 30000, 10); strings.Contains(got, "█") {
		t.Errorf("none left fills nothing: %q", got)
	}
	if got := progressBar(30000, 30000, 10); strings.Contains(got, "░") {
		t.Errorf("all left fills everything: %q", got)
	}
	if got := progressBar(5000, 0, 10); got != "" {
		t.Errorf("an unknown total draws no bar: %q", got)
	}
}
