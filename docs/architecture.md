# Architecture

FACT is one Windows process. The terminal user interface and the ConPTY shells
live in the same `fact.exe`; there is no daemon to connect to and no stored
workspace to restore. The design goal is a small, predictable terminal host
where tabs, splits and grids work without a background service or database.

## Process model

```text
+---------------------------------------------------------------+
| fact.exe                                                      |
|                                                               |
|  TUI thread                     Engine owner thread           |
|  - Ratatui rendering            - ConPTY processes            |
|  - keyboard/mouse encoding      - terminal models             |
|  - selection/clipboard          - tabs and split layout       |
|  - overlays                     - frame generation            |
|          |          commands          ^                       |
|          +--------------------------> |                       |
|          |          frames            |                       |
|          <-------------------------- +                       |
|                                                               |
|                    +---------+---------+                      |
|                    |         |         |                      |
|                 pwsh.exe   cmd.exe   profile                  |
+---------------------------------------------------------------+
```

The engine starts before the TUI enters the alternate screen. `Engine::drop`
sends a shutdown request, the owner thread kills each PTY and drains the
workspace, and `drop` joins the thread. Closing the last tab, pressing the quit
shortcut, closing the console window or otherwise ending the process therefore
ends every shell it owns.

## Crate boundaries

| Crate | Responsibility |
| --- | --- |
| `fact-protocol` | In-process command, frame, cell and validation types. |
| `fact-config` | TOML config, profiles, chord parsing/compilation, paths and shell detection. |
| `fact-layout` | In-memory binary split tree, grids, focus, zoom, minimize, move/swap. |
| `fact-term` | Alacritty-backed terminal model, bounded scrollback, search and frame snapshots. |
| `fact-pty` | ConPTY creation, process/job-object ownership, input, resize and exit codes. |
| `fact-engine` | Owner thread, workspace state, shell lifecycle, layout commands and frame generation. |
| `fact-client` | Ratatui renderer, keymap, themes, selection, mouse handling and overlays. |
| `fact-cli` | Argument parsing, configuration commands and engine/TUI startup. |

Dependencies point downward. `fact-client` and `fact-cli` do not parse terminal
escape sequences; they render `CellDto` values supplied by the engine.

## Engine and channels

`fact-engine::start` resolves the requested profile, creates the `FACT`
workspace and spawns its initial grid of shells. It then starts one owner
thread. Every later mutation happens on that thread, so PTY reads, terminal
state, layout changes and frame construction never race each other.

The UI holds an `EngineClient`. A command is sent through an `mpsc` channel with
a one-slot reply channel; the UI waits for the response before continuing. The
engine sends ordered `ServerMessage` values to an `EventStream` through a
bounded eight-message channel. The UI's reader thread moves those messages into
a second bounded channel, so a busy shell cannot grow an unbounded queue while
the renderer is busy. A slow subscriber loses frames rather than blocking the
owner thread.

The owner loop wakes on a command or after a 33 ms tick. On each turn it reads
bounded PTY output, updates terminal models, records exits, handles terminal
responses, advances titles/bells, and produces any needed frames. The first
frame sequence is `1`; the client discards older or duplicate sequences.

## Workspace model

There is exactly one workspace, named `FACT`. It contains:

- tabs, each with a binary split tree
- pane records with shell profile, working directory, PID and exit code
- one terminal model per pane, including bounded scrollback and terminal modes
- a viewport size (columns and rows)
- layout and metadata epochs used to decide when a frame is needed

The binary tree stores normalized split ratios. Layout adds a one-cell gutter at
each split. Terminal content uses the full slot width and height; a shell
history scrollbar overlays the rightmost column only on hover or drag.
Applications in the alternate screen or with mouse reporting get no FACT
scrollbar. When slots fall below `min_pane_width` or `min_pane_height`, the
layout switches to focused-pane mode instead of rendering unusably small
terminals. The explicit show-all action sets a per-tab `fit_all` flag that
bypasses those minimums.

Minimized panes keep their processes and terminal state; zoom shows only the
focused pane. Every pane receives an informational `FACT_PANE` environment
value. Fact does not use it to discover or reattach to anything.

