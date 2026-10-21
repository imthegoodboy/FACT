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
| Terminal | ANSI colors/styles, wide and zero-width cells, alternate screen, cursor/modes, bounded scrollback, search and frame snapshots. |
| ConPTY | Independent processes, input/output, resize, exit status, environment block and argument quoting. |
| IPC | Pipe round-trip, peer PID/current-user check, protected DACL, local name validation, identity file ACL, DACL diagnostics. |
| Sessions | Schema migration (including secure v1 snapshot cleanup and WAL truncation), save/load, case-insensitive names, layout corruption rejection, export/import copy semantics, archive/delete. |
| Daemon | Maximum-size bracketed UTF-8 paste with live-view reset, queued-output/exit ordering, grid of eight independent shells, live detach/reattach PID preservation, saved-layout restart with fresh shells and discarded output, split/tab/focus/search, direct scrollback offsets, lifecycle, bad-token rejection. |
| CLI | Help surface, config init/validate, offline session lifecycle, export/import, doctor, JSON output. |
| Installer | `scripts/test-install.ps1` mocks release downloads and exercises pinned, unprefixed and latest versions, checking the URL/tag, SHA-256 verification and installed `--version`. A replacement install also proves a dead daemon identity is cleared. |
| TUI | A driven real-ConPTY `--grid 8` session covering pane output, chrome-free rendering, hover scrollbar appearance and disappearance, keyboard input, palette, search, F1 help, prefix detach and eight live panes afterwards. A second test leaves a one-pane session behind and proves `--grid 8` starts `grid-8` instead of resuming it. A third verifies a second client sees the same live grid. A fourth shrinks to 60x18 and proves maximize/show-all return to the full grid. A fifth proves a nested `fact` launch inside a pane is rejected. A sixth shuts the daemon down, resumes the saved layout, proves the old marker and recovery screen are absent, and verifies a new shell PID accepts input. A seventh writes a stale protocol-3 identity and proves startup clears it and renders a live pane. An eighth generates 120 lines, enters scroll mode with the configured prefix and `[`, verifies the offset moves on PageUp and that Esc returns to the shell. A ninth starts a fake alternate-screen app, proves no FACT rail appears even on hover, enters app scroll mode, verifies PageUp is forwarded to the app and that Esc leaves the mode. Further scenarios exercise streaming mouse gestures/selection, history motion suppression and clickable/clamped context menus. The documentation capture is opt-in and ignored by default. |
| Security invariant | Source scan rejects network listener/stream APIs; DACL tests assert only the current user and SYSTEM are granted. |

## Requirement-to-test map

The streaming regression `mouse_tracking_and_selection_work_while_an_app_is_streaming`
starts `crates/fact-cli/tests/fixtures/mouse-app.ps1` as a real child behind two ConPTY layers.
It asserts wheel, click, drag and release reports with mode 1002, selects text
while a timer redraws the screen, checks the selected viewport stays stable,
and verifies Esc resumes live output. The keyboard-only alternate-screen test
also sends a real wheel event and requires PageUp instead of UpArrow. Client
unit tests cover SGR/legacy encodings, coordinate limits and copying from a
snapshot after the live cells change.
`mouse_motion_does_not_snap_scrollback_to_live_output` uses the same fixture in
main-screen mode with all-motion tracking. It checks Shift+wheel, pointer motion
over history and selection of history without passing clicks to the app.
The TUI scenarios share a test lock: each starts real console processes, and
running all the eight-pane grids alongside the streaming fixtures can exhaust
local Windows console/startup resources. Other crate tests retain their normal
parallel execution.

