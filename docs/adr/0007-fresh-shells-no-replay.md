# 0007: Saved layouts reopen with fresh shells

- **Status:** Accepted
- **Date:** 2026-10-21
- **Supersedes:** [0003](0003-visual-cold-recovery.md)

## Context

v0.2 stored a bounded ANSI snapshot of each pane and replayed it after a daemon
restart. The modal and restored screen made a cold start look like a live
session, even though the old processes and their environment were gone. Users
asked for a simpler restart: keep the layout, start the shells, and show nothing
from the previous run.

A dead Windows process cannot be resurrected, so any restart necessarily starts
new shells. The remaining question is whether to persist and replay terminal
output at all.

## Decision

Persist layout and pane metadata, not terminal output.

1. SQLite stores session identity, tabs, splits, profiles, working directories
   and last known PIDs. It does not store screen contents or input history.
2. A daemon restart remaps pane IDs, creates a fresh terminal model and starts a
   fresh shell in every saved pane.
3. The client attaches through the normal path. There is no recovery overlay,
   recovery timestamp or recovery-specific protocol field.
4. The v1-to-v2 database migration clears the legacy snapshot columns before any
   session is loaded. Legacy exports may still contain snapshot fields; they are
   ignored on import.
5. Live detach and reattach are unchanged: they keep the original processes,
   terminal state, scrollback and PIDs.
6. The wire protocol is version 5 because the recovery metadata fields were
   removed.

## Consequences

- A restart is predictable: every pane shows a fresh shell at its saved working
  directory with the saved profile.
- Old output cannot be mistaken for live output and no saved bytes can be
  replayed as input.
- Restart loses previous scrollback and on-screen context. This is intentional;
  use live detach/reattach when that context matters.
- Exports are smaller and no longer contain terminal output.
- v0.2 databases are still readable. The migration overwrites and clears snapshot
  data, vacuums the database and truncates the WAL. The legacy columns remain in
  the schema but are empty.
- A client and daemon must both speak protocol 5; versions are compared exactly,
  so there is no forward or backward negotiation.

## Alternatives considered

- **Keep snapshots but hide the modal:** rejected; replay still creates a fake
  screen and keeps sensitive output on disk.
- **Store output without replaying it:** rejected; it adds storage and privacy
  cost with no user-visible benefit.
- **Start a fresh default session on every launch:** rejected; saved tabs, splits
  and working directories are still useful and were explicitly kept.
- **Replay input:** rejected; it can execute destructive commands and duplicate
  side effects.
