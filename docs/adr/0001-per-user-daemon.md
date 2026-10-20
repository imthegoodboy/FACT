# 0001: Per-user daemon owns all shells

- **Status:** Accepted
- **Date:** 2026-10-19

## Context

A terminal multiplexer must keep shells alive when the display disconnects. The
two obvious designs are:

1. keep PTYs in the UI process and detach by transferring handles, or
2. put PTYs in a long-lived background process and make the UI a client.

FACT targets native Windows ConPTY. ConPTY handles are process-local kernel
objects, and a UI crash, console close or terminal-window close must not kill
running shells. FACT also needs one authoritative source for layout, terminal
scrollback and persistence so multiple clients cannot disagree.

A Windows service was considered and rejected: it would require service
installation/elevation or service ACL configuration, and it does not match the
per-user data and security boundary.

## Decision

Run a per-user daemon as the same `fact.exe` binary with a hidden `daemon`
subcommand. The first CLI that needs it spawns it detached from the console. The
daemon owns:

- ConPTY processes and job objects
- terminal emulation and scrollback
- the layout tree and session metadata
- the SQLite store and saved layouts
- IPC authentication and frame fan-out

The client owns only presentation and input encoding. There is exactly one
daemon per data directory, discovered through a stable pipe name and an
ACL-protected identity file.

## Consequences

- `fact detach`, closing the UI and losing the client do not terminate shells.
- A daemon crash loses live processes, but the saved layout reopens with fresh
  shells in every pane.
- The daemon is a single point of failure for all sessions of that user.
- Protocol versioning and client/daemon compatibility become explicit concerns.
- No elevation, service registration or machine-wide state is required.
- A user can run isolated instances with `FACT_HOME` or `--data-dir`; each gets
  its own pipe and database.

## Alternatives considered

- **In-process ConPTY and handle transfer:** fragile on Windows, complicated
  console ownership and did not survive all client exits.
- **Windows service:** stronger isolation but requires elevation/service setup
  and is a poor fit for per-user credentials and sessions.
- **Named shared memory plus a coordinator:** more moving parts than a single
  owner thread and SQLite.
