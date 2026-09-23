package main

import (
	"bufio"
	"encoding/json"
	"os"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
)

// plain strips SGR escape sequences so a CONTENT assertion does not depend on
// where lipgloss chooses to place them — it styles per span, so a styled line
// carries escapes inside the text. Handles `ESC [ ... m`, which is all the
// renderer emits.
func plain(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); {
		if s[i] == 0x1b && i+1 < len(s) && s[i+1] == '[' {
			j := i + 2
			for j < len(s) && s[j] != 'm' {
				j++
			}
			i = min(j+1, len(s))
			continue
		}
		b.WriteByte(s[i])
		i++
	}
	return b.String()
}

// captureConn returns a conn whose writes are collected; calling the returned
// func closes the write end and parses every line back into an rpcMessage.
func captureConn(t *testing.T) (*conn, func() []rpcMessage) {
	t.Helper()
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	c := &conn{w: w}
	return c, func() []rpcMessage {
		_ = w.Close()
		var msgs []rpcMessage
		sc := bufio.NewScanner(r)
		for sc.Scan() {
			var m rpcMessage
			if err := json.Unmarshal(sc.Bytes(), &m); err != nil {
				t.Fatalf("non-JSON-RPC line on control channel: %q", sc.Text())
			}
			msgs = append(msgs, m)
		}
		_ = r.Close()
		return msgs
	}
}

// Provenance guard (eng-versioning(7)): posh-palette must report both a version
// and a git sha, formatted `posh-palette <version> (<sha>)`. Under plain
// `go test` the components are the inert dev defaults, but both are non-empty
// and the shape must hold; the nix build flows the real values via -ldflags -X.
func TestVersionLineReportsVersionAndSHA(t *testing.T) {
	line := versionLine()
	rest, ok := strings.CutPrefix(line, "posh-palette ")
	if !ok {
		t.Fatalf("missing %q prefix: %q", "posh-palette ", line)
	}
	open := strings.Index(rest, " (")
	if open < 0 {
		t.Fatalf("missing \" (\": %q", line)
	}
	if !strings.HasSuffix(rest, ")") {
		t.Fatalf("missing closing \")\": %q", line)
	}
	ver := rest[:open]
	sha := rest[open+2 : len(rest)-1]
	if ver == "" {
		t.Errorf("empty version in %q", line)
	}
	if sha == "" {
		t.Errorf("empty git sha in %q", line)
	}
}

// A chosen command issues its action to the client as a JSON-RPC request:
// jsonrpc 2.0, a non-null id, the action's method, and its params verbatim
// (RFC 0005 §4.1).
func TestChooseIssuesActionAsRequest(t *testing.T) {
	c, collect := captureConn(t)
	m := newModel(c)
	m.commands = []command{{
		Name:   "Optimistic echo",
		Action: &action{Method: "echo.set", Params: json.RawMessage(`{"model":"optimistic"}`)},
	}}
	m.recompute()
	m.choose()

	msgs := collect()
	if len(msgs) != 1 {
		t.Fatalf("want 1 message, got %d: %+v", len(msgs), msgs)
	}
	got := msgs[0]
	if got.JSONRPC != "2.0" {
		t.Errorf("jsonrpc = %q, want 2.0", got.JSONRPC)
	}
	if got.ID == nil {
		t.Error("a request must carry an id")
	}
	if got.Method != "echo.set" {
		t.Errorf("method = %q, want echo.set", got.Method)
	}
	if string(got.Params) != `{"model":"optimistic"}` {
		t.Errorf("params = %s, want params passed through verbatim", got.Params)
	}
}

// Selecting with no matches (or a no-op entry) reports a dismissal, not an
// action request — and a notification carries no id (RFC 0005 §4.2).
func TestChooseWithNoMatchesCancels(t *testing.T) {
	c, collect := captureConn(t)
	m := newModel(c)
	m.commands = []command{{Name: "Quit"}}
	m.input.SetValue("zzz")
	m.recompute()
	m.choose()

	msgs := collect()
	if len(msgs) != 1 || msgs[0].Method != "ui.cancelled" {
		t.Fatalf("want a single ui.cancelled, got %+v", msgs)
	}
	if msgs[0].ID != nil {
		t.Error("a notification must not carry an id")
	}
}

// A command with no action is a no-op entry: choosing it dismisses rather than
// issuing an empty request.
func TestChooseNoActionEntryCancels(t *testing.T) {
	c, collect := captureConn(t)
	m := newModel(c)
	m.commands = []command{{Name: "— separator —"}}
	m.recompute()
	m.choose()

	msgs := collect()
	if len(msgs) != 1 || msgs[0].Method != "ui.cancelled" {
		t.Fatalf("want a single ui.cancelled, got %+v", msgs)
	}
}

