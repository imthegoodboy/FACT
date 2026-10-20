# 0003: Alacritty terminal model and visual-only cold recovery

- **Status:** Superseded by [0007](0007-fresh-shells-no-replay.md)
- **Date:** 2026-10-19

## Context

FACT needs a mature VT parser: alternate screen, wide Unicode, truecolor,
scrollback, cursor modes, mouse modes, bracketed paste and the many edge cases
that real applications exercise. Writing a terminal emulator from scratch would
delay the session and daemon work and would be difficult to verify.

After a daemon restart, Windows cannot resurrect a dead process. FACT still needs
to restore the session's visual state and layout. A tempting shortcut is to save
input and replay it; that can execute destructive commands, duplicate work or
hang, and it violates the requirement to never replay commands.

## Decision

Use `alacritty_terminal` as the emulation engine. Add a FACT snapshot format:

1. Capture the visible screen as styled ANSI text.
2. Trim trailing blank rows and do not append an extra newline.
3. Bound the snapshot size and record dimensions plus a SHA-256 checksum.
4. Store it with the pane/session record in SQLite.
5. On recovery, validate the checksum, dimensions, pane layout and ANSI
   constraints, then feed the ANSI into a new terminal model.
6. Start a new shell and mark the session cold-recovered with explicit notes.
7. Never treat saved bytes as input or shell commands.

Snapshots are visual state, not scrollback dumps. The terminal model still has
bounded scrollback during normal operation, and search can query it.

## Consequences

- Advanced terminal behavior comes from a well-tested engine instead of a custom
  parser.
- Cold recovery preserves layout and visible output but not process identity,
  environment changes made inside the old process, unsaved files, or jobs.
- Restored output can scroll into scrollback when a new shell prints its banner.
  Search remains available, and the cold-recovery banner explains what happened.
- Snapshot validation is a security boundary: malformed or oversized data must
  fail closed.
- ANSI snapshots are less complete than a full terminal-state serialization
  (for example, exact scrollback history), but they are simple and auditable.

## Alternatives considered

- **Replay input:** rejected; it can execute commands and duplicate side effects.
- **Resurrect ConPTY handles:** not possible after the owning process exits.
- **Serialize complete alacritty grid state:** larger, version-coupled and still
  does not restore the process.
- **Write a custom VT emulator:** high risk and unnecessary duplication.
