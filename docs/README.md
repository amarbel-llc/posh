# posh design records

Four kinds of record, each a directory:

| directory    | kind | what it records                                          |
|--------------|------|----------------------------------------------------------|
| `decisions/` | ADR  | an architecture decision: the options weighed and why one won |
| `rfcs/`      | RFC  | a wire or file-format contract another implementation could be written against |
| `features/`  | FDR  | a user-facing feature: what it does, its levers, its rollback |
| `plans/`     | —    | dated working documents for one piece of work; not durable records |

Each record carries YAML frontmatter with a `status` and a `date`.

## Status vocabulary

**`status` describes the CODE, not the author's confidence in the design.**
It answers one question for a reader who has not read the source: *can I rely
on this today?*

| status         | meaning                                                                 |
|----------------|-------------------------------------------------------------------------|
| `exploring`    | research. No implementation is expected, and the design may not converge. |
| `proposed`     | designed, **not implemented**. Reading the code will not find it.        |
| `experimental` | implemented and in use, but the contract may still change — a lever may flip, a field may move, a default may be revisited. |
| `stable`       | implemented and settled. Changing it is a breaking change with a migration story. |
| `accepted`     | for ADRs: the decision stands. An ADR does not become `stable`; it is a decision, not a surface. |
| `superseded`   | replaced. The record MUST name its successor.                            |

## The rule

**Move the status in the same commit that moves the code.** A record that
says `proposed` while its feature ships is worse than no status at all: it
makes the index unreadable, because a reader cannot tell a genuine proposal
from a shipped contract without reading the implementation — which is the
work the record exists to save.

The transition that matters most is `proposed` → `experimental`, and it
belongs to the commit that lands the first working version. The later
promotion `experimental` → `stable` is a deliberate act, usually its own
issue (posh#45 is the pattern: *"[fdr] promote FDR 0002 from experimental"*),
because it is a commitment not to break the surface — not merely an
observation that the code exists.

## History

Audited 2026-09-22. Eleven records were marked `proposed` while their
implementations were shipped and load-bearing — among them RFC 0011 (both mux
increments default-on), RFC 0013 and RFC 0014 (which `posh mux ls` and
`posh status` are built on), and FDR 0015/0016 (`ph` and the session
switcher). They were moved to `experimental`.

The drift had a concrete cost: it hid the one record that genuinely *was*
unimplemented. ADR 0006 and RFC 0012 (session geometry on the frame) decided,
in July 2026, to stop making every serializer independently uphold an
invariant that had already been violated twice in production — and sat
indistinguishable from a dozen shipped records that shared their status.