| Requirement | Test |
| --- | --- |
| `fact --grid 8` creates independent shells | `fact-daemon` integration test `grid_launches_independent_shells_and_streams_output` checks eight distinct PIDs and output routing. `fact-cli` test `grid_flag_starts_a_new_grid_instead_of_resuming_the_latest_session` proves a prior saved/live session cannot swallow the grid request. |
| Live detach/reattach preserves processes | `detach_and_reattach_keep_the_same_processes` checks the original pane ID, PID and alive state after reattach. |
| Restart reopens layout with fresh shells | `saved_layout_starts_fresh_shells_without_replaying_output` checks the saved pane count, new PIDs, an empty search for the old marker and a new shell that accepts input. The TUI test repeats the restart through the real UI and rejects both the old marker and the removed recovery screen. |
| Layout integrity | `fact-session` tests `rejects_pane_layout_mismatch` and validates pane IDs before a saved layout can start processes. The v1 migration test clears legacy snapshots, asserts no SQLite free pages remain and scans the database and WAL sidecars for the old snapshot marker. |
| Bad token rejection | `wrong_token_is_rejected_without_killing_the_daemon` verifies the daemon rejects authentication and remains available. |
| Non-owner access | Pipe and identity files use a protected current-user/SYSTEM DACL. Tests assert exactly those grants, peer-user comparison is in the accept path, and local-only pipe names are enforced. |
| No network listeners | A source-level test scans every crate's `src` tree for TCP/UDP/HTTP listener and stream APIs. |
| Import copies identity | `export_import_is_a_copy` and the daemon lifecycle test verify fresh session/pane IDs and reset process state. |
| Clean, controllable UI | Layout tests prove 8 panes retain full content height and fit 120x35, 100x30 and 80x24. Client unit tests cover tab/new-tab geometry, the bottom-right minimized tray, hover-only history scrollbar geometry, app rail suppression and steady cursor styles. The real-ConPTY TUI test renders the screen and rejects pane-header buttons, shell paths, PIDs, pane counts, focused-mode labels, session names and theme names. |
| Recover from a narrow window | Layout tests prove `zoom(false)`/`fit_all` keeps all eight panes visible at 60x18 and survives serialization. The real-ConPTY `maximize_and_show_all_work_in_a_small_window` test presses prefix+M/N and counts the rendered panes. |
| Safe nested launch | The real-ConPTY `nested_launch_is_rejected_inside_a_pane` test launches `fact --grid 1` inside a pane and checks for the explicit refusal instead of a black second UI. |
| Upgrade over a stale daemon | The startup unit test removes an unusable protocol-3 identity; the real-ConPTY stale-identity test proves the TUI then starts; the installer replacement test proves a dead identity is cleared. A live incompatible daemon is never overwritten and produces a stop-and-retry error. |
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
  A third saves the layout, stops the daemon, resumes it, proves the old output
  and recovery screen are absent and verifies a new shell PID. Further tests
  cover a second live client, a 60x18 grid with prefix+M/N, nested-launch
  refusal, a stale protocol-3 identity that must not block startup, scroll
  mode reading real history, mouse gestures/selection during streaming,
  context-menu clicks (including clamped menus and outside dismissal), and an
  alternate-screen app with no FACT rail that receives forwarded PageUp. Set
  `FACT_SMOKE_EXE` to run all twelve tests against a downloaded release binary instead of the test build. Fresh
  PIDs and discarded old output are asserted
  directly by the daemon saved-layout test.
- Tests wait for frames and output with deadlines rather than fixed sleeps where
  possible. Restart assertions search the whole pane buffer, not only the visible
  screen, so a new shell's banner cannot mask a replay bug.

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
- Windows 10 1809+: intended, but not verified for v0.3.1. Do not claim support
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
# shrink the window below min_pane_width, then press Alt+N and confirm all panes return
# press Alt+M to maximize the focused pane, then Alt+N to restore the grid
# press Ctrl+Space M/N and confirm the same behavior
# press Alt+T, cancel the confirmation, then confirm and verify the grid closes
# minimize a pane, then click the bottom-right tray to restore it
# maximize a pane with Ctrl+Space Z and restore it with Ctrl+Space Z
# from a pane, run `fact --grid 8` and confirm it refuses instead of going black
# generate 200 lines, hover at the right edge, then click/drag; confirm output moves
# move away and confirm the bar hides; run an agent and confirm no FACT bar appears
# press Ctrl+Space then [, scroll with Up/Down and PgUp/PgDn, and press Esc
# hold Shift and use the wheel over a pane; confirm it scrolls FACT, not the app
# start Codex, Claude Code or OpenCode; confirm no FACT rail appears, wheel and
#   Shift+wheel page the app, and Ctrl+Space then [ reports APP SCROLL
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
# verify the saved grid reopens directly with a fresh shell prompt in every pane
# confirm there is no recovery modal and no old output on screen
# print $PID in a pane and confirm it differs from the pre-shutdown PID

fact export <session> --output .\exports
fact import .\exports\<file>.json --name imported
fact config validate --json
```

Record the result in the release checklist. Screenshots are not a substitute for
the checks above.
