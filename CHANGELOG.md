# Changelog

All notable changes to FACT are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and semantic versioning.

## [Unreleased]

### Changed

- `Esc` no longer dismisses the `F1` help overlay; press `F1` again to close
  it, leaving `Esc` for the focused pane rather than FACT help.

## [0.4.0] - 2026-10-21

### Changed

- FACT is now a single process. The per-user daemon, named pipe, authentication
  token, SQLite session database, saved layouts, runtime identity and
  export/import commands are removed. Shells are children of `fact.exe` and close
  when it exits.
- Configuration schema is version 2. Daemon, persistence, snapshot and
  session-limit keys are rejected as unknown fields.
- The CLI is now `fact [--grid N] [--profile NAME] [--cwd DIR] [--config FILE]`
  plus `config path`, `config validate` and `config init`. The `resume`,
  `sessions`, `attach`, `detach`, `doctor`, `save`, `export`, `import`, `archive`,
  `unarchive`, `delete`, `terminate`, `stats` and `shutdown` commands are gone.
- Right-click copies the current selection, or pastes when there is none.
  `Shift`+right-click always opens FACT's pane menu. Applications that requested
  mouse reporting receive a plain right-click as before.
- The installer and uninstaller no longer stop a daemon or remove runtime
  identity files. The uninstaller's `-PurgeConfig` switch removes the optional
  configuration directory; `-PurgeData` remains an alias.

### Added

- Double-click selects the word under the pointer and triple-click selects the
  visual line, matching PowerShell and Windows Terminal conventions.

### Removed

- The `fact-daemon`, `fact-ipc`, `fact-runtime`, `fact-session` and `fact-testkit`
  crates, along with `rusqlite`, `bincode`, `uuid`, `getrandom` and `sha2`.

## [0.3.3] - 2026-10-21

### Changed

- Wire protocol is now version 6. The installer stops the previous daemon;
  saved layouts reopen with fresh shells on the next launch.
- Shell history scrollbars appear only near the right edge on hover and remain
  visible during a drag. Alternate-screen and mouse-controlled applications get
  no FACT scrollbar. Panes use their full width, with protocol 6 preventing
  older peers from clipping the last column. Wheel and keyboard scrolling remain.

### Fixed

- Context-menu actions now respond to mouse clicks, clicks outside dismiss the
  menu, and the wheel navigates menus, help, the palette and session lists.
  Focus loss clears hover state; bell cues no longer overwrite terminal
  content. Search results also accept wheel navigation.
  The menu fits full action labels and configured shortcuts when space allows.
- Exit notices wait for all process output to drain, preserving the final
  result even when a busy daemon has several ticks of output queued.
- Pastes up to the advertised 4 MiB limit are queued as one ordered operation,
  including bracket markers, without increasing the input queue's byte budget.
  Pasting returns a history viewport to live input even without new output.

## [0.3.2] - 2026-10-21

### Fixed

- Wheel input in keyboard-only full-screen apps now sends PageUp/PageDown rather
  than arrow keys, which agent CLIs commonly reserve for prompt history.
- Mouse clicks, releases and drags now work in all three tracking modes (1000,
  1002 and 1003). Reports honor the app's SGR or legacy X11 encoding; motion and
  middle/right-button gestures are forwarded too. Releases outside a pane,
  including over the tab strip, finish the original gesture.
- Text selection holds a snapshot of the visible pane while the agent continues
  running, so redraws cannot change the copied text. Esc, typing or scrolling
  resumes the live view. Shift+right-click opens FACT's menu in mouse apps.
- Pointer movement over main-screen scrollback no longer snaps the view to live
  output. History can be selected without sending clicks to the running app.
- Pending input is processed in bounded batches and the ordered frame queue is
  bounded, preventing unchecked frame accumulation during heavy output. App
  scrollbar page bursts use one input request instead of five round trips.
- Added real-ConPTY regressions for wheel routing, mouse gestures and stable
  selection during streaming output, plus encoding and snapshot copy tests.

## [0.3.1] - 2026-10-21

### Fixed

- The pane scrollbar no longer disappears when a full-screen application enters
  the alternate screen. The rail is always visible: idle panes show a one-cell
  thumb at the bottom, full-screen panes show a centred grip with paging caps.
- Full-screen panes now have a usable app-scroll rail. Clicking or dragging the
  rail, using the wheel, or using Shift+wheel pages the application's own
  transcript. Apps that request mouse reporting receive synthetic wheel events;
  keyboard-only apps receive alternate-scroll arrow or page keys.
- Prefix then `[` now enters app scroll for a full-screen pane instead of
  reporting that there is no scrollback. PgUp/PgDn, Up/Down and Home/End are
  forwarded to the application, and Esc returns to normal input.
- Corrected the v0.3.0 claim that Shift+wheel could browse an alternate-screen
  app's history. Terminal scrollback still works for shells and main-screen
  apps; full-screen apps now page their own viewport.
- Added inline-mode guidance for agent CLIs that need true FACT history:
  `codex --no-alt-screen`, `/tui default` in Claude Code, and
  `OTUI_USE_ALTERNATE_SCREEN=false` for OpenCode.

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
  markers and all ANSI snapshot capture, validation, storage and replay.
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
- Resuming saved sessions now respects `max_sessions` instead of bypassing the
  live-session limit.
- Relative pane working directories are normalized to absolute paths when the
  shell starts, so a daemon restart reopens the same directory.
- Bracketed paste is ignored while scroll mode is active and cannot leak text
  into the shell behind the scrollback view.

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
- Live detach and reattach keep the original processes and scrollback. A daemon
  restart reopens the saved layout with fresh shells and no replay of old output.
  Unsaved work in a lost process cannot be recovered.

[Unreleased]: https://github.com/imthegoodboy/FACT/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/imthegoodboy/FACT/releases/tag/v0.4.0
[0.3.3]: https://github.com/imthegoodboy/FACT/releases/tag/v0.3.3
[0.3.2]: https://github.com/imthegoodboy/FACT/releases/tag/v0.3.2
[0.3.1]: https://github.com/imthegoodboy/FACT/releases/tag/v0.3.1
[0.3.0]: https://github.com/imthegoodboy/FACT/releases/tag/v0.3.0
[0.2.3]: https://github.com/imthegoodboy/FACT/releases/tag/v0.2.3
[0.2.2]: https://github.com/imthegoodboy/FACT/releases/tag/v0.2.2
[0.2.1]: https://github.com/imthegoodboy/FACT/releases/tag/v0.2.1
[0.2.0]: https://github.com/imthegoodboy/FACT/releases/tag/v0.2.0
[0.1.0]: https://github.com/imthegoodboy/FACT/releases/tag/v0.1.0
