# Architecture

FACT separates terminal ownership from terminal display. A per-user daemon owns
the shells; the UI is a replaceable viewer/controller. This is what makes
detach and live reattach possible.

## Process model

```text
+--------------------------+         +------------------------------+
| fact.exe                 |         | fact.exe --daemon            |
| CLI + Ratatui client     |<------->| per-user daemon              |
|                          |  local  |                              |
| - terminal rendering     |  pipe   | - ConPTY processes           |
| - key/mouse encoding     |         | - terminal models            |
| - selection/clipboard    |         | - layout tree                |
| - overlays               |         | - frame fan-out              |
+--------------------------+         | - SQLite persistence         |
                                     +---------------+--------------+
                                                     |
                                       +-------------+-------------+
                                       |             |             |
                                    pwsh.exe      cmd.exe      profile
```

The daemon is not a Windows service and does not run elevated. The first CLI
that needs a daemon spawns the same executable with the hidden `daemon`
subcommand, detached from the console, then waits for the authenticated pipe.

The daemon has one owner thread. A bounded event channel carries commands,
subscriptions and tick notifications. The owner thread mutates all session
state, reads PTY output, writes SQLite and builds frames. Background threads only
accept pipe connections, authenticate clients, wait for command replies and
write event frames to their pipe connection.

## Crate boundaries

| Crate | Responsibility |
| --- | --- |
| `fact-protocol` | Wire structs, commands, frames, validation, session names, cell flags. |
| `fact-config` | TOML config, profiles, chord parsing/compilation, paths and shell detection. |
| `fact-layout` | Serializable binary split tree, grids, focus, zoom, minimize, move/swap. |
| `fact-term` | Alacritty-backed terminal model, bounded scrollback, search, ANSI snapshots. |
| `fact-pty` | ConPTY creation, process/job-object ownership, input, resize and exit codes. |
| `fact-ipc` | Local named-pipe transport, SDDL/DACL creation, frame framing, peer checks. |
| `fact-runtime` | Identity files, daemon discovery, authenticated client and event streams. |
| `fact-session` | SQLite schema/migrations, snapshot checksums, export/import and lifecycle. |
| `fact-daemon` | Session manager, frame generation, persistence loop, cold recovery. |
| `fact-client` | Ratatui renderer, keymap, themes, selection, mouse handling and overlays. |
| `fact-cli` | Command surface, autostart, doctor, output formatting and mode dispatch. |
| `fact-testkit` | Shared helpers for tests that need real Windows resources. |

Dependencies point downward. `fact-client` and `fact-cli` do not parse terminal
escape sequences; they only render `CellDto` values supplied by the daemon.

## IPC and authentication

Every data directory has a stable pipe name derived from the canonical data
directory path. The daemon creates the first pipe instance with:

- `PIPE_REJECT_REMOTE_CLIENTS`
- a protected DACL for the current user and SYSTEM
- `FILE_FLAG_FIRST_PIPE_INSTANCE`
- byte mode and a 64 KiB kernel buffer

The daemon accepts a connection, obtains the client PID with
`GetNamedPipeClientProcessId`, checks that the client token's user SID equals the
daemon token's user SID, and only then speaks the protocol. Messages are
length-prefixed bincode frames with a maximum size.

Client and server exchange `Hello` messages carrying a random 256-bit token
from `runtime/daemon.json`. The token file itself is ACL-protected. A control
connection then sends commands with monotonically increasing request IDs and
waits for the matching response. An event stream is a second connection that
authenticates, presents a short-lived one-time stream token, and receives
frames until it closes.

No socket, HTTP server or network listener exists.

## Session model

A session has:

- a case-insensitive name and stable ID
- tabs, each with a binary split tree
- pane records with shell profile, working directory, PID and exit code
- one terminal model per pane, including bounded scrollback and modes
- a viewport size (columns and rows)
- layout and metadata epochs plus detach generation
- optional cold-recovery timestamp and notes

The binary tree stores normalized split ratios. Layout adds a one-cell gutter at
each split and reserves the rightmost column of each pane for its scrollbar. A
pane has no title row: terminal content uses the full slot height. The daemon
subtracts one row from the client viewport only when the session has more than
one tab; a single-tab session has no persistent chrome at all. When slots fall
below `min_pane_width` or `min_pane_height`, the layout switches to focused-pane
mode instead of rendering unusably small terminals. The explicit show-all
action sets a per-tab `fit_all` flag that bypasses those minimums; restoring a
zoomed pane turns it on so a resize can never trap the UI in focused-pane mode.
Minimized panes keep their processes and terminal state; zoom shows only the
focused pane. Every pane is started with `FACT_SESSION` and `FACT_PANE` in its
environment, which lets the CLI reject a nested TUI launch instead of attaching
to the session that owns the current terminal.

## Frame protocol

The daemon ticks every 33 ms. For each subscriber it builds a frame only when
something relevant changed. A frame carries:

