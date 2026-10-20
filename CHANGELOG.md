# Changelog

All notable changes to FACT are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and semantic versioning.

## [0.3.0] - 2026-10-21

### Changed

- After a daemon restart, FACT now reopens the saved session name, tabs, splits,
  profiles and working directories with a fresh shell in every pane. It no longer
  restores or replays old screen output. Live detach and reattach still keep the
  original processes, PIDs, terminal state and scrollback.
- The v1 database migration clears legacy snapshot columns before loading a
  session. Existing saved sessions keep their layout and start fresh shells.
- The wire protocol is now version 5. The installer stops the previous daemon,
  and a stale daemon identity is cleared as before.
- `snapshot_interval_secs` and `snapshot_max_bytes` remain accepted in existing
  configuration files but are ignored. New config templates omit them.
- Session exports no longer contain terminal output; v1 exports still import, and
  their snapshot fields are ignored.

### Removed

- The cold-recovery modal, recovery notes, recovery timestamps, pane restore
  markers and all new ANSI snapshot capture, validation, storage and replay.
  The v1-to-v2 migration zeroes legacy snapshot and cold-recovery values,
  vacuums the database and truncates the WAL. The legacy columns remain in the
  schema but no longer contain screen output.
- The client no longer has a `Cold` overlay, and the protocol no longer has
  `cold_recovered`, `recovery_notes`, `cold_recovered_at_ms` or `cold_restored`
  fields.

### Added

- `Alt+T` directly terminates the active grid (tab) and all of its panes after
  the usual close confirmation. The right-click menu also lists the close-grid
  action.
- Prefix then `[` enters a scroll mode for the focused pane: Up/Down by a line,
  PgUp/PgDn by a page, Home/End for top/bottom, and Esc to exit. The mode shows
  the current offset and never forwards its keys to the application.
- Shift+wheel bypasses application mouse capture and scrolls FACT's terminal
  history, which makes agent chats and other full-screen applications readable.

### Fixed

- A fresh terminal model is always created on restart, so old output cannot
  appear above a new shell or be mistaken for live output. No saved bytes are
  ever submitted as input.

## [0.2.3] - 2026-10-21

### Fixed

- Upgrading over a stale or incompatible `daemon.json` no longer blocks the first
  launch with a protocol-mismatch error. If no process still owns the old pipe,
  `fact` removes the dead identity and starts the current daemon; if the old
  daemon is still alive, it reports a clear stop-and-retry error. The installer
  also clears a dead daemon identity when replacing an existing install.

## [0.2.2] - 2026-10-21

### Fixed

- A narrow window no longer traps the TUI in focused-pane mode. `Alt+N` (or
  prefix then `N`) shows the full grid even below `min_pane_width`/
  `min_pane_height`, and `Alt+M` (or prefix then `M`) maximizes the focused pane.
  Restoring a zoomed pane also restores the full grid instead of falling back to
  focused-pane mode.
- Running `fact` or `fact attach` inside a FACT pane now fails with a clear
  message instead of nesting a second UI in the same terminal and rendering a
  black screen. Every pane receives `FACT_SESSION`/`FACT_PANE`; set
  `FACT_ALLOW_NESTED=1` only for deliberate nested testing.
- The minimized-pane restore binding moved to prefix then `Shift+U`, next-tab to
  prefix then `Shift+N`, and minimize to prefix then `Shift+M`, leaving `M`/`N`
  for maximize/show-all.

### Changed

- The right-click menu and command palette include Maximize focused pane and
  Show all panes.
- The wire protocol is now version 4 because restoring a zoomed pane has
  full-grid semantics. The installer stops the previous daemon; saved sessions
  cold-recover on the next launch.

## [0.2.1] - 2026-10-20

### Fixed

- `fact --grid N` no longer attaches to whichever live or saved session was most
  recent and silently ignores the requested grid. It now starts or reattaches a
  dedicated `grid-N` session, allocates `grid-N-2`, `grid-N-3`, ... when that
  name is taken by a different shape, and rejects an explicit `--session NAME
  --grid N` mismatch with a clear error instead of modifying or attaching to it.
- An eight-pane grid stays a grid in an 80x24 window. The default
  `min_pane_width` is now 16, and pane title rows no longer consume terminal
  height.

### Changed

- The TUI is chrome-free for a single-tab session: per-pane headers and their
  `V H _ ^ X` buttons are gone, along with the permanent status bar and the
  single-tab tab strip. Terminal content uses the full window height.
- A one-line tab strip appears only when a session has two or more tabs. Status
  messages are brief bottom-right overlays, the prefix is a two-cell accent
  marker while armed, and a `▾ N` restore tray appears only when panes are
  minimized.
- Split, zoom, minimize, close, new tab and copy remain available through the
  keyboard and the pane right-click menu. A bell is now a small amber dot at the
  pane's top-right instead of status text.
- Panes reserve their rightmost column for the scrollbar and use every remaining
  row for terminal content.
