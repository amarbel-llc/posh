package main

import (
	"bufio"
	"encoding/json"
	"os"
	"strings"
	"testing"
)

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
