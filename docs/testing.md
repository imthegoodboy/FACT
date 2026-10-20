# Testing and verification

FACT treats Windows behavior as the product. Unit tests cover parsers and data
structures; integration tests launch real ConPTY shells and drive the daemon, CLI
and TUI through the same binary a user runs.

## Local gates

Run all three before merging or tagging:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

Tests create temporary data directories. They do not use the developer's real
`%LOCALAPPDATA%\FACT` state. The daemon tests start a daemon on a per-test data
directory, use a real shell and terminate the daemon during cleanup.

## What is covered

| Area | Evidence |
| --- | --- |
| Protocol framing | Round-trip, malformed bincode, zero/oversize frame rejection, name and cell validation. |
| Configuration | Defaults, unknown-field rejection, version mismatch, environment expansion, profiles, duplicate/invalid keybindings, theme and range checks. |
| Layout | Grid construction, split/close/focus, zoom, minimize/restore, move/swap, ratio clamping and serialization. |
| Terminal | ANSI colors/styles, wide and zero-width cells, alternate screen, cursor/modes, bounded scrollback, search, snapshot checksum and blank-row trimming. |
| ConPTY | Independent processes, input/output, resize, exit status, environment block and argument quoting. |
| IPC | Pipe round-trip, peer PID/current-user check, protected DACL, local name validation, identity file ACL, DACL diagnostics. |
| Sessions | Schema migration, save/load, case-insensitive names, checksum and layout corruption rejection, export/import copy semantics, archive/delete. |
| Daemon | Grid of eight independent shells, live detach/reattach PID preservation, cold recovery, split/tab/focus/search, direct scrollback offsets, lifecycle, bad-token rejection. |
| CLI | Help surface, config init/validate, offline session lifecycle, export/import, doctor, JSON output. |
| Installer | `scripts/test-install.ps1` mocks release downloads and exercises pinned, unprefixed and latest versions, checking the URL/tag, SHA-256 verification and installed `--version`. |
| TUI | A driven real-ConPTY `--grid 8` session covering pane output, chrome-free rendering, scrollbar rendering, keyboard input, palette, search, F1 help, prefix detach and eight live panes afterwards. A second test leaves a one-pane session behind and proves `--grid 8` starts `grid-8` instead of resuming it. A third shuts the daemon down, resumes cold, checks the recovery modal and proves a new shell PID accepts input. |
| Security invariant | Source scan rejects network listener/stream APIs; DACL tests assert only the current user and SYSTEM are granted. |

## Requirement-to-test map

| Requirement | Test |
| --- | --- |
| `fact --grid 8` creates independent shells | `fact-daemon` integration test `grid_launches_independent_shells_and_streams_output` checks eight distinct PIDs and output routing. `fact-cli` test `grid_flag_starts_a_new_grid_instead_of_resuming_the_latest_session` proves a prior saved/live session cannot swallow the grid request. |
| Live detach/reattach preserves processes | `detach_and_reattach_keep_the_same_processes` checks the original pane ID, PID and alive state after reattach. |
| Restart cold-recovers layout/output only | `cold_recovery_restores_output_with_new_shells` checks `cold_recovered`, new PIDs, restored scrollback output and absence of command replay. The TUI cold-recovery test repeats the restart through the real UI and verifies a new shell accepts input. |
| Snapshot integrity | `fact-session` tests `rejects_checksum_mismatch` and `rejects_pane_layout_mismatch`; `fact-term` validates snapshot size/shape before replay. |
| Bad token rejection | `wrong_token_is_rejected_without_killing_the_daemon` verifies the daemon rejects authentication and remains available. |
| Non-owner access | Pipe and identity files use a protected current-user/SYSTEM DACL. Tests assert exactly those grants, peer-user comparison is in the accept path, and local-only pipe names are enforced. |
| No network listeners | A source-level test scans every crate's `src` tree for TCP/UDP/HTTP listener and stream APIs. |
| Import copies identity | `export_import_is_a_copy` and the daemon lifecycle test verify fresh session/pane IDs and reset process state. |
| Clean, controllable UI | Layout tests prove 8 panes retain full content height and fit 120x35, 100x30 and 80x24. Client unit tests cover tab/new-tab geometry, the bottom-right minimized tray, scrollbar geometry and steady cursor styles. The real-ConPTY TUI test renders the screen and rejects pane-header buttons, shell paths, PIDs, pane counts, focused-mode labels, session names and theme names. |
| Windows CI on every PR | `.github/workflows/ci.yml` runs format, Clippy and the full suite on `windows-latest` for every pull request. |

