# 0008: One process owns the UI, engine and shells; no persistence

- **Status:** Accepted
- **Date:** 2026-10-21

## Context

Earlier FACT versions ran a per-user daemon that owned ConPTY shells and
survived UI detach. A client connected over an authenticated named pipe, and
sessions, layouts and exports were stored in SQLite and JSON on disk. That design
bought live detach/reattach and restart recovery, but also required a daemon
lifecycle, a wire protocol, token authentication, a database and migration path,
stale-identity handling, recovery UI and a large test harness.

The product requirement is now narrower: FACT is an ordinary terminal window.
Tabs, splits and grids are workspace features inside that window. Users expect
closing the window to close its shells, the same way closing Windows Terminal
closes its panes. There is no requirement to survive UI exit, attach a second
client, resume a named session or preserve scrollback across restarts.

## Decision

Run the terminal UI, the engine owner thread and every ConPTY shell in one
`fact.exe` process.

- The engine owns the panes, layout, terminal models and frame generation on one
  owner thread.
- The UI owns rendering, keyboard/mouse handling, selection and clipboard, and
  communicates with the engine over in-process channels.
- There is exactly one workspace, named `FACT`. Every launch starts a fresh grid
  and tabs; nothing is restored.
- Closing FACT, closing the last tab, choosing quit or ending the process kills
  every shell it owns. Confirmations apply to closing a pane or tab, not to
  quitting FACT.
- There is no daemon, service, named pipe, token, session database, saved
  layout, export file, log or runtime identity.
- An optional `config.toml` is the only input file. `fact config init` is the
  only command that writes one.
- `FACT_PANE` remains an informational environment value. Nested launches are
  not rejected.
- Configuration schema version 2 removes daemon and persistence settings;
  obsolete keys are rejected rather than ignored.

## Consequences

- The process and security model is simple: no listener, no authentication
  protocol, no shared endpoint and no stored terminal output.
- Startup is independent of previous runs, so there is no stale state, migration
  or recovery path.
- Shells and unsaved work are lost when FACT exits or crashes. This is the
  intended console-window behavior, not a recoverable failure.
- Two FACT windows are two independent workspaces with no shared state.
- Tests no longer need an isolated data directory, daemon identity or transport;
  they start the engine directly or run the CLI in a temporary `FACT_HOME`.
- `fact-daemon`, `fact-ipc`, `fact-runtime`, `fact-session` and `fact-testkit`
  are deleted. The remaining crates keep their boundaries, so a future embedder
  could reuse the engine without the TUI.

## Alternatives considered

- **Keep the daemon as an optional mode:** preserves detach and reattach, but
  keeps two process models, a transport, persistence and stale-state handling
  alive for a feature the product no longer needs.
- **Windows service:** requires service configuration or elevation, does not
  match a per-user console window and adds lifecycle complexity without user
  value.
- **In-process engine with an optional database:** would preserve layouts while
  still adding migrations, validation and recovery behavior. Launching with the
  same options already reproduces a workspace without storing anything.
- **Keep session export/import as a file format:** useful for sharing a layout,
  but it would reintroduce schema, validation and identity concerns for a
  feature that a configuration profile can cover.