## Frame protocol

The owner thread builds a frame only when one of its subscribers needs new
state. A frame carries:

- workspace summary, tabs, active tab, focus and minimized panes
- pane slots, content dimensions, PIDs, alive/exit state and cursor/modes
- split handles with tree paths, regions and ratios
- only the cell grids whose pane version is newer than what that subscriber was
  sent; layout changes force a full pane refresh
- viewport and tiny-mode information

The client keeps the last cell buffer per pane and keeps rendering when `cells`
is absent. Pane versions increment only when terminal cells change, so an idle
pane costs a small header rather than a full screen. Frames are in-memory Rust
values; `fact-protocol` retains serde derives for tests and diagnostics, not for
a network or disk format.

## Terminal model and scrolling

`fact-term` wraps `alacritty_terminal` and owns the grid, scrollback, modes,
cursor, title callbacks and bell state. Frame snapshots are bounded `CellDto`
grids with cursor and mode metadata; they exist only in memory and are replaced
on every changed frame. Terminal output is never written to disk.

`fact-term` implements capped case-sensitive/insensitive search across history
and visible rows. The UI sends the query to the engine, so searching does not
require transferring the whole scrollback to the client.

When a pane is in the alternate screen, the application owns its viewport and
scrollbar. FACT adds no rail. Ordinary wheel input retains its original
coordinates when mouse reporting is enabled, with PageUp/PageDown fallback
otherwise. Prefix scroll mode targets its own pane and synthesizes wheel input
at that pane's centre; paging and navigation keys are still forwarded. Ordinary
shell history uses a hover-only scrollbar overlay, without reducing terminal
width.

## Input and mouse routing

The client converts crossterm key events to the chord format used by config and
keybindings. A non-prefix chord that is not a FACT binding is encoded to terminal
input according to the focused pane's modes (application cursor, bracketed
paste, mouse reporting). The prefix chord arms a one-shot command state. F1,
Ctrl+Shift+C/V, clipboard aliases and the direct Alt shortcuts work without the
prefix.

Mouse events are routed by zone:

- the one-line tab strip (only present with two or more tabs) selects tabs and
  its trailing `+` opens a new tab
- the minimized-pane tray (only present when panes are minimized) restores them
- split gutters drag to resize ratios
- pane content is forwarded as SGR mouse input when the application requested
  mouse reporting; otherwise it starts a text selection
- drag selects; double-click selects a word; triple-click selects a visual line
- Shift+drag always selects text, even when the application owns the mouse
- right-click copies a selection; with no selection it pastes, or is forwarded
  when the application requested mouse reporting
- Shift+right-click always opens FACT's context menu

Selections are extracted from the client's last pane buffers, skipping
double-width spacer cells and masking hidden cells. A selection snapshots the
visible pane so a streaming process cannot change the selected text; Esc,
typing or scrolling resumes the live view.

## Limits and failure behavior

FACT enforces limits from configuration and protocol constants: 32 panes in an
initial grid, 256 panes per workspace, split ratio range 0.1-0.9, tree depth 64,
scrollback, frame cell buffers, 16 MiB queued input, 4 MiB paste, a 256 KiB
single input chunk and a 256-character search query. The ConPTY layer bounds
output and input queues.

If a shell exits, its pane remains visible with the exit code and a restart
hint; the process that owns the pane is cleaned up. If the last pane or tab
closes, the engine ends the workspace and the UI exits. If the engine channel
closes, the UI reports the failure and restores the console instead of rendering
stale state. If terminal setup fails or the UI returns an error, raw mode and the
alternate screen are restored on the error path.

## Why not a daemon or Windows service?

A daemon adds a second process, an authenticated transport, lifecycle commands,
persistence, crash recovery and a whole class of stale-state bugs. It buys live
detach and survival after the UI exits, neither of which is a requirement here.
The current product is a terminal window: when the window closes, its shells
should close too. A Windows service would additionally require elevation or
service ACL configuration and would not match the per-user, no-persistence
model. See [docs/adr](adr/README.md) for the recorded trade-offs.