func TestRecomputeFiltersByName(t *testing.T) {
	m := newModel(&conn{})
	m.commands = []command{{Name: "Quit"}, {Name: "Logging"}, {Name: "Echo"}}
	m.input.SetValue("og")
	m.recompute()
	if len(m.filtered) != 1 || m.filtered[0].Name != "Logging" {
		t.Fatalf("want [Logging], got %+v", m.filtered)
	}
}

// The "dialog" view (RFC 0005 §3.2) switches the model into viewDialog and
// renders the supplied body verbatim, with a copy hint.
func TestShowDialogRendersBody(t *testing.T) {
	body := "agent-fwd: on channels=0\nserver: endpoint=up symlink=ok"
	updated, _ := newModel(&conn{}).Update(showMsg{View: "dialog", Title: "agent forwarding", Body: body})
	dm := updated.(model)
	if dm.view != viewDialog {
		t.Fatalf("view = %d, want viewDialog", dm.view)
	}
	if dm.body != body {
		t.Errorf("body = %q, want %q", dm.body, body)
	}
	out := dm.dialogView()
	for _, want := range []string{"agent-fwd: on channels=0", "server: endpoint=up symlink=ok", "copy"} {
		if !strings.Contains(out, want) {
			t.Errorf("dialogView() missing %q in:\n%s", want, out)
		}
	}
}

// The "picker" view (RFC 0005 §3.5): a row matches the filter when ANY of its
// cells contains the query, and the highlighted row's action is what a
// selection issues — with its params verbatim.
func TestPickerFiltersAnyCellAndChoosesRowAction(t *testing.T) {
	c, collect := captureConn(t)
	updated, _ := newModel(c).Update(showMsg{View: "picker", Rows: []row{
		{Cells: []string{"cargo build", "box", "running"}, Action: &action{Method: "session.switch", Params: json.RawMessage(`{"target":"box:s-1"}`)}},
		{Cells: []string{"vim ~/notes", "dev", "idle"}, Action: &action{Method: "session.switch", Params: json.RawMessage(`{"target":"dev:s-2"}`)}},
		{Cells: []string{"+ create new session…", "local"}, Action: &action{Method: "session.switch", Params: json.RawMessage(`{"target":":+"}`)}},
	}})
	m := updated.(model)
	if m.view != viewPicker {
		t.Fatalf("view = %d, want viewPicker", m.view)
	}
	if m.title != "Sessions" {
		t.Errorf("default title = %q, want Sessions", m.title)
	}
	// "dev" appears only in the second row's HOST cell, not its label.
	m.input.SetValue("dev")
	m.recompute()
	if len(m.filteredRows) != 1 || m.filteredRows[0].Cells[0] != "vim ~/notes" {
		t.Fatalf("want the dev row alone, got %+v", m.filteredRows)
	}
	m.choose()
	msgs := collect()
	if len(msgs) != 1 || msgs[0].Method != "session.switch" {
		t.Fatalf("want a session.switch request, got %+v", msgs)
	}
	if string(msgs[0].Params) != `{"target":"dev:s-2"}` {
		t.Errorf("params = %s, want the row's params verbatim", msgs[0].Params)
	}
}

// Picker rows render as an aligned table: each column padded to its widest
// cell, a short row padded with blanks, and the empty text when nothing
// matches.
func TestPickerViewAlignsColumns(t *testing.T) {
	updated, _ := newModel(&conn{}).Update(showMsg{View: "picker", Rows: []row{
		{Cells: []string{"cargo build", "box", "running"}},
		{Cells: []string{"vi", "dev"}},
	}})
	m := updated.(model)
	out := m.pickerView()
	for _, want := range []string{"cargo build  box  running", "vi           dev"} {
		if !strings.Contains(out, want) {
			t.Errorf("pickerView() missing aligned row %q in:\n%s", want, out)
		}
	}
	m.input.SetValue("zzz")
	m.recompute()
	if !strings.Contains(m.pickerView(), "(no sessions)") {
		t.Errorf("empty picker must show the default empty text:\n%s", m.pickerView())
	}
	if got := truncate("abcdef", 4); got != "abc…" {
		t.Errorf("truncate = %q, want abc…", got)
	}
}

