# 0005: SQLite persistence with versioned JSON export

- **Status:** Amended by [0007](0007-fresh-shells-no-replay.md)
- **Date:** 2026-10-19

## Context

FACT must persist sessions, layouts and pane metadata across daemon restarts,
provide offline session commands, and support user-controlled export and import. A single JSON file would be easy to inspect but hard to update
atomically, query, or migrate. A custom binary store would be opaque and
difficult to recover.

Exports also need a stable, documented shape so users can move work between
machines or archive it without copying the database.

## Decision

Use bundled SQLite through `rusqlite`:

- schema migrations recorded in the database
- transactional saves of session, layout and panes
- live state and terminal output kept by the daemon; SQLite is the durable
  record for layout and metadata only
- offline CLI operations open the database directly when no daemon is running

Exports are versioned `fact-session` JSON. Import:

- rejects unknown versions and malformed structures
- validates names, layouts and pane records
- always creates a fresh session identity and fresh pane IDs
- resets PIDs and exit codes
- requires an explicit overwrite for a name collision

## Consequences

- Offline `sessions`, `rename`, `export`, `import`, archive and delete work
  without the daemon.
- Live commands such as attach, detach, save and terminate still require IPC.
- SQLite's bundled build avoids depending on a system SQLite installation.
- Migrations must be forward-only and tested.
- The database is not encrypted; the user profile ACL and volume encryption are
  the data-at-rest controls.
- Export files contain layout and metadata, not terminal output.

## Alternatives considered

- **One JSON file:** poor concurrency, torn writes and expensive updates.
- **Registry:** not appropriate for structured session metadata and hard to back
  up or inspect.
- **Opaque binary export:** no user inspection or cross-version tooling.
