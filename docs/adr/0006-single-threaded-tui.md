# 0006: Single-threaded TUI with a separate event reader

- **Status:** Accepted
- **Date:** 2026-10-19

## Context

Ratatui applications typically have an input/render loop and one or more
producers. FACT has three asynchronous sources: crossterm input, daemon frames
from a blocking named-pipe stream, and local timers/status expiration. The UI
must remain responsive while a shell produces output, and switching sessions must
not leak reader threads or leave stale subscriptions.

A fully async runtime would add a large dependency and complicate Windows pipe
integration. Spawning a thread per frame would create ordering and teardown
problems.

## Decision

Keep the TUI event loop single-threaded:

- crossterm events are polled with a short timeout
- daemon frames arrive through a dedicated event-stream reader thread
- the reader forwards decoded `ServerMessage` values over a standard channel
- an `AtomicBool` cancellation flag plus nonblocking `PeekNamedPipe` checks let
  the UI stop the reader on session switch or exit without waiting forever
- all application state and rendering happen on the main thread
- the client sends a separate control request for each action and applies the
  resulting frame

The daemon also runs a single owner thread and pushes frames; the client never
polls terminal state directly.

## Consequences

- No async runtime and no shared mutable UI state.
- Frame ordering is preserved by the event stream and sequence numbers.
- Slow rendering can delay input, but does not block the daemon; the daemon drops
  frames for a slow subscriber.
- Reader teardown is explicit and tested by the session-switch/TUI smoke tests.
- Frames are the source of truth; optimistic local layout edits could diverge and
  are avoided.

## Alternatives considered

- **Async runtime:** more dependencies and an awkward fit with the blocking Win32
  pipe API.
- **Reader thread per pane:** excessive threads and complex lifetime handling.
- **Polling the daemon from the render loop:** higher latency and unnecessary
  request traffic.
- **Multi-threaded rendering:** state sharing and terminal corruption risks
  without a clear performance need.
