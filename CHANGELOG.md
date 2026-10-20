# Changelog

All notable changes to FACT are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and semantic versioning.

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

[0.1.0]: https://github.com/imthegoodboy/FACT/releases/tag/v0.1.0