- The wire protocol is now version 3. The installer stops the previous daemon;
  saved sessions cold-recover on the next launch.

## [0.2.0] - 2026-10-20

### Added

- Per-pane header controls for split right (`V`), split down (`H`), minimize
  (`_`), maximize/restore (`^`/`v`) and close (`X`), with identical mouse hit
  targets and keyboard actions.
- A persistent draggable scrollbar for each pane, including click-to-jump,
  drag-to-scroll and wheel support even when the focused application captures
  the mouse.
- A `+` new-tab button and a clickable minimized-pane tray.
- `Ctrl+C` copy, `Ctrl+V` paste, `Ctrl+Insert` copy and `Shift+Insert` paste
  shortcuts alongside the existing configurable bindings.

### Changed

- `fact-dark` is now a true black theme; active tabs and focused pane headers no
  longer use the light-blue accent.
- Pane and tab titles discard Windows executable paths and show friendly names
  such as `PowerShell` and `Command Prompt`. The pid, scroll-offset and recovered
  text markers were removed from pane headers; a bell now colors the pane and tab
  title amber instead of adding text.
- The status bar is minimal by default: it shows the session name and transient
  messages only. The prefix badge appears only while the prefix is armed; pane
  counts, tab counts, focused-mode text, theme name and clock were removed.
- Layout reserves the rightmost column of every pane for the scrollbar so output
  is never hidden underneath it.
- The wire protocol is now version 2. An old daemon must be stopped before a
  v0.2.0 client attaches; the installer does this and saved sessions cold-recover
  on the next launch.

### Fixed

- The terminal cursor is rendered as a steady cell overlay instead of a
  repositioned hardware cursor. This removes cursor blinking/flicker while
  fast-repainting applications such as AI agents update the screen.
- Pane-header controls remain visible while a pane is maximized, and the
  maximize button becomes a restore button. Clicking any header control focuses
  the target pane first, so controls cannot act on the wrong pane.
- Stale scrollbar glyphs are cleared when a pane leaves scrollback or changes
  layout.
- The active tab remains visible and clickable when the tab strip overflows, and
  long tab titles are capped so one title cannot hide the rest.

## [0.1.0] - 2026-10-20

### Added

- Per-user Windows daemon that owns real ConPTY shells and survives UI detach.
- Independent shells via `fact --grid N`, including a tested grid of eight
  processes with distinct PIDs.
- Live detach/reattach without replacing shell processes.
- Cold recovery that restores validated layout and visual output into new
  shells, reports lost process state and never replays commands.
- Tabs, binary split layouts, focus, zoom, minimize/restore, move and swap.
- Ratatui TUI with Chrome-inspired tabs, compact status bar, command palette,
  pane search, context menus, five themes, F1 help and focused-pane mode.
- Mouse click-to-focus, selection, clipboard copy/paste, wheel forwarding and
  split-gutter resizing.
- Remappable prefix and per-action keybindings, validated against a fixed action
  set.
- CLI surface: `resume`, `sessions`, `attach`, `detach`, `doctor`,
  `config validate|init|path`, `rename`, `save`, `export`, `import`, `archive`,
  `unarchive`, `delete`, `terminate`, `stats` and `shutdown`.
- Offline SQLite session management and JSON export/import with fresh imported
  identity.
- Daemon autostart with authenticated pipe retry, JSON output modes and a
  `doctor` health report.
- Local-only IPC with a protected current-user/SYSTEM DACL, peer user check,
  identification-only impersonation, random daemon token and single-use stream
  tokens.
- SQLite migrations, bounded scrollback, checksummed visual snapshots, size and
  layout validation.
- Real-ConPTY tests for daemon, CLI and TUI behavior; Windows CI on every pull
  request.
- Release workflow gated on format, Clippy and tests, with SHA-256 checksums and
  per-user install/uninstall scripts.

### Fixed

- The installer's release-download path now resolves pinned, unprefixed and
  latest versions correctly. An offline regression test builds a fake release,
  mocks the download, and verifies checksums and installed versions.

### Known limitations

- Windows 11 is the primary verified target. Windows 10 1809+ is intended but not
  verified for this release.
- Windows on ARM and non-Windows platforms are not supported.
- Release artifacts are checksummed but not code-signed.
- The same-user IPC boundary does not protect against malware running as the
  same Windows user.
- Terminal snapshots are visual state, not process resurrection; unsaved work in
  a lost process cannot be recovered.

[0.3.0]: https://github.com/imthegoodboy/FACT/releases/tag/v0.3.0
[0.2.3]: https://github.com/imthegoodboy/FACT/releases/tag/v0.2.3
[0.2.2]: https://github.com/imthegoodboy/FACT/releases/tag/v0.2.2
[0.2.1]: https://github.com/imthegoodboy/FACT/releases/tag/v0.2.1
[0.2.0]: https://github.com/imthegoodboy/FACT/releases/tag/v0.2.0
[0.1.0]: https://github.com/imthegoodboy/FACT/releases/tag/v0.1.0