// A `description` (RFC 0005 §3.2) is drawn dim between the heading and the
// filter input of a palette and of a picker — every line of it, in order —
// and is never part of what the filter matches.
func TestDescriptionRendersBetweenTitleAndInput(t *testing.T) {
	for _, view := range []string{"palette", "picker"} {
		updated, _ := newModel(&conn{}).Update(showMsg{
			View: view, Title: "Leaving", Description: "alpha\nbeta",
			Commands: []command{{Name: "Keep"}},
			Rows:     []row{{Cells: []string{"Keep"}}},
		})
		m := updated.(model)
		if m.description != "alpha\nbeta" {
			t.Fatalf("%s: description = %q", view, m.description)
		}
		out := m.paletteView()
		if view == "picker" {
			out = m.pickerView()
		}
		title, a, b, prompt := strings.Index(out, "Leaving"), strings.Index(out, "alpha"), strings.Index(out, "beta"), strings.Index(out, "/ ")
		if title < 0 || a < 0 || b < 0 || prompt < 0 {
			t.Fatalf("%s: missing a part (title=%d alpha=%d beta=%d prompt=%d) in:\n%s", view, title, a, b, prompt, out)
		}
		if !(title < a && a < b && b < prompt) {
			t.Errorf("%s: want title < alpha < beta < prompt, got %d %d %d %d in:\n%s", view, title, a, b, prompt, out)
		}
		// The description is not filterable: a query only it contains matches nothing.
		m.input.SetValue("alpha")
		m.recompute()
		if m.listLen() != 0 {
			t.Errorf("%s: the description must not match the filter (listLen=%d)", view, m.listLen())
		}
		// A later show without a description clears it.
		updated, _ = m.Update(showMsg{View: view, Title: "Leaving"})
		if d := updated.(model).description; d != "" {
			t.Errorf("%s: description not cleared by a show without one: %q", view, d)
		}
	}
}

// posh#216: the empty filter shows the whole placeholder, not just its first
// rune — bubbles' textinput cuts the placeholder to Width()+1 runes, so an
// input left at width 0 rendered "/ T".
func TestEmptyFilterShowsTheWholePlaceholder(t *testing.T) {
	for _, view := range []string{"palette", "picker"} {
		updated, _ := newModel(&conn{}).Update(showMsg{View: view, Title: "Commands"})
		m := updated.(model)
		out := m.paletteView()
		if view == "picker" {
			out = m.pickerView()
		}
		if !strings.Contains(plain(out), "Type to filter") {
			t.Errorf("%s: placeholder cut short in:\n%s", view, plain(out))
		}
	}
}

// Without a description the layout is what it was: the filter input sits on
// the line right after the heading, no block in between.
func TestNoDescriptionKeepsInputUnderTitle(t *testing.T) {
	for _, view := range []string{"palette", "picker"} {
		updated, _ := newModel(&conn{}).Update(showMsg{View: view, Title: "Leaving"})
		m := updated.(model)
		out := m.paletteView()
		if view == "picker" {
			out = m.pickerView()
		}
		lines := strings.Split(out, "\n")
		at := -1
		for i, l := range lines {
			if strings.Contains(l, "Leaving") {
				at = i
				break
			}
		}
		if at < 0 || at+1 >= len(lines) {
			t.Fatalf("%s: no title line in:\n%s", view, out)
		}
		if !strings.Contains(lines[at+1], "/ ") {
			t.Errorf("%s: the input must follow the title directly, got %q in:\n%s", view, lines[at+1], out)
		}
	}
}

// The "notice" view (RFC 0005 §3.6) draws the stack in the order given, with
// `state` alone deciding the treatment: `popped` struck through as removed,
// `current` marked, and anything else — including a state this renderer
// predates — degraded to `below` rather than breaking the view. Several
// `popped` entries in one notice is the cascade: a pop whose target was
// itself gone, reported once rather than as a queue of modals.
func TestNoticeRendersStackInOrderWithStates(t *testing.T) {
	updated, _ := newModel(&conn{}).Update(showMsg{View: "notice", Stack: []entry{
		{Target: "flac:clown-0dfe", State: "popped", Detail: "exited 0"},
		{Target: "flac:s-1", State: "popped", Detail: "gone"},
		{Target: "box:dev", State: "current"},
		{Target: "box:old", State: "from-the-future"},
	}})
	m := updated.(model)
	if m.view != viewNotice {
		t.Fatalf("view = %d, want viewNotice", m.view)
	}
	if m.title != "Session ended" {
		t.Errorf("default title = %q, want Session ended", m.title)
	}
	out := plain(m.noticeView())
	for _, want := range []string{
		"flac:clown-0dfe  (exited 0)", "flac:s-1  (gone)", "box:dev", "box:old",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("noticeView() missing %q in:\n%s", want, out)
		}
	}
	// The order given, never sorted.
	if i, j := strings.Index(out, "flac:clown-0dfe"), strings.Index(out, "box:dev"); i < 0 || j < 0 || i > j {
		t.Errorf("entries must render in the order given:\n%s", out)
	}
	// The marker belongs to `current` and to nothing else.
	if !strings.Contains(out, "→ box:dev") {
		t.Errorf("the current entry must be marked:\n%s", out)
	}
	for _, other := range []string{"→ flac:s-1", "→ box:old"} {
		if strings.Contains(out, other) {
			t.Errorf("only the current entry may carry the marker, found %q in:\n%s", other, out)
		}
	}
	if !strings.Contains(out, "dismiss") {
		t.Errorf("a must-dismiss view needs its hint:\n%s", out)
	}
}

