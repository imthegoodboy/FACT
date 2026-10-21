# Testing and verification

FACT treats Windows behavior as the product. Unit tests cover parsers, layout,
configuration and terminal state. Integration tests start real ConPTY shells and
drive the in-process engine and the CLI through the binaries a user runs. There
are no mock shells and no daemon to test.

## Local gates

Run all four before merging or tagging:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
.\scripts\test-install.ps1 -FactExe .\target\debug\fact.exe
```

The CLI tests point `FACT_HOME` at a throwaway directory, so a developer's real
`%LOCALAPPDATA%\FACT\config.toml` is never read or written. The engine and PTY
tests start their own shells and do not read the developer's configuration. A
normal test run creates no database, log, saved layout or runtime identity because
FACT has no such state. The ConPTY tests terminate their shells during cleanup.

## What is covered

| Area | Evidence |
| --- | --- |
| Protocol values | `fact-protocol` tests cover title sanitization, friendly shell titles, preserved zero-width symbols and rectangle hit testing. Frames are in-memory Rust values; serde is used for tests and diagnostics, not for a wire or disk format. |
| Configuration | Defaults, version mismatch, bad values, unknown and obsolete fields, environment/profile resolution, round trips and disabled profiles; chord parsing, uppercase/Shift handling, invalid chords, unique defaults and duplicate overrides. |
| Layout | Grid construction, full-width content, focused-pane fallback at tiny sizes, zoom, show-all, serialization, split/close symmetry, minimize/restore, focus cycling, ratio clamping, move/swap and moving panes between tabs. |
| Terminal model | Plain text and cursor, ANSI colors and bold, wide/combining characters, alternate screen and modes, scrollback and search, tiny resizes, malformed escapes, title callbacks and mouse mode bits. |
| ConPTY | Real `cmd.exe` processes: output and exit, propagated exit codes, interactive input, resize, eight independent shells, more than 100 KB of output without truncation, kill cleanup and a no-deadlock shutdown regression with unread output. |
| Engine | A real grid of two shells is resized, receives typed input, returns output through the ordered frame stream and shuts down with its shells. |
| Client | Selection extraction and snapshot copy, wide-spacer handling, overlapping mouse buttons, direct Alt shortcuts, action labels, word selection, tab/new-tab geometry, the minimized tray, hover-only scrollbar hit testing, app scroll routing and theme/render helpers. |
| CLI | Help surface (including the absence of removed session commands), `FACT_HOME` path resolution, `config validate` with built-in defaults and JSON, `config init` and overwrite refusal, invalid-prefix diagnostics and rejection of obsolete daemon settings. |
| Installer | `scripts/test-install.ps1` mocks release downloads and exercises pinned, unprefixed and latest versions, checking the URL/tag, SHA-256 verification and the installed `--version`. |
| Windows CI | `.github/workflows/ci.yml` runs format, Clippy, the full suite and the offline installer test on `windows-latest` for every push to `main` and every pull request. |

## Real-ConPTY tests

These are intentionally not mock tests:

- `crates/fact-pty/tests/conpty.rs` starts `cmd.exe` through the Windows
  pseudo-console API and drives it directly. The eight-shell case keeps eight
  distinct processes alive at once, and the backpressure case runs in an isolated
  child process so a deadlocked `ClosePseudoConsole` cannot hang the suite.
- `crates/fact-engine/tests/engine.rs` starts the same engine the TUI uses,
  resizes the workspace to 100x30, types `echo FACT_ENGINE_OK` into a pane and
  waits for the marker in the ordered frame stream. It ends with
  `Command::Shutdown`.
- `crates/fact-cli/tests/cli.rs` launches the compiled `fact` binary with a
  temporary `FACT_HOME`. It exercises argument parsing and configuration
  commands and never enters the interactive TUI.

Helpers wait on frames and output with deadlines instead of fixed sleeps where
possible. The ConPTY layer needs a short settle after a resize because Windows
rebuilds the console around the new dimensions.

Run the focused integration suites with:

```powershell
cargo test -p fact-pty --test conpty -- --nocapture
cargo test -p fact-engine --test engine -- --nocapture
cargo test -p fact-cli --test cli -- --nocapture
```

The TUI itself is verified manually; there is no scripted TUI harness in this
tree. The manual checklist below is the release gate for rendering, mouse and
clipboard behavior.

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
- Windows 10 1809+: intended, but not verified for the current build. Do not
  claim support until a run on a physical or VM Windows 10 installation passes
  the suite.

## Manual smoke check

Before a release, on a clean Windows 11 account, build and install the binary:

```powershell
cargo build --release --bin fact
.\scripts\install.ps1 -BinaryPath .\target\release\fact.exe -Force
```

Then start FACT:

```powershell
fact --version
fact config validate
fact --grid 8
```

In the running window:

1. Confirm eight independent panes, each with its own prompt and no title row,
   button strip or permanent status bar.
2. Print `$PID` in two panes and confirm the values differ.
3. Split with prefix `|` and `-`, move focus with the arrow keys, and close a
   split with prefix `X`.
4. Press prefix `C`; confirm the one-line tab strip appears and `+` works. Close
   the tab with `Alt+T`, first cancelling and then confirming.
5. Press `Alt+M` and `Alt+N`; confirm maximize and full-grid restore. Minimize a
   pane with prefix `Shift+M` and restore it from the `▾ N` tray.
6. Drag to select text; double-click a word; triple-click a line. Press `Ctrl+C`
   and paste into another pane with `Ctrl+V`.
7. Right-click with a selection (copy), then right-click with no selection
   (paste). `Shift`+right-click opens the pane menu in both cases.
8. Start a mouse-aware program, confirm wheel and click reports reach it, then
   hold `Shift` and drag to select text.
9. Generate 200 lines, hover the right edge and click/drag the scrollbar. Confirm
   it hides when the pointer leaves. Press prefix `[` and scroll with arrows,
   PgUp/PgDn and Home/End; press Esc.
10. Start a full-screen agent (Codex, Claude Code or OpenCode). Confirm FACT adds
    no scrollbar, wheel and `Shift`+wheel page the app, and prefix `[` reports
    app scroll.
11. Press `F1`; confirm help lists the compiled bindings. Press prefix `K` and
    filter the palette. Press prefix `/` and search a marker in the focused pane.
12. Press `F2`; confirm the theme changes for this run and is not written to disk.
13. Note a pane PID, quit with prefix `Shift+Q`, and confirm that process no
    longer exists.
14. Before launching, record `Get-ChildItem $env:LOCALAPPDATA\FACT -Force`. After
    launching, using and quitting FACT, confirm the listing is unchanged. A
    normal launch writes nothing.

Record the result in the release checklist. Screenshots are not a substitute for
the checks above.
