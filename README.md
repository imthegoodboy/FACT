# FACT

FACT is a Windows-native terminal multiplexer written in Rust. It runs real
ConPTY shells under a per-user background daemon, keeps them alive when the UI
detaches, and restores sessions after a restart.

> **Status: core in progress.** The low-level engine is implemented and tested;
> the daemon, CLI and TUI are landing in follow-up changes. Nothing is released
> yet.

## What exists today

| Area | Status |
| --- | --- |
| Real ConPTY panes (input, output, resize, exit codes, job-object cleanup) | Implemented, 7 integration tests |
| Authenticated local named-pipe IPC (per-user DACL, token handshake) | Implemented, 6 tests |
| Binary split-tree layout (grids, focus, zoom, minimize, tiny-terminal mode) | Implemented, 11 tests |
| Terminal emulation + bounded snapshots + search | Implemented, 11 tests |
| SQLite sessions, validation, export/import, archive/delete | Implemented, 8 tests |
| Config, profiles, keybindings, path resolution | Implemented, 11 tests |
| Daemon, CLI commands, Ratatui client | In progress |

## Requirements

- Windows 10 1809 or newer (Windows 11 is the primary target)
- A recent stable Rust toolchain (MSVC target)

## Build and test

```powershell
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The ConPTY integration suite launches real shells and exercises eight
concurrent panes, resize, input, large output, exit codes and cleanup.

## Design

```
fact (CLI + Ratatui client)
      │  authenticated named pipe, local only
      ▼
fact-daemon (per-user background process)
      │  owns ConPTY handles + terminal models
      ▼
Windows ConPTY ── powershell.exe / cmd.exe / profile command
```

- **Sessions survive detach** because the daemon, not the UI, owns the shells.
- **Cold recovery** replays a validated ANSI snapshot into a new terminal
  model and starts new shells. Lost process state is called out explicitly;
  commands are never replayed automatically.
- **Security**: named pipes are local-only with a DACL restricted to the
  current user and SYSTEM. Clients verify the server token before sending
  anything. There are no network listeners.
- **Storage**: `%LOCALAPPDATA%\FACT` by default (`FACT_HOME`, `--data-dir` and
  `--config` override it), with SQLite sessions, bounded scrollback and
  SHA-256-checked snapshots.

## Repository layout

| Crate | Purpose |
| --- | --- |
| `fact-protocol` | Wire types, session names, snapshots, frames |
| `fact-config` | TOML config, profiles, key chords, Windows paths |
| `fact-layout` | Split-tree tiling and focus management |
| `fact-term` | Alacritty-backed terminal model and recovery snapshots |
| `fact-pty` | Real Windows ConPTY process management |
| `fact-ipc` | Authenticated local named-pipe transport |
| `fact-session` | SQLite persistence and session import/export |
| `fact-daemon` | Per-user background daemon |
| `fact-client` | Ratatui user interface |
| `fact-cli` | `fact` command-line entry point |
| `fact-testkit` | Integration-test harness |

## License

MIT. See [LICENSE](LICENSE).