// The state -> treatment mapping itself (RFC 0005 §3.6), pinned directly so
// the assertion does not depend on where lipgloss places escape sequences:
// `popped` is drawn as removed, `current` takes the marker, and an
// unrecognized state is indistinguishable from `below`.
func TestNoticeEntryStyleMapsStateToTreatment(t *testing.T) {
	if _, marker := entryStyle("current"); marker != "→ " {
		t.Errorf("current marker = %q, want the arrow", marker)
	}
	gone, marker := entryStyle("popped")
	if !gone.GetStrikethrough() {
		t.Error("a popped entry must be drawn as visibly removed")
	}
	if marker != "  " {
		t.Errorf("popped marker = %q, want blank — only current is marked", marker)
	}
	future, fm := entryStyle("from-the-future")
	below, bm := entryStyle("below")
	if future.GetStrikethrough() != below.GetStrikethrough() || fm != bm {
		t.Error("an unrecognized state must degrade to `below`, never error")
	}
}

// §3.6: an unbound keystroke must be IGNORED, not swallowed. The notice
// appears unprompted, at a moment the user did not choose, so a character
// meant for the shell underneath must neither dismiss it nor be consumed.
func TestNoticeIgnoresAnUnboundKey(t *testing.T) {
	c, collect := captureConn(t)
	shown, _ := newModel(c).Update(showMsg{View: "notice", Stack: []entry{
		{Target: "box:dev", State: "current"},
	}})
	after, _ := shown.(model).Update(tea.KeyPressMsg{Code: 'x', Text: "x"})
	if after.(model).view != viewNotice {
		t.Error("an unbound key must leave the notice up")
	}
	if msgs := collect(); len(msgs) != 0 {
		t.Errorf("an unbound key must send nothing, got %+v", msgs)
	}
}

// Dismissing reports ui.cancelled (RFC 0005 §4.2) and takes the view down.
// `q` is one of the renderer's existing dismiss keys, shared with the dialog.
func TestNoticeDismissNotifiesCancelled(t *testing.T) {
	c, collect := captureConn(t)
	shown, _ := newModel(c).Update(showMsg{View: "notice", Stack: []entry{
		{Target: "box:dev", State: "current"},
	}})
	after, _ := shown.(model).Update(tea.KeyPressMsg{Code: 'q', Text: "q"})
	if after.(model).view != viewNone {
		t.Error("a bound dismiss key must take the notice down")
	}
	msgs := collect()
	if len(msgs) != 1 || msgs[0].Method != "ui.cancelled" {
		t.Fatalf("want a single ui.cancelled, got %+v", msgs)
	}
	if msgs[0].ID != nil {
		t.Error("a notification must not carry an id")
	}
}

// ui.show accepts exactly the four RFC 0005 views.
func TestKnownViews(t *testing.T) {
	for _, v := range []string{"palette", "dialog", "picker", "notice"} {
		if !knownView(v) {
			t.Errorf("%q must be a known view", v)
		}
	}
	if knownView("tabs") {
		t.Error("an unknown view must be rejected (-32602)")
	}
}

// Pressing copy in a dialog notifies the client (which owns the real terminal
// and emits the OSC 52); the notification carries no id (RFC 0005 §4.3).
func TestDialogCopyNotifies(t *testing.T) {
	c, collect := captureConn(t)
	m := newModel(c)
	m.view = viewDialog
	m.sendCopy()

	msgs := collect()
	if len(msgs) != 1 || msgs[0].Method != "ui.copy" {
		t.Fatalf("want a single ui.copy, got %+v", msgs)
	}
	if msgs[0].ID != nil {
		t.Error("a notification must not carry an id")
	}
}