- session summary, tabs, active tab, focus and minimized panes
- pane slots, content dimensions, PIDs, alive/exit state and cursor/modes
- split handles with tree paths, regions and ratios
- only the cell grids whose pane version is newer than what that subscriber was
  sent; layout changes force a full pane refresh
- detach generation and server time

Subscribers use bounded channels. A slow client drops frames rather than
blocking PTY processing. A pane version increments only when its terminal cells
change, so idle panes cost a small header per frame. The client keeps the last
cell buffer per pane and keeps rendering when `cells` is absent.

## Terminal and snapshots

`fact-term` wraps `alacritty_terminal` and owns the grid, scrollback, modes,
cursor, title callbacks and bell state. Snapshots:

- are ANSI text rendered from the visible screen plus relevant style state
- omit trailing blank screen rows so restored output stays visible
- have a maximum size and reject NUL or malformed content
- are stored with a SHA-256 checksum and the source dimensions
- are validated for pane IDs, layout consistency and size on load

`fact-term` also implements capped case-sensitive/insensitive search across
history and visible rows. The TUI sends the query to the daemon, so searching
does not require transferring the whole scrollback to the client.

## Persistence and recovery

`fact-session` uses bundled SQLite with schema migrations. Saves are
transactional. Session data records the schema/export version, timestamps,
layout, panes and validated snapshots.

Persistence cadence:

- layout and metadata are saved on mutations and periodically
- terminal snapshots are captured on a separate interval
- `fact save` forces an immediate snapshot
- normal daemon shutdown snapshots every live session before exiting

Warm resume:

1. The CLI authenticates and sends `ResumeSession`.
2. The daemon finds a live session and returns `Attached` with
   `cold_recovered = false`.
3. The client subscribes to the event stream and receives current pane state.
4. The original PID is preserved.

Cold recovery:

1. The session exists in SQLite but no live session exists in this daemon.
2. The store validates identity, layout and every snapshot checksum.
3. The daemon remaps pane IDs, creates fresh terminal models at saved
   dimensions, replays validated ANSI snapshots and adds an explicit cold banner.
4. New shells are spawned. PIDs and exit codes in the new pane records are reset.
5. `Attached` reports `cold_recovered = true` and recovery notes. The TUI shows a
   modal; the CLI-created session is marked with a recovery timestamp.
6. Commands are never read from saved output and never replayed.

If snapshot replay fails, the pane still starts, the error is added to recovery
notes, and the session remains usable. If the layout or checksum is corrupt, the
session load fails rather than silently executing unvalidated data.

## Detach and reattach

`fact detach` asks the daemon to increment every session's detach generation and
push a frame. Clients exit when the generation changes. The daemon does not stop
shells. `fact resume` or `fact attach` opens a new control connection and event
stream to the same live session. Shells, PIDs, terminal scrollback and layout are
unchanged.

Closing the TUI without detaching also leaves the daemon running. Quitting the
daemon (`fact shutdown`) snapshots sessions and stops the shells; the next
resume is cold.

## Input and mouse routing

The client converts crossterm key events to the chord format used by config and
keybindings. A non-prefix chord that is not a FACT binding is encoded to terminal
input according to the focused pane's modes (application cursor, bracketed
paste, mouse reporting). The prefix chord arms a one-shot command state. F1,
Ctrl+Shift+C/V and other direct bindings work without the prefix.

Mouse events are routed by zone:

- the one-line tab strip (only present with two or more tabs) selects tabs and
  its trailing `+` opens a new tab
- the minimized-pane tray (only present when panes are minimized) restores them
- split gutters drag to resize ratios
- pane content is forwarded as SGR mouse input when the application requested
  mouse reporting; otherwise it starts a text selection
- right-click opens the pane context menu when the application has not requested
  mouse reporting; otherwise it is forwarded as SGR mouse input
- Shift+drag always selects text, even when the application owns the mouse

Selections are extracted from the client's last pane buffers, skipping
double-width spacer cells and masking hidden cells.

## Limits and failure behavior

The daemon enforces limits from configuration and protocol constants: maximum
sessions, panes per tab/session, layout depth, split ratio range, scrollback,
snapshot bytes, frame cells, paste bytes, search query length and IPC frame size.
Protocol versions are negotiated exactly. Invalid names, tokens, cursors,
dimensions and malformed frames are rejected.

If the daemon dies, the CLI reports that it is not running and can start a new
one. If an event stream drops, the UI reports the disconnect and exits safely
instead of continuing to render stale state as live. If the terminal setup fails,
raw mode/alternate screen are restored on the error path.

## Why not tmux, Windows Terminal or a service?

- tmux does not own native Windows ConPTY processes.
- Windows Terminal owns its own UI and session model; FACT needs a custom TUI
  and a stable IPC protocol.
- A Windows service would require elevation or service ACL configuration and
  would complicate per-user sessions. A per-user daemon matches the security
  boundary and the data directory.

See [docs/adr](adr/README.md) for the recorded trade-offs.
