---
status: experimental
date: 2026-06-22
---

# Palette Control Protocol

## Abstract

This document specifies the control protocol between a posh **client** (the
Rust backend that owns the user's terminal and a session) and the **palette
renderer** (`posh-palette`, a long-running Go/charmbracelet subprocess that
draws the command palette overlay). The two communicate over a private control
channel — newline-delimited [JSON-RPC 2.0] messages — while the renderer paints
its UI to a PTY whose screen the client composites onto the session view. The
protocol is deliberately **transport-agnostic and generic**: the renderer knows
nothing about posh's session, transport, or the meaning of any command; it is a
reusable palette that the client configures at runtime and whose selections it
dispatches. This decoupling is the point — the frontend (UI) and backend
(behavior) evolve independently, and the same renderer serves any client core
(see posh#87 on converging the roaming and attach clients).

## Introduction

posh's clients expose runtime controls (predictive-echo model, debug logging,
quit, …) that today are reachable only as single-key escape chords
(`Ctrl-^ e`, `Ctrl-^ d`) with no discoverability. A command palette — a
chord-summoned, filterable list of named commands — makes the full control
surface visible and extensible without growing the chord table.

Rather than build the palette UI in Rust, posh hosts a charmbracelet
(bubbletea) renderer as a subprocess and drives it over a control channel. This
buys a mature TUI toolkit and, more importantly, forces a clean **frontend ↔
backend boundary**: the renderer is a generic palette; the client is the
authority on what commands exist and what they do. The protocol below is that
boundary.

The renderer draws to a PTY allocated by the client. The client reads that
PTY's screen with its existing `posh_term::Terminal`, composites the non-blank
region onto the session `Snapshot`, and paints the result — exactly as it
already composites predicted echo and the status banner. Thus the *rendering*
path needs no new wire format; only the *control* path (what to show, what was
chosen) does, and that is this protocol.

Scope: this document specifies the control-channel framing, the JSON-RPC method
surface in both directions, the command/action model, and the lifecycle
(handshake through shutdown). It does NOT specify how the client allocates the
renderer's PTY/control socket, how it composites the rendered screen, or which
chord summons the palette — those are client implementation details. It does
not touch posh's session IPC or roaming transport.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Roles and channel

There are two peers:

- The **client** — the posh process that owns the terminal and session (the
  Rust backend). It spawns the renderer, configures the palette, and dispatches
  selected commands.
- The **renderer** — the `posh-palette` subprocess (the Go frontend). It draws
  the palette and reports selections.

The two exchange messages over a single bidirectional **control channel**,
distinct from the renderer's PTY (which carries only the rendered screen). The
client provides the channel to the renderer as **file descriptor 3**; the
renderer reads requests from fd 3 and writes its messages to fd 3. The means by
which the client creates fd 3 (e.g. a `socketpair`) is out of scope.

The renderer MUST NOT write protocol messages to its stdout (that is the PTY
screen) or stderr (reserved for human-readable diagnostics). All protocol
traffic is on fd 3.

### 2. Framing

The channel carries **newline-delimited JSON** (NDJSON): each message is a
single [JSON-RPC 2.0] object encoded as UTF-8 JSON on one line, terminated by a
single `\n` (0x0A). A message MUST NOT contain an unescaped newline. Peers MUST
ignore a blank line and MUST attempt to parse each non-blank line independently;
a line that is not valid JSON-RPC is handled per §6.

Every message MUST carry `"jsonrpc":"2.0"`. The three message kinds are the
JSON-RPC standard:

- **Request** — has `id` (a non-null integer or string), `method`, optional
  `params`. Expects exactly one matching Response.
- **Response** — has the request's `id` and exactly one of `result` or `error`.
- **Notification** — has `method`, optional `params`, and no `id`. Expects no
  reply.

Each peer maintains its own `id` space for the requests it originates; ids are
matched only within a direction. Batch arrays are NOT supported (a renderer's UI
is inherently sequential); a peer receiving a top-level JSON array MUST respond
with an `invalid request` error (§6) if it can recover an id, else ignore it.

### 3. Methods the renderer implements (client → renderer)

#### 3.1 `initialize` (request)

The capability handshake. The client SHOULD send `initialize` as its first
message; the renderer MUST respond before processing any later request is
required to take effect (the renderer MAY queue later requests until it has
responded).

`params`:

| field      | type    | required | meaning                              |
|------------|---------|----------|--------------------------------------|
| `protocol` | integer | yes      | Highest protocol version the client speaks. This document is version **1**. |

`result`:

| field      | type    | meaning                                          |
|------------|---------|--------------------------------------------------|
| `name`     | string  | Renderer identity, e.g. `"posh-palette"`.        |
| `version`  | string  | Renderer build version (`POSH_VERSION` + rev).   |
| `protocol` | integer | Protocol version the renderer will speak (≤ the client's). |

If the renderer cannot satisfy the client's protocol (no common version), it
MUST return error code `-32000` (§6) and MAY exit.

#### 3.2 `ui.show` (request)

Show a view. `result` is an empty object `{}` acknowledging the view is up
(the client MAY begin compositing on receipt).

`params`:

| field      | type      | required | meaning                                  |
|------------|-----------|----------|------------------------------------------|
| `view`     | string    | yes      | View to display. Version 1 defines `"palette"`. |
| `commands` | command[] | for `palette` | The command list (§5).             |
| `title`    | string    | no       | Heading; default `"Commands"`.           |
| `prompt`   | string    | no       | Filter-input prompt; default `"/ "`.     |

An unknown `view` MUST yield error `-32602` (invalid params). A second `ui.show`
while a view is up REPLACES it (re-configures in place).

#### 3.3 `ui.hide` (request)

Dismiss the current view without reporting a selection. `result` is `{}`.
Idempotent: hiding when nothing is shown succeeds with `{}`.

#### 3.4 `ui.shutdown` (notification)

Tell the renderer to exit. The renderer MUST stop drawing, restore nothing
(the client owns the screen), and terminate its process promptly. Because it is
a notification there is no response; the client SHOULD treat control-channel
EOF or process exit as completion, and SHOULD enforce a timeout-bounded
`SIGKILL` backstop (the renderer's event loop may be wedged).

### 4. Methods the client implements (renderer → client)

#### 4.1 Command actions (request)

When the user selects a command, the renderer issues that command's **action**
(§5) as a JSON-RPC request to the client: it takes the action's `{method,
params}` verbatim, adds `"jsonrpc":"2.0"` and a fresh renderer-side `id`, and
writes it to fd 3. The renderer treats the method and params as opaque — it does
not know what `echo.set` means.

The client dispatches the method (§7) and MUST respond: `result` (an object,
possibly `{}`) on success, or an `error` (§6) on failure. The renderer SHOULD
close the palette on selection regardless of the response, and MAY surface an
`error` to the user.

A command with no `action` (a no-op/separator) MUST NOT produce a request.

#### 4.2 `ui.cancelled` (notification)

The renderer sends `ui.cancelled` when the user dismisses a view without
selecting (e.g. `Esc`). It carries no `params`. The client SHOULD treat this as
"palette closed, do nothing".

### 5. Command and action model

A **command** offered in `ui.show` `params.commands`:

| field    | type   | required | meaning                                            |
|----------|--------|----------|----------------------------------------------------|
| `name`   | string | yes      | Display label; also the fuzzy-filter key.          |
| `action` | action | no       | What to issue on selection (§4.1); omit for a no-op.|

An **action** is a partial JSON-RPC request the renderer completes and issues:

| field    | type   | required | meaning                                            |
|----------|--------|----------|----------------------------------------------------|
| `method` | string | yes      | The client method to invoke (§7).                  |
| `params` | object | no       | Method arguments, passed through verbatim.         |

Commands are opaque to the renderer beyond `name` (for display/filtering) and
`action` (for dispatch). This is what makes the renderer generic: the client's
command vocabulary (§7) can grow with zero renderer changes.

### 6. Errors

Error objects use the JSON-RPC 2.0 shape `{"code","message","data"?}`. Standard
codes apply: `-32700` parse error, `-32600` invalid request, `-32601` method not
found, `-32602` invalid params, `-32603` internal error. Application codes use
the reserved server range `-32000..-32099`; this document assigns:

| code     | meaning                                          |
|----------|--------------------------------------------------|
| `-32000` | Protocol-version mismatch (handshake, §3.1).     |
| `-32001` | Command dispatch failed (the action ran but errored). |

A peer that receives an unparseable line SHOULD emit a `-32700` parse-error
Response if and only if it can recover the `id`; otherwise it MUST ignore the
line (a Notification cannot be answered). A received Response with an unknown
`id` MUST be ignored.

### 7. Initial client method registry (version 1)

The client-implemented methods (§4.1 targets) defined by version 1. The set is
extensible; a client MAY implement a superset and MUST return `-32601` for
methods it does not implement.

| method           | params                                  | effect                                       |
|------------------|-----------------------------------------|----------------------------------------------|
| `echo.set`       | `{"model": <string>}`                   | Set predictive-echo model. `model` ∈ `adaptive`, `optimistic`, `always`, `never`. Invalid → `-32602`. On a client without prediction (local attach), `-32601`. |
| `logging.set`    | `{"enabled": <bool>}`                   | Enable/disable client debug logging. `result` MAY carry `{"path": <string>}`. |
| `shell.open`     | `{}`                                    | Open the server-side escape-to-shell overlay (FDR 0008) in the session cwd. On a client without a server-side overlay, `-32601`. |
| `client.suspend` | `{}`                                    | Suspend the client process (job-control `SIGSTOP`); the remote session keeps running. |
| `app.quit`       | `{}`                                    | Quit the client (close the connection / detach per client semantics). |

Per posh#87, these methods are the client's shared control surface: a converged
client core implements them once and the same palette drives every transport.
`echo.set` is the one capability that is transport-conditional (prediction
exists only over a network), which the registry models as a `-32601` on clients
that lack it rather than as a separate protocol.

### 8. Lifecycle

1. Client spawns the renderer with fd 3 wired to the control channel and a PTY
   for its screen.
2. Client → `initialize`; renderer → result (handshake, §3.1).
3. On the summon chord, client → `ui.show {view:"palette", commands:[…]}`;
   renderer → `{}`, draws, client composites its screen.
4. User filters/navigates (renderer-local; no protocol traffic) and either:
   - selects → renderer → action request (§4.1); client dispatches, responds;
     both sides close the view; or
   - cancels → renderer → `ui.cancelled` (§4.2); client closes the view.
5. Steps 3–4 repeat for the renderer's lifetime (it stays resident between
   summons, showing an empty view when hidden).
6. On client exit: client → `ui.shutdown`; renderer terminates; client enforces
   a `SIGKILL` backstop on timeout (§3.4).

### 9. Versioning and extension

The protocol version is an integer carried in `initialize` (§3.1); this is
version **1**. Backward-compatible growth (new optional `params` fields, new
`view` kinds, new §7 methods) does NOT bump the version — a peer MUST ignore
unknown object fields and answer unknown methods/views with the appropriate
error. A breaking change bumps the version and is negotiated by the handshake.

[JSON-RPC 2.0]: https://www.jsonrpc.org/specification