A true second-Windows-user connection test requires creating a local account or
credentials, which CI does not provide. The ACL assertions, peer SID comparison
and server-side rejection are tested directly instead.

## Real-ConPTY tests

These are intentionally not mock tests:

- Daemon tests use the testkit harness to write an isolated config, start the
  daemon executable, connect over the real named pipe and run `powershell.exe` or
  the detected shell.
- The TUI test allocates a 120x35 ConPTY, launches `fact --grid 8`, waits for a
  real PowerShell prompt before typing (early input can race the first frame),
  opens the palette, searches for an echoed marker, opens help, detaches with the
  configured prefix and verifies through `fact sessions --json` that the session
  is still live with eight panes. Another test first detaches a one-pane session,
  then proves a bare `fact --grid 8` creates `grid-8` instead of attaching to it.
  A third snapshots, stops the daemon, resumes cold, dismisses the recovery modal
  and verifies a new shell PID. Set `FACT_SMOKE_EXE` to run the three tests
  against a downloaded release binary instead of the test build. Restored visual content and no-command-replay behavior are
  asserted directly by the daemon cold-recovery test.
- Tests wait for frames and output with deadlines rather than fixed sleeps where
  possible. Cold-recovery assertions search the terminal buffer for restored
  output because a new shell's banner can scroll the restored screen.

Run the focused integration suites with:

```powershell
cargo test -p fact-daemon --test daemon -- --nocapture
cargo test -p fact-cli --test cli -- --nocapture
cargo test -p fact-cli --test tui -- --nocapture
```

## CI

`.github/workflows/ci.yml` runs on every push to `main` and every pull request:

1. checkout
2. stable Rust with rustfmt and Clippy
3. cargo cache
4. `cargo fmt --all -- --check`
5. `cargo clippy --workspace --all-targets -- -D warnings`
6. `cargo test --workspace --no-fail-fast`
7. `scripts/test-install.ps1` against the test binary, with mocked downloads

The release workflow repeats the same gates before it can build and publish an
artifact. A tag cannot bypass a failing test.

## Windows versions

- Development and manual verification: Windows 11.
- CI: GitHub's current Windows Server runner, which is not the same as Windows
  11 or Windows 10.
- Windows 10 1809+: intended, but not verified for v0.2.1. Do not claim support
  until a run on a physical or VM Windows 10 installation passes the suite.

## Manual smoke check

Before a release, on a clean Windows 11 account:

```powershell
fact doctor
fact --grid 8
# confirm there is no title row, button strip or status bar around any pane
# in each pane: hostname; $PID; echo $env:TERM_PROGRAM
# confirm eight panes are visible, not focused-pane mode
# right-click a pane or press Ctrl+Space then | and - to split; confirm input
# press Ctrl+Space C; confirm a one-line tab strip appears and + is clickable
# minimize a pane, then click the bottom-right tray to restore it
# maximize a pane with Ctrl+Space Z and restore it with Ctrl+Space Z
# generate 200 lines, then click and drag the scrollbar; confirm output moves
# select text, press Ctrl+C, focus another pane and press Ctrl+V
# press Ctrl+Space and confirm the two-cell accent marker appears only while armed
# detach with Ctrl+Space D
fact sessions --json
fact --grid 8
# verify this reattaches grid-8, not whichever session was used most recently
fact resume
# verify pane PIDs are unchanged by printing $PID again

fact shutdown
fact resume
# verify the cold-recovery modal and restored output
# confirm the old PIDs are reported as lost and no command was replayed

fact export <session> --output .\exports
fact import .\exports\<file>.json --name imported
fact config validate --json
```

Record the result in the release checklist. Screenshots are not a substitute for
the checks above.
