---
status: exploring
date: 2026-06-10
decision-makers: sfriedenberg
---

# Bridge macOS/Linux libc divergence in posh with cfg-gating and width casts

## Context and Problem Statement

The `posh` crate makes raw `libc` syscalls for PTY allocation (`crates/posh/src/pty.rs`) and dual-stack UDP socket binding (`crates/posh/src/remote/datagram.rs`). The Linux/glibc and macOS/BSD libc ABIs disagree at three call sites, so code that builds on Linux fails to compile for `aarch64-apple-darwin`:

1. `SOCK_CLOEXEC` — exists on Linux as a `socket()` type flag; absent on macOS/BSD.
2. `openpty()` — takes `*const termios` / `*const winsize` on Linux but `*mut` on macOS/BSD.
3. `TIOCSCTTY` (and other `ioctl` request constants) — `c_int`-width (`u32`) on Linux, `c_ulong`-width (`u64`) on macOS, while `ioctl`'s request parameter is `c_ulong` on macOS.

We need the workspace to compile and pass tests on both targets without forking the syscall logic into per-platform modules.

## Decision Drivers

* Smallest diff that unblocks the Darwin build — the logic is correct; only the ABI surface differs.
* Keep the unsafe syscall sequences readable and in one place, not split across `#[cfg]` module copies.
* Preserve the close-on-exec guarantee on both platforms (a leaked fd across `exec` is a real correctness concern for a remote-shell tool).
* Avoid weakening the Linux path's atomicity without a deliberate reason.

## Considered Options

* **Option 1 — Minimal cfg + casts.** Keep one code path. Cast the `openpty` pointers (`as *mut _`) and the `TIOCSCTTY` constant (`as _`) so each platform's signature resolves the type. `cfg(target_os = "linux")` keeps the atomic `SOCK_CLOEXEC` flag; the non-Linux branch creates the socket plain and sets `FD_CLOEXEC` via a follow-up `fcntl`.
* **Option 2 — fcntl on both.** Drop `SOCK_CLOEXEC` entirely; always set `FD_CLOEXEC` via `fcntl` after `socket()` on every platform. Plus the same cast fixes for `openpty`/`TIOCSCTTY`. One uniform path, no `cfg` on the socket branch.

## Decision Outcome

Chosen option: **Option 1 (Minimal cfg + casts)**, because it is the smallest change that unblocks the Darwin build while preserving Linux's atomic close-on-exec, accepting that the close-on-exec idiom now differs by platform and that the two-syscall non-Linux branch has a (currently irrelevant) atomicity gap.

This decision is **exploring**, not settled: the only thing separating Option 1 from Option 2 is whether atomic `SOCK_CLOEXEC` on Linux is worth a `cfg` split. posh does not currently `fork()` between the `socket()` and the `fcntl()` in `bind_udp_v6`, so the non-atomic window the Linux flag protects against is not reachable today. If a future change introduces a fork on that path — or if the `cfg` split proves to be more maintenance burden than the atomicity is worth — collapsing to Option 2's uniform path is the expected move. Revisit before adding any threading or forking near socket creation.

### Consequences

* Good, because the Darwin build compiles and `cargo check --workspace` passes on `aarch64-apple-darwin` with no new warnings.
* Good, because the Linux path keeps atomic close-on-exec via `SOCK_CLOEXEC`.
* Good, because the syscall logic stays in a single readable function per concern — no per-platform module duplication.
* Bad, because close-on-exec is now expressed two different ways depending on platform, which a reader must reconcile.
* Bad, because the non-Linux `socket()` + `fcntl()` sequence is non-atomic; safe only as long as nothing forks between the two calls.
* Neutral, because `as _` width casts silently absorb future constant-width changes — convenient, but they also hide a real ABI difference behind a cast.

### Confirmation

`just build-rust` (hermetic `cargo test --workspace`) must pass on both Linux and macOS CI lanes. The original failure was a `nix build` / `just build-rust` break on `aarch64-apple-darwin`; that lane going green is the confirmation.

## Pros and Cons of the Options

### Option 1 — Minimal cfg + casts

* Good, because it is the smallest diff and touches only the three failing sites.
* Good, because Linux retains atomic close-on-exec.
* Neutral, because it introduces one `cfg(target_os)` split on the socket branch.
* Bad, because the close-on-exec idiom is no longer uniform across platforms.

### Option 2 — fcntl on both

* Good, because the close-on-exec path is identical on every platform — one idiom to understand.
* Good, because no `cfg` on socket creation.
* Neutral, because the cast fixes for `openpty`/`TIOCSCTTY` are still required either way.
* Bad, because it gives up Linux's atomic `SOCK_CLOEXEC` for a uniformity that only matters if a fork is ever introduced on that path.

## More Information

* Failing build: `just build-rust` on `aarch64-apple-darwin` (passed on Linux), errors `E0425` (`SOCK_CLOEXEC`), `E0308` (`openpty` pointer mutability, `TIOCSCTTY` width).
* Implementation: `crates/posh/src/remote/datagram.rs` (`bind_udp_v6`), `crates/posh/src/pty.rs` (`spawn_shell`). Both sites carry a comment pointing back to this ADR.
